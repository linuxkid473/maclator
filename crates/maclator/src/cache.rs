//! Emulation of the kernel's shared-region syscalls for the arm64e dyld
//! shared cache.
//!
//! dyld asks the kernel to map the cache (`shared_region_map_and_slide_2_np`)
//! and later asks where it is (`shared_region_check_np`). We perform the
//! mapping ourselves at a fixed address inside our process (so AOT
//! translations of cache code stay valid across runs) and apply the slide
//! info (rebasing; pointer-auth signing is the identity in Maclator).

use crate::hostsys;
use std::sync::Mutex;

/// Where we place the arm64 shared region (unslid start 0x180000000).
pub const CACHE_BASE: u64 = 0x6_0000_0000;

pub static CACHE: Mutex<Option<MappedCache>> = Mutex::new(None);

#[derive(Debug, Clone)]
pub struct MappedCache {
    pub base: u64,
    pub slide: u64,
    pub ranges: Vec<(u64, u64)>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct SharedFileNp {
    fd: i32,
    mappings_count: u32,
    slide: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct MappingSlideNp {
    address: u64,
    size: u64,
    file_offset: u64,
    slide_size: u64,
    slide_start: u64,
    max_prot: u32,
    init_prot: u32,
}

const VM_PROT_ZF: u32 = 0x10;

#[allow(dead_code)]
fn pread_all(fd: i32, buf: &mut [u8], off: u64) -> bool {
    let mut done = 0usize;
    while done < buf.len() {
        let r = unsafe { libc::pread(fd, buf[done..].as_mut_ptr() as *mut _, buf.len() - done, (off + done as u64) as i64) };
        if r <= 0 {
            return false;
        }
        done += r as usize;
    }
    true
}

/// Handle `shared_region_map_and_slide_2_np(files_count, files, mappings_count, mappings)`.
/// Returns 0 or an errno.
pub fn map_and_slide_2(files_count: u32, files: u64, mappings_count: u32, mappings: u64) -> i64 {
    let files: &[SharedFileNp] = unsafe { std::slice::from_raw_parts(files as *const SharedFileNp, files_count as usize) };
    let maps: &[MappingSlideNp] = unsafe { std::slice::from_raw_parts(mappings as *const MappingSlideNp, mappings_count as usize) };
    if maps.is_empty() {
        return libc::EINVAL as i64;
    }
    let mut guard = CACHE.lock().unwrap();
    if guard.is_some() {
        return libc::EEXIST as i64;
    }
    let unslid_base = maps[0].address;
    let slide = CACHE_BASE.wrapping_sub(unslid_base);
    let mut ranges = Vec::new();
    let mut mi = 0usize;
    for f in files {
        for _ in 0..f.mappings_count {
            let m = maps[mi];
            mi += 1;
            let addr = m.address.wrapping_add(slide);
            let size = m.size as usize;
            let prot = (m.init_prot & 3) as i32; // never map guest code executable on the host
            unsafe {
                let p = if f.fd < 0 {
                    // Dynamic config data lives in the caller's memory.
                    let p = libc::mmap(addr as *mut _, size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_FIXED, -1, 0);
                    if p != libc::MAP_FAILED {
                        std::ptr::copy_nonoverlapping(m.file_offset as *const u8, p as *mut u8, size);
                    }
                    p
                } else if m.max_prot & VM_PROT_ZF != 0 {
                    libc::mmap(addr as *mut _, size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_FIXED, -1, 0)
                } else {
                    let wprot = if m.slide_size != 0 { libc::PROT_READ | libc::PROT_WRITE } else { prot.max(libc::PROT_READ) };
                    libc::mmap(addr as *mut _, size, wprot, libc::MAP_PRIVATE | libc::MAP_FIXED, f.fd, m.file_offset as i64)
                };
                if p == libc::MAP_FAILED || p as u64 != addr {
                    eprintln!("maclator: failed to map shared cache region {:#x}+{:#x}: {}", addr, size, std::io::Error::last_os_error());
                    return libc::ENOMEM as i64;
                }
                if m.slide_size == 0 {
                    let final_prot = if prot == 0 { libc::PROT_NONE } else { prot };
                    libc::mprotect(addr as *mut _, size, final_prot);
                }
            }
            ranges.push((addr, m.size));
        }
    }
    // Second pass: apply slide info. It lives in each file's __LINKEDIT
    // mapping (sms_slide_start is an unslid address), mapped above.
    for m in maps.iter() {
        if m.slide_size == 0 {
            continue;
        }
        let addr = m.address.wrapping_add(slide);
        let info = unsafe { std::slice::from_raw_parts(m.slide_start.wrapping_add(slide) as *const u8, m.slide_size as usize) };
        if let Err(e) = unsafe { apply_slide_info(info, addr, m.size, slide, unslid_base) } {
            eprintln!("maclator: slide info: {e}");
            return libc::EINVAL as i64;
        }
        let prot = (m.init_prot & 3) as i32;
        unsafe { libc::mprotect(addr as *mut _, m.size as usize, if prot == 0 { libc::PROT_NONE } else { prot }) };
    }
    if std::env::var_os("MACLATOR_TRACE").is_some() {
        eprintln!("[maclator] mapped arm64 shared cache at {:#x} (slide {:#x}), {} regions", CACHE_BASE, slide, ranges.len());
    }
    let end = ranges.iter().map(|(a, s)| a + s).max().unwrap_or(CACHE_BASE);
    *guard = Some(MappedCache { base: CACHE_BASE, slide, ranges });
    drop(guard);
    let mut uuid = [0u8; 16];
    unsafe { std::ptr::copy_nonoverlapping((CACHE_BASE + 88) as *const u8, uuid.as_mut_ptr(), 16) };
    crate::aot::register_cache(uuid, CACHE_BASE, end);
    0
}

pub fn check_np() -> Option<u64> {
    CACHE.lock().unwrap().as_ref().map(|c| c.base)
}

fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Apply dyld cache slide info (v3 or v5) to a mapped region.
unsafe fn apply_slide_info(info: &[u8], region: u64, size: u64, slide: u64, unslid_base: u64) -> Result<(), String> {
    let version = rd32(info, 0);
    match version {
        3 => {
            let page_size = rd32(info, 4) as u64;
            let count = rd32(info, 8) as usize;
            let auth_value_add = rd64(info, 16);
            for i in 0..count {
                let start = u16::from_le_bytes([info[24 + i * 2], info[25 + i * 2]]);
                if start == 0xFFFF {
                    continue;
                }
                let page = region + i as u64 * page_size;
                if page >= region + size {
                    break;
                }
                let mut loc = page + start as u64;
                loop {
                    let raw = std::ptr::read_unaligned(loc as *const u64);
                    let delta = (raw >> 51) & 0x7ff;
                    let auth = raw >> 63 != 0;
                    let v = if auth {
                        (raw & 0xffff_ffff).wrapping_add(auth_value_add).wrapping_add(slide)
                    } else {
                        let v51 = raw & ((1u64 << 51) - 1);
                        let top8 = v51 & 0x0007_F800_0000_0000;
                        let bottom43 = v51 & 0x0000_07FF_FFFF_FFFF;
                        ((top8 << 13) | bottom43).wrapping_add(slide)
                    };
                    std::ptr::write_unaligned(loc as *mut u64, v);
                    if delta == 0 {
                        break;
                    }
                    loc += delta * 8;
                }
            }
            Ok(())
        }
        5 => {
            let page_size = rd32(info, 4) as u64;
            let count = rd32(info, 8) as usize;
            let value_add = rd64(info, 16);
            let _ = unslid_base;
            for i in 0..count {
                let start = u16::from_le_bytes([info[24 + i * 2], info[25 + i * 2]]);
                if start == 0xFFFF {
                    continue;
                }
                let page = region + i as u64 * page_size;
                if page >= region + size {
                    break;
                }
                let mut loc = page + start as u64;
                loop {
                    let raw = std::ptr::read_unaligned(loc as *const u64);
                    let next = (raw >> 52) & 0x7ff;
                    let auth = raw >> 63 != 0;
                    let target = (raw & ((1u64 << 34) - 1)).wrapping_add(value_add).wrapping_add(slide);
                    let v = if auth { target } else { target | (((raw >> 34) & 0xff) << 56) };
                    std::ptr::write_unaligned(loc as *mut u64, v);
                    if next == 0 {
                        break;
                    }
                    loc += next * 8;
                }
            }
            Ok(())
        }
        v => Err(format!("unsupported slide info version {v}")),
    }
}

/// Keep `hostsys` referenced for callers that map via raw syscalls.
#[allow(dead_code)]
fn _unused() {
    let _ = hostsys::CLASS_UNIX;
}
