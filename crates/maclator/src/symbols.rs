//! Diagnostics: map guest addresses to "image+offset" and walk frame chains.

use crate::cache;
use maclator_core::cpu::Cpu;
use std::sync::Mutex;

#[derive(Clone)]
struct Img {
    start: u64,
    end: u64,
    name: String,
}

static IMAGES: Mutex<Vec<Img>> = Mutex::new(Vec::new());
static CACHE_IMAGES_LOADED: Mutex<bool> = Mutex::new(false);

pub fn add_image(start: u64, end: u64, name: &str) {
    IMAGES.lock().unwrap().push(Img { start, end, name: name.to_string() });
}

fn load_cache_images() {
    let mut done = CACHE_IMAGES_LOADED.lock().unwrap();
    if *done {
        return;
    }
    let Some(base) = cache::check_np() else { return };
    *done = true;
    unsafe {
        let hdr = base as *const u8;
        let rd32 = |o: usize| std::ptr::read_unaligned(hdr.add(o) as *const u32);
        let rd64 = |p: *const u8| std::ptr::read_unaligned(p as *const u64);
        let slide = cache::CACHE.lock().unwrap().as_ref().map(|c| c.slide).unwrap_or(0);
        let images_off = rd32(0x1c0) as usize;
        let images_cnt = rd32(0x1c4) as usize;
        if images_off == 0 || images_cnt == 0 || images_cnt > 10000 {
            return;
        }
        let mut v = IMAGES.lock().unwrap();
        for i in 0..images_cnt {
            let e = hdr.add(images_off + i * 32);
            let addr = rd64(e).wrapping_add(slide);
            let path_off = std::ptr::read_unaligned(e.add(24) as *const u32) as usize;
            let path = std::ffi::CStr::from_ptr(hdr.add(path_off) as *const libc::c_char).to_string_lossy().into_owned();
            // __TEXT size from the mach header's first segment
            let mh = addr as *const u8;
            let ncmds = std::ptr::read_unaligned(mh.add(16) as *const u32);
            let mut p = mh.add(32);
            let mut size = 0x1000u64;
            for _ in 0..ncmds {
                let cmd = std::ptr::read_unaligned(p as *const u32);
                let cmdsize = std::ptr::read_unaligned(p.add(4) as *const u32);
                if cmd == 0x19 {
                    let name = std::ffi::CStr::from_ptr(p.add(8) as *const libc::c_char);
                    if name.to_bytes() == b"__TEXT" {
                        size = std::ptr::read_unaligned(p.add(32) as *const u64);
                        break;
                    }
                }
                p = p.add(cmdsize as usize);
            }
            let short = path.rsplit('/').next().unwrap_or(&path).to_string();
            v.push(Img { start: addr, end: addr + size, name: short });
        }
    }
}

pub fn describe(addr: u64) -> String {
    load_cache_images();
    let v = IMAGES.lock().unwrap();
    for i in v.iter() {
        if addr >= i.start && addr < i.end {
            return format!("{}+{:#x}", i.name, addr - i.start);
        }
    }
    "?".to_string()
}

pub fn backtrace(cpu: &Cpu) {
    let mut fp = cpu.x[29];
    for i in 0..32 {
        if fp == 0 || fp & 7 != 0 {
            break;
        }
        let (next, lr) = unsafe {
            let mut v = [0u64; 2];
            // Avoid faulting on a bogus chain.
            let mut len: libc::size_t = 16;
            let kr = mach_vm_read_overwrite(libc::mach_task_self(), fp, 16, v.as_mut_ptr() as u64, &mut len);
            if kr != 0 {
                break;
            }
            (v[0], v[1])
        };
        eprintln!("  #{i:<2} {:#x} {}", lr, describe(lr));
        fp = next;
    }
}

extern "C" {
    fn mach_vm_read_overwrite(task: libc::mach_port_t, addr: u64, size: u64, data: u64, outsize: *mut libc::size_t) -> libc::c_int;
}
