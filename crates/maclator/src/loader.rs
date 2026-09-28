//! Process image setup, doing what the XNU exec path does for an arm64
//! process: map the main executable and dyld, build the initial stack
//! (mach header, argc/argv/envp/apple strings) and point the CPU at dyld.

use crate::macho::MachO;
use maclator_core::cpu::Cpu;
use std::ffi::CString;
use std::os::unix::io::AsRawFd;

/// Fixed load addresses keep AOT translations position-stable.
pub const MAIN_BASE: u64 = 0x5_0000_0000;
pub const DYLD_BASE: u64 = 0x5_8000_0000;
pub const STACK_SIZE: u64 = 8 << 20;

pub struct Image {
    pub path: String,
    pub macho: MachO,
    pub slide: u64,
    pub base: u64,
}

pub struct Loaded {
    pub main: Image,
    pub dyld: Image,
    pub entry: u64,
    pub sp: u64,
    pub stack_lo: u64,
    pub stack_hi: u64,
}

fn map_image(path: &str, macho: &MachO, want_base: u64) -> std::io::Result<Image> {
    let file = std::fs::File::open(path)?;
    let fd = file.as_raw_fd();
    let (lo, hi) = macho.vm_range();
    let slide = want_base.wrapping_sub(lo);
    // Reserve the whole range first so segments land contiguously.
    unsafe {
        let p = libc::mmap(want_base as *mut _, (hi - lo) as usize, libc::PROT_NONE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0);
        if p as u64 != want_base {
            if p != libc::MAP_FAILED {
                libc::munmap(p, (hi - lo) as usize);
            }
            return Err(std::io::Error::other(format!("{path}: address {:#x} unavailable", want_base)));
        }
    }
    for seg in &macho.segments {
        if seg.name == "__PAGEZERO" || seg.vmsize == 0 {
            continue;
        }
        let addr = seg.vmaddr.wrapping_add(slide);
        // Guest code is never executed natively; map R/RW only.
        let prot = (seg.initprot & 3) as i32;
        unsafe {
            if seg.filesize > 0 {
                let p = libc::mmap(addr as *mut _, seg.filesize as usize, libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_FIXED, fd, (macho.slice_offset + seg.fileoff) as i64);
                if p as u64 != addr {
                    return Err(std::io::Error::other(format!("{path}: mapping {} failed: {}", seg.name, std::io::Error::last_os_error())));
                }
            }
            let file_end = (addr + seg.filesize + 0xfff) & !0xfff;
            let seg_end = addr + seg.vmsize;
            if seg_end > file_end {
                let p = libc::mmap(file_end as *mut _, (seg_end - file_end) as usize, libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_FIXED, -1, 0);
                if p as u64 != file_end {
                    return Err(std::io::Error::other(format!("{path}: zerofill {} failed", seg.name)));
                }
            }
            if seg.filesize & 0xfff != 0 && seg.vmsize > seg.filesize {
                // zero the tail of the last file page
                let tail = addr + seg.filesize;
                let end = ((tail + 0xfff) & !0xfff).min(seg_end);
                std::ptr::write_bytes(tail as *mut u8, 0, (end - tail) as usize);
            }
            libc::mprotect(addr as *mut _, seg.vmsize as usize, if prot == 0 { libc::PROT_READ } else { prot });
        }
    }
    let base = macho.text_vmaddr().wrapping_add(slide);
    Ok(Image { path: path.to_string(), macho: macho.clone(), slide, base })
}

fn file_ids(path: &str) -> (u64, u64) {
    let c = CString::new(path).unwrap();
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    unsafe { libc::stat(c.as_ptr(), &mut st) };
    (st.st_dev as u64, st.st_ino)
}

pub fn load(exe: &str, dyld_path: &str, argv: &[String], envp: &[String]) -> std::io::Result<(Loaded, Box<Cpu>)> {
    let main_m = MachO::open_arm64(exe, false)?;
    if main_m.filetype != crate::macho::MH_EXECUTE {
        return Err(std::io::Error::other(format!("{exe}: not an executable")));
    }
    let dyld_m = MachO::open_arm64(dyld_path, true)?;
    let main = map_image(exe, &main_m, MAIN_BASE)?;
    let dyld = map_image(dyld_path, &dyld_m, DYLD_BASE)?;
    let entry = dyld_m.thread_pc.ok_or_else(|| std::io::Error::other("dyld has no LC_UNIXTHREAD"))?.wrapping_add(dyld.slide);

    // Stack
    let stack_lo = unsafe {
        let p = libc::mmap(std::ptr::null_mut(), (STACK_SIZE + 0x10000) as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0);
        assert!(p != libc::MAP_FAILED);
        libc::mprotect(p, 0x10000, libc::PROT_NONE); // guard
        p as u64 + 0x10000
    };
    let stack_hi = stack_lo + STACK_SIZE;

    // apple[] strings
    let (edev, eino) = file_ids(exe);
    let (ddev, dino) = file_ids(dyld_path);
    let mut rnd = [0u64; 4];
    for r in rnd.iter_mut() {
        let mut b = [0u8; 8];
        unsafe { libc::arc4random_buf(b.as_mut_ptr() as *mut _, 8) };
        *r = u64::from_le_bytes(b);
    }
    let apple: Vec<String> = vec![
        format!("executable_path={exe}"),
        format!("ptr_munge=0x{:x}", rnd[0]),
        format!("main_stack=0x{:x},0x{:x},0x{:x},0x{:x}", stack_hi, STACK_SIZE, stack_lo - 0x10000, 0x10000),
        format!("executable_file=0x{:x},0x{:x}", edev, eino),
        format!("dyld_file=0x{:x},0x{:x}", ddev, dino),
        format!("stack_guard=0x{:x}", rnd[1] & !0xff),
        format!("malloc_entropy=0x{:x},0x{:x}", rnd[2], rnd[3]),
        "arm64e_abi=os".to_string(),
        format!("th_port=0x{:x}", unsafe { libc::mach_thread_self() }),
    ];

    // Build the string area at the top of the stack.
    let mut sp = stack_hi;
    let mut push_str = |s: &str| -> u64 {
        let bytes = s.as_bytes();
        sp -= bytes.len() as u64 + 1;
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), sp as *mut u8, bytes.len());
            *((sp + bytes.len() as u64) as *mut u8) = 0;
        }
        sp
    };
    let exec_path_ptr = push_str(exe);
    let _ = exec_path_ptr;
    let argv_p: Vec<u64> = argv.iter().map(|s| push_str(s)).collect();
    let envp_p: Vec<u64> = envp.iter().map(|s| push_str(s)).collect();
    let apple_p: Vec<u64> = apple.iter().map(|s| push_str(s)).collect();
    sp &= !0xf;
    // pointer area: mh, argc, argv..., 0, envp..., 0, apple..., 0
    let nwords = 2 + argv_p.len() + 1 + envp_p.len() + 1 + apple_p.len() + 1;
    sp -= (nwords as u64) * 8;
    sp &= !0xf;
    let mut w = sp;
    let mut put = |v: u64| {
        unsafe { *(w as *mut u64) = v };
        w += 8;
    };
    put(main.base);
    put(argv_p.len() as u64);
    for p in &argv_p {
        put(*p);
    }
    put(0);
    for p in &envp_p {
        put(*p);
    }
    put(0);
    for p in &apple_p {
        put(*p);
    }
    put(0);

    let mut cpu = Cpu::new();
    cpu.pc = entry;
    cpu.x[31] = sp;
    Ok((Loaded { main, dyld, entry, sp, stack_lo, stack_hi }, cpu))
}
