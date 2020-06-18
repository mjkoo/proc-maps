use std;
use std::io;
use std::mem;
use failure::Error;
use libc::{c_int, pid_t, strlen};
use mach::kern_return::{KERN_SUCCESS, kern_return_t};
use mach::port::{mach_port_name_t, mach_port_t, MACH_PORT_NULL};
use mach::vm_types::{mach_vm_address_t, mach_vm_size_t};
use mach::message::mach_msg_type_number_t;
use mach::vm_region::{vm_region_basic_info_data_t, vm_region_info_t,
                      vm_region_basic_info_data_64_t, VM_REGION_BASIC_INFO};
use mach::types::vm_task_entry_t;
use libproc::libproc::proc_pid::regionfilename;
use mach;

mod dyld_bindings;
use self::dyld_bindings::{dyld_all_image_infos, dyld_image_info, mach_header_64, segment_command_64};

pub type Pid = pid_t;

#[derive(Debug, Clone)]
pub struct MapRange {
    size: mach_vm_size_t,
    info: vm_region_basic_info_data_t,
    start: mach_vm_address_t,
    count: mach_msg_type_number_t,
    filename: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub value: Option<usize>,
    pub typ: String,
    pub name: String,
}

fn parse_nm_output(output: &str) -> Vec<Symbol> {
    let mut vec = vec![];
    for line in output.split('\n') {
        let split: Vec<&str> = line.split_whitespace().collect();
        let sym = if split.len() == 2 {
            Symbol {
                value: None,
                typ: split[0].to_string(),
                name: split[1].to_string(),
            }
        } else if split.len() == 3 {
            let value = usize::from_str_radix(split[0], 16).unwrap();
            Symbol {
                value: Some(value),
                typ: split[1].to_string(),
                name: split[2].to_string(),
            }
        } else {
            continue;
        };
        vec.push(sym);
    }
    vec
}

pub fn get_symbols(filename: &str) -> Result<Vec<Symbol>, Error> {
    let output = std::process::Command::new("nm").arg(filename).output()?;
    Ok(parse_nm_output(&String::from_utf8_lossy(&output.stdout)))
}

impl MapRange {
    pub fn size(&self) -> usize { self.size as usize }
    pub fn start(&self) -> usize { self.start as usize }
    pub fn filename(&self) -> &Option<String> { &self.filename }

    pub fn is_read(&self) -> bool {
        self.info.protection & mach::vm_prot::VM_PROT_READ != 0
    }
    pub fn is_write(&self) -> bool {
        self.info.protection & mach::vm_prot::VM_PROT_WRITE != 0
    }
    pub fn is_exec(&self) -> bool {
        self.info.protection & mach::vm_prot::VM_PROT_EXECUTE != 0
    }

    fn end(&self) -> mach_vm_address_t {
        self.start + self.size as mach_vm_address_t
    }
}

/*
 * The way the `mach_vm_region` API works is a bit weird -- you give it an address, and then it
 * returns the first memory map in the process **after** that address. So we start by passing it `1`
 * to get the first memory map in the process, pass the end of that memory map to get the second
 * memory map, etc.
 */
pub fn get_process_maps(pid: Pid) -> io::Result<Vec<MapRange>> {
    let task = task_for_pid(pid)?;
    let init_region = mach_vm_region(pid, task, 1).unwrap();
    let mut vec = vec![];
    let mut region = init_region.clone();
    vec.push(init_region);
    loop {
        match mach_vm_region(pid, task, region.end()) {
            Some(r) => {
                vec.push(r.clone());
                region = r;
            }
            _ => return Ok(vec),
        }
    }
}

fn mach_vm_region(
    pid: Pid,
    target_task: mach_port_name_t,
    mut address: mach_vm_address_t,
) -> Option<MapRange> {
    let mut count = mem::size_of::<vm_region_basic_info_data_64_t>() as mach_msg_type_number_t;
    let mut object_name: mach_port_t = 0;
    let mut size = unsafe { mem::zeroed::<mach_vm_size_t>() };
    let mut info = unsafe { mem::zeroed::<vm_region_basic_info_data_t>() };
    let result = unsafe {
        mach::vm::mach_vm_region(
            target_task as vm_task_entry_t,
            &mut address,
            &mut size,
            VM_REGION_BASIC_INFO,
            &mut info as *mut vm_region_basic_info_data_t as vm_region_info_t,
            &mut count,
            &mut object_name,
        )
    };
    if result != KERN_SUCCESS {
        return None;
    }
    let filename = match regionfilename(pid, address) {
        Ok(x) => Some(x),
        _ => None,
    };
    Some(MapRange{size, info, start: address, count, filename})
}

pub fn task_for_pid(pid: Pid) -> io::Result<mach_port_name_t> {
    let mut task: mach_port_name_t = MACH_PORT_NULL;
    // sleep for 10ms to make sure we don't get into a race between `task_for_pid` and execing a new
    // process. Races here can freeze the OS because of a Mac kernel bug on High Sierra.
    // See https://jvns.ca/blog/2018/01/28/mac-freeze/ for more.
    std::thread::sleep(std::time::Duration::from_millis(10));
    unsafe {
        let result =
            mach::traps::task_for_pid(mach::traps::mach_task_self(), pid as c_int, &mut task);
        if result != KERN_SUCCESS {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(task)
}

#[derive(Debug, Clone)]
pub struct DyldInfo {
    pub filename: String,
    pub address: usize,
    pub file_mod_date: usize,
    pub segment: segment_command_64
}

/// Returns basic information on modules loaded up by dyld. This lets
/// us get the filename/address of the system Ruby or Python frameworks for instance.
/// (which won't appear as a separate entry in vm_regions returned by get_process_maps)
pub fn get_dyld_info(pid: Pid) -> io::Result<Vec<DyldInfo>> {
    // Adapted from :
    // https://stackoverflow.com/questions/4309117/determining-programmatically-what-modules-are-loaded-in-another-process-os-x
    // https://blog.lse.epita.fr/articles/82-playing-with-mach-os-and-dyld.html

    // This gets addresses to TEXT sections ... but we really want addresses to DATA
    // this is a good start though
    // hmm
    use mach::task::task_info;
    use mach::task_info::{TASK_DYLD_INFO, task_info_t};

    let mut vec = Vec::new();
    let task = task_for_pid(pid)?;

    // Note: this seems to require osx MAC_OS_X_VERSION_10_6 or greater
    // https://chromium.googlesource.com/breakpad/breakpad/+/master/src/client/mac/handler/dynamic_images.cc#388
    let mut dyld_info = task_dyld_info{all_image_info_addr: 0, all_image_info_size: 0, all_image_info_format: 0};

    // TASK_DYLD_INFO_COUNT is #define'd to be 5 in /usr/include/mach/task_info.h
    // ... doesn't seem to be included in the mach crate =(
    let mut count: mach_msg_type_number_t = 5;
    unsafe {
        if task_info(task, TASK_DYLD_INFO,
                    &mut dyld_info as *mut task_dyld_info as task_info_t,
                    &mut count) != KERN_SUCCESS {
            return Err(io::Error::last_os_error());
        }
    }

    // Read in the dyld_all_image_infos information here here.
    let mut image_infos = dyld_all_image_infos::default();
    let mut read_len = std::mem::size_of_val(&image_infos) as mach_vm_size_t;

    let result = unsafe {
        // While we could use the read_process_memory crate for this, this adds a dependency
        // for something that is pretty trivial
        vm_read_overwrite(task,
            dyld_info.all_image_info_addr,
            read_len,
            (&mut image_infos) as *mut dyld_all_image_infos as mach_vm_address_t,
            &mut read_len)
    };
    if result != KERN_SUCCESS {
        return Err(io::Error::last_os_error());
    }

    // copy the infoArray element of dyld_all_image_infos ovber
    let mut modules = vec![dyld_image_info::default(); image_infos.infoArrayCount as usize];
    let mut read_len = (std::mem::size_of::<dyld_image_info>()
                        * image_infos.infoArrayCount as usize) as mach_vm_size_t;
    let result = unsafe {
        vm_read_overwrite(task,
            image_infos.infoArray as mach_vm_address_t,
            read_len,
            modules.as_mut_ptr() as mach_vm_address_t,
            &mut read_len)
    };
    if result != KERN_SUCCESS {
        return Err(io::Error::last_os_error());
    }

    for module in modules {
        let mut read_len = 512 as mach_vm_size_t;
        let mut image_filename = [0_i8; 512];
        let result = unsafe {
            vm_read_overwrite(task,
                module.imageFilePath as mach_vm_address_t,
                read_len,
                image_filename.as_mut_ptr() as mach_vm_address_t,
                &mut read_len)
        };
        if result != KERN_SUCCESS {
            return Err(io::Error::last_os_error());
        }

        let ptr = image_filename.as_ptr();
        let slice = unsafe { std::slice::from_raw_parts(ptr as *mut u8, strlen(ptr))};
        let filename = std::str::from_utf8(slice).unwrap().to_owned();

        // read in the mach header
        let mut header = mach_header_64::default();
        let mut read_len = std::mem::size_of_val(&header) as mach_vm_size_t;
        let result = unsafe {
            // While we could use the read_process_memory crate for this, this adds a dependency
            // for something that is pretty trivial
            vm_read_overwrite(task, module.imageLoadAddress as u64,
                read_len,
                (&mut header) as *mut mach_header_64 as mach_vm_address_t,
                &mut read_len)
        };
        if result != KERN_SUCCESS {
            return Err(io::Error::last_os_error());
        }

        let mut commands_buffer = vec![0_i8; header.sizeofcmds as usize];
        let mut read_len = mach_vm_size_t::from(header.sizeofcmds);
        let result = unsafe {
            vm_read_overwrite(task,
                (module.imageLoadAddress as usize + std::mem::size_of_val(&header)) as mach_vm_size_t,
                read_len,
                commands_buffer.as_mut_ptr() as mach_vm_address_t,
                &mut read_len)
        };
        if result != KERN_SUCCESS {
            return Err(io::Error::last_os_error());
        }

        // Figure out the slide from the __TEXT segment if appropiate
        let mut offset: u32 = 0;
        let mut slide: u64 = 0;
        for _ in 0..header.ncmds {
            unsafe {
                let command = *(commands_buffer.as_ptr().offset(offset as isize) as * const segment_command_64);
                // LC_SEGMENT_64 = 0x19 TODO
                // find the __TEXT segment and compute the slide if appropiate
                if command.cmd == 0x19 && command.segname[0..7] ==  [95, 95, 84, 69, 88, 84, 0] {
                    slide = module.imageLoadAddress as u64 - command.vmaddr;
                    break;
                }
                offset += command.cmdsize;
            }
        }

        let mut offset: u32 = 0;
        for _ in 0..header.ncmds {
            unsafe {
                let mut command = *(commands_buffer.as_ptr().offset(offset as isize) as * const segment_command_64);
                if command.cmd == 0x19 {
                    command.vmaddr += slide;
                    vec.push(DyldInfo{filename: filename.clone(),
                                    address: module.imageLoadAddress as usize,
                                    file_mod_date: module.imageFileModDate,
                                    segment: command});
                }
                offset += command.cmdsize;
            }
        }
    }
    Ok(vec)
}

extern "C" {
    fn vm_read_overwrite(target_task: mach_port_t,
                         address: mach_vm_address_t,
                         size: mach_vm_size_t,
                         data: mach_vm_address_t,
                         out_size: *mut mach_vm_size_t) -> kern_return_t;
}

// bindgen seemed to put all the members for this struct as a single opaque blob:
//      (bindgen /usr/include/mach/task_info.h --with-derive-default --whitelist-type task_dyld_info)
// rather than debug the bindgen command, just define manually here
#[repr(C)]
#[derive(Default, Debug)]
pub struct task_dyld_info {
    pub all_image_info_addr: mach_vm_address_t,
    pub all_image_info_size: mach_vm_size_t,
    pub all_image_info_format: mach::vm_types::integer_t,
}
