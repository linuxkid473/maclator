//! Guest system call emulation.
//!
//! arm64 Darwin: `svc #0x80` with the number in x16 (positive: BSD, negative:
//! Mach trap, 0x80000000: platform call selected by x3). Arguments in x0..x7.
//! BSD results go in x0/x1 with the carry flag reporting errors.
//! Most calls pass straight through to the host kernel.

use crate::cache;
use crate::hostsys::{self, SysResult};
use crate::threads;
use maclator_core::cpu::Cpu;
use std::sync::atomic::{AtomicBool, Ordering};

pub static TRACE: AtomicBool = AtomicBool::new(false);

const PLATFORM_SYSCALL: i32 = 0x8000_0000u32 as i32;

// BSD numbers we care about
const SYS_SYSCALL: u64 = 0;
const SYS_EXIT: u64 = 1;
const SYS_VFORK: u64 = 66;
const SYS_FORK: u64 = 2;
const SYS_SIGACTION: u64 = 46;
const SYS_SIGALTSTACK: u64 = 53;
const SYS_MPROTECT: u64 = 74;
const SYS_SIGRETURN: u64 = 184;
const SYS_MMAP: u64 = 197;
const SYS_MUNMAP: u64 = 73;
const SYS_SYSCTL: u64 = 202;
const SYS_SYSCTLBYNAME: u64 = 274;
const SYS_SHARED_REGION_CHECK_NP: u64 = 294;
const SYS_BSDTHREAD_CREATE: u64 = 360;
const SYS_BSDTHREAD_TERMINATE: u64 = 361;
const SYS_BSDTHREAD_REGISTER: u64 = 366;
const SYS_WORKQ_OPEN: u64 = 367;
const SYS_WORKQ_KERNRETURN: u64 = 368;
const SYS_SHARED_REGION_MAP_AND_SLIDE_2_NP: u64 = 536;
const SYS_MAP_WITH_LINKING_NP: u64 = 550;
const SYS_EXECVE: u64 = 59;
const SYS_POSIX_SPAWN: u64 = 244;

const PROT_EXEC: u64 = 4;
const MAP_JIT: u64 = 0x800;

#[inline]
fn set_ok(cpu: &mut Cpu, r0: u64, r1: u64) {
    cpu.x[0] = r0;
    cpu.x[1] = r1;
    cpu.cf = 0;
}
#[inline]
fn set_err(cpu: &mut Cpu, errno: u64) {
    cpu.x[0] = errno;
    cpu.cf = 1;
}
#[inline]
fn set_res(cpu: &mut Cpu, r: SysResult) {
    if r.carry {
        set_err(cpu, r.rax);
    } else {
        set_ok(cpu, r.rax, r.rdx);
    }
}

fn cstr_at(p: u64) -> String {
    if p == 0 {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(p as *const libc::c_char).to_string_lossy().into_owned() }
}

/// Handle SVC. `cpu.pc` already points after the SVC.
pub fn handle_svc(cpu: &mut Cpu) {
    let num = cpu.x[16] as u32 as i32;
    if num == PLATFORM_SYSCALL {
        match cpu.x[3] {
            2 => cpu.tpidrro_el0 = cpu.x[0],
            3 => cpu.x[0] = cpu.tpidrro_el0,
            _ => {}
        }
        return;
    }
    if num < 0 {
        mach_trap(cpu, (-num) as u64);
        return;
    }
    let mut n = num as u64;
    let mut args = [cpu.x[0], cpu.x[1], cpu.x[2], cpu.x[3], cpu.x[4], cpu.x[5], cpu.x[6], cpu.x[7]];
    if n == SYS_SYSCALL {
        n = args[0];
        args = [args[1], args[2], args[3], args[4], args[5], args[6], args[7], 0];
    }
    let trace = TRACE.load(Ordering::Relaxed);
    if trace {
        eprintln!("[sys] {}#{}({:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x}) pc={:#x}", syscall_name(n), n, args[0], args[1], args[2], args[3], args[4], args[5], cpu.pc - 4);
    }
    bsd(cpu, n, args);
    if trace {
        eprintln!("[sys]   -> {}{:#x}", if cpu.cf != 0 { "ERR " } else { "" }, cpu.x[0]);
    }
}

/// Guest paths of open fds under the redirected cache prefixes ("/", "/System", ...),
/// so `openat(dirfd, "rel")` walks (as dyld does) can be resolved to a full guest path.
static FD_VPATHS: std::sync::Mutex<Option<std::collections::HashMap<i32, String>>> = std::sync::Mutex::new(None);

fn vpath_join(base: &str, rel: &str) -> String {
    if base == "/" {
        format!("/{rel}")
    } else {
        format!("{}/{rel}", base.trim_end_matches('/'))
    }
}

fn bsd(cpu: &mut Cpu, n: u64, mut args: [u64; 8]) {
    // Path-taking syscalls: (path arg index, dirfd arg index for *at variants).
    let (pidx, at) = match n {
        5 | 398 => (Some(0), false),
        33 | 58 | 188 | 190 | 220 | 338 | 340 => (Some(0), false),
        463 | 464 | 469 | 470 => (Some(1), true),
        _ => (None, false),
    };
    let mut keep: Option<std::ffi::CString> = None;
    let mut guest_path: Option<String> = None;
    if let Some(i) = pidx {
        if args[i] != 0 {
            let orig = cstr_at(args[i]);
            let full = if orig.starts_with('/') {
                Some(orig.clone())
            } else if at {
                let dfd = args[0] as i32;
                FD_VPATHS.lock().unwrap().as_ref().and_then(|m| m.get(&dfd).map(|b| vpath_join(b, &orig)))
            } else {
                None
            };
            if TRACE.load(Ordering::Relaxed) {
                eprintln!("[path] #{n} {orig} -> {full:?}");
            }
            if let Some(full) = full {
                if let Some(new) = crate::paths::redirect(&full) {
                    if let Ok(c) = std::ffi::CString::new(new) {
                        args[i] = c.as_ptr() as u64;
                        if at {
                            args[0] = (-2i64) as u64; // AT_FDCWD; path is absolute now
                        }
                        keep = Some(c);
                    }
                }
                guest_path = Some(full);
            }
        }
    }
    bsd_inner(cpu, n, args);
    drop(keep);
    if matches!(n, 5 | 398 | 463 | 464) && cpu.cf == 0 {
        if let Some(g) = guest_path {
            if g == "/" || g.starts_with("/System") {
                let g = if g.len() > 1 { g.trim_end_matches('/').to_string() } else { g };
                FD_VPATHS.lock().unwrap().get_or_insert_with(Default::default).insert(cpu.x[0] as i32, g);
            }
        }
    } else if matches!(n, 6 | 399) {
        if let Some(m) = FD_VPATHS.lock().unwrap().as_mut() {
            m.remove(&(args[0] as i32));
        }
    }
}

fn bsd_inner(cpu: &mut Cpu, n: u64, mut args: [u64; 8]) {
    match n {
        crate::gpu::SYS_MCL_HOSTCALL => crate::gpu::hostcall(cpu),
        SYS_VFORK | SYS_FORK => {
            // vfork shares our address space and stack: use fork instead. Go through libc's
            // fork (not the raw syscall) so the host libSystem re-initialises in the child
            // (mach_task_self, MIG reply ports, ...); the JIT reads guest memory through
            // mach_vm_read with the cached task port.
            crate::engine::flush_profile();
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                set_err(cpu, unsafe { *libc::__error() } as u64);
            } else if pid == 0 {
                // Child: XNU returns the parent's pid with the "is child" flag in x1.
                set_ok(cpu, unsafe { libc::getppid() } as u64, 1);
            } else {
                set_ok(cpu, pid as u64, 0);
            }
        }
        SYS_EXIT => {
            crate::engine::flush_profile();
            let r = hostsys::unix(SYS_EXIT, &args);
            set_res(cpu, r);
        }
        SYS_SIGACTION => {
            let r = crate::signals::sigaction(args[0] as i32, args[1], args[2]);
            match r {
                Ok(()) => set_ok(cpu, 0, 0),
                Err(e) => set_err(cpu, e as u64),
            }
        }
        SYS_SIGALTSTACK => {
            if args[1] != 0 {
                unsafe { std::ptr::write_bytes(args[1] as *mut u8, 0, 24) };
                unsafe { *((args[1] + 16) as *mut u32) = 4 }; // SS_DISABLE
            }
            set_ok(cpu, 0, 0);
        }
        SYS_SIGRETURN => {
            crate::signals::sigreturn(cpu, args[0], args[1] as i32);
        }
        SYS_MMAP => {
            let was_exec = args[2] & PROT_EXEC != 0;
            args[2] &= !PROT_EXEC;
            args[3] &= !MAP_JIT;
            let r = mmap_16k(&args);
            // dyld maps a library's __TEXT (file offset 0, executable) at its final address:
            // register it with the AOT cache so translations persist across runs.
            if !r.carry && was_exec && args[5] == 0 && (args[4] as i64) >= 0 && args[1] >= 0x1000 {
                crate::aot::register_image_at(r.rax);
            }
            set_res(cpu, r);
        }
        SYS_MPROTECT => {
            let exec = args[2] & PROT_EXEC != 0;
            args[2] &= !PROT_EXEC;
            let r = hostsys::unix(n, &args);
            if !r.carry && exec {
                crate::engine::invalidate_range(args[0], args[1]);
            }
            set_res(cpu, r);
        }
        SYS_SYSCTLBYNAME => {
            let name = cstr_at(args[0]);
            if name == "hw.machine" {
                write_sysctl_str(cpu, args[2], args[3], "arm64");
                return;
            }
            if let Some(v) = sysctl_override(&name) {
                write_sysctl_int(cpu, args[2], args[3], v);
                return;
            }
            let r = hostsys::unix(n, &args);
            set_res(cpu, r);
        }
        SYS_SYSCTL => {
            // hw.machine (CTL_HW=6, HW_MACHINE=1) is what uname() reports.
            let mib = unsafe { std::slice::from_raw_parts(args[0] as *const i32, (args[1] as usize).min(8)) };
            if args[1] == 2 && mib == [6, 1] {
                write_sysctl_str(cpu, args[2], args[3], "arm64");
                return;
            }
            let r = hostsys::unix(n, &args);
            set_res(cpu, r);
        }
        SYS_SHARED_REGION_CHECK_NP => {
            // -1 (DYLD_VM_END_MWL) marks the end of map_with_linking; 0 just
            // refreshes the task's shared region.
            if args[0] == u64::MAX || args[0] == 0 {
                set_ok(cpu, 0, 0);
                return;
            }
            match cache::check_np() {
                Some(base) => {
                    if crate::guestmem::write_u64(args[0], base) {
                        set_ok(cpu, 0, 0);
                    } else {
                        set_err(cpu, libc::EFAULT as u64);
                    }
                }
                None => set_err(cpu, libc::ENOMEM as u64),
            }
        }
        SYS_SHARED_REGION_MAP_AND_SLIDE_2_NP => {
            let e = cache::map_and_slide_2(args[0] as u32, args[1], args[2] as u32, args[3]);
            if e == 0 {
                set_ok(cpu, 0, 0);
            } else {
                set_err(cpu, e as u64);
            }
        }
        SYS_MAP_WITH_LINKING_NP => set_err(cpu, libc::ENOTSUP as u64),
        SYS_BSDTHREAD_REGISTER => {
            let r = threads::bsdthread_register(&args);
            match r {
                Ok(v) => set_ok(cpu, v, 0),
                Err(e) => set_err(cpu, e),
            }
        }
        SYS_BSDTHREAD_CREATE => match threads::bsdthread_create(&args) {
            Ok(v) => set_ok(cpu, v, 0),
            Err(e) => set_err(cpu, e),
        },
        SYS_BSDTHREAD_TERMINATE => threads::bsdthread_terminate(cpu, &args),
        SYS_WORKQ_OPEN => set_ok(cpu, 0, 0),
        SYS_WORKQ_KERNRETURN => match crate::workq::kernreturn(cpu, &args) {
            Ok(v) => set_ok(cpu, v, 0),
            Err(e) => set_err(cpu, e),
        },
        SYS_EXECVE | SYS_POSIX_SPAWN => {
            if n == SYS_EXECVE {
                crate::engine::flush_profile();
            }
            let r = crate::spawn::exec_like(n, &args);
            set_res(cpu, r);
        }
        374 | 375 => {
            if TRACE.load(Ordering::Relaxed) {
                eprintln!("[kev] #{n} fd/id={:#x} nchanges={} nevents={} flags={:#x}", args[0], args[2], args[4], args[7]);
                for i in 0..(args[2].min(4)) {
                    let e = args[1] + i * 72;
                    let v = unsafe { std::slice::from_raw_parts(e as *const u64, 9) };
                    eprintln!("[kev]   change ident={:#x} filter={} flags={:#x} qos={:#x} udata={:#x} fflags={:#x}", v[0], (v[1] & 0xffff) as i16, (v[1] >> 16) & 0xffff, v[1] >> 32, v[2], v[3] & 0xffffffff);
                }
            }
            let r = crate::workq::kevent_redirect(n, &args).unwrap_or_else(|| hostsys::unix(n, &args));
            set_res(cpu, r);
        }
        // bsdthread_ctl(BSDTHREAD_CTL_SET_SELF): thread QoS/priority/voucher. Guest threads are
        // host threads we schedule ourselves, and the host kernel rejects some arm64-era
        // priority combinations with EINVAL (libdispatch then aborts), so just accept it.
        478 if args[0] == 0x100 => set_ok(cpu, 0, 0),
        92 if matches!(args[1], 59 | 61 | 97 | 98 | 103) => {
            // Code-signing fcntls (F_ADDSIGS, F_ADDFILESIGS[_RETURN/_INFO], F_CHECK_LV):
            // the Intel host kernel cannot validate arm64 signatures, so accept them.
            // F_ADDFILESIGS_RETURN/_INFO report the signed extent back in fs_file_start;
            // the signature blob sits at the tail, so everything before it is signed.
            if matches!(args[1], 97 | 103) && args[2] != 0 {
                unsafe {
                    let fs = args[2] as *mut u64;
                    let blob_off = *fs.add(1);
                    *fs = blob_off;
                }
            }
            set_ok(cpu, 0, 0);
        }
        _ => {
            let r = hostsys::unix(n, &args);
            set_res(cpu, r);
        }
    }
}

/// arm64 macOS has 16 KiB pages; the Intel host has 4 KiB. Guest allocators (V8,
/// PartitionAlloc, libmalloc) rely on mmap returning 16 KiB-aligned, 16 KiB-sized
/// regions, so over-reserve, align, and trim.
fn mmap_16k(args: &[u64; 8]) -> hostsys::SysResult {
    const PAGE: u64 = 0x4000;
    const MAP_FIXED: u64 = 0x10;
    const MAP_ANON: u64 = 0x1000;
    const MAP_PRIVATE: u64 = 0x2;
    let (len, flags, fd) = (args[1], args[3], args[4] as i64);
    if len == 0 || flags & MAP_FIXED != 0 {
        return hostsys::unix(SYS_MMAP, args);
    }
    let len16 = (len + PAGE - 1) & !(PAGE - 1);
    // Reserve len16 + one page of slack, address space only.
    let reserve = hostsys::unix(SYS_MMAP, &[0, len16 + PAGE, 0, MAP_ANON | MAP_PRIVATE, u64::MAX, 0, 0, 0]);
    if reserve.carry {
        return reserve;
    }
    let base = reserve.rax;
    let aligned = (base + PAGE - 1) & !(PAGE - 1);
    // Map the real thing over the aligned window.
    let mut a = *args;
    a[0] = aligned;
    a[1] = len16;
    a[3] = flags | MAP_FIXED;
    let _ = fd;
    let r = hostsys::unix(SYS_MMAP, &a);
    if r.carry {
        hostsys::unix(SYS_MUNMAP, &[base, len16 + PAGE, 0, 0, 0, 0, 0, 0]);
        return r;
    }
    // Give back the slack before and after the aligned window.
    if aligned > base {
        hostsys::unix(SYS_MUNMAP, &[base, aligned - base, 0, 0, 0, 0, 0, 0]);
    }
    let end = aligned + len16;
    let reserve_end = base + len16 + PAGE;
    if reserve_end > end {
        hostsys::unix(SYS_MUNMAP, &[end, reserve_end - end, 0, 0, 0, 0, 0, 0]);
    }
    r
}

fn write_sysctl_str(cpu: &mut Cpu, oldp: u64, oldlenp: u64, v: &str) {
    let bytes = [v.as_bytes(), &[0]].concat();
    unsafe {
        if oldlenp != 0 {
            let len = *(oldlenp as *const u64) as usize;
            if oldp != 0 {
                if len < bytes.len() {
                    set_err(cpu, libc::ENOMEM as u64);
                    return;
                }
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), oldp as *mut u8, bytes.len());
            }
            *(oldlenp as *mut u64) = bytes.len() as u64;
        }
    }
    set_ok(cpu, 0, 0);
}

fn write_sysctl_int(cpu: &mut Cpu, oldp: u64, oldlenp: u64, v: i64) {
    unsafe {
        if oldlenp != 0 {
            let len = *(oldlenp as *const u64);
            if oldp != 0 {
                if len >= 8 {
                    *(oldp as *mut i64) = v;
                    *(oldlenp as *mut u64) = 8;
                } else if len >= 4 {
                    *(oldp as *mut i32) = v as i32;
                    *(oldlenp as *mut u64) = 4;
                } else {
                    set_err(cpu, libc::ENOMEM as u64);
                    return;
                }
            } else {
                *(oldlenp as *mut u64) = 4;
            }
        }
    }
    set_ok(cpu, 0, 0);
}

/// sysctls whose answer must describe the emulated arm64 machine.
fn sysctl_override(name: &str) -> Option<i64> {
    Some(match name {
        "sysctl.proc_translated" => 0,
        "sysctl.proc_native" => 1,
        "hw.cputype" => 0x0100_000c,
        "hw.cpusubtype" => 2,
        "hw.optional.arm64" => 1,
        "hw.optional.neon" | "hw.optional.AdvSIMD" | "hw.optional.floatingpoint" => 1,
        "hw.optional.armv8_crc32" | "hw.optional.arm.FEAT_CRC32" => 1,
        "hw.optional.armv8_1_atomics" | "hw.optional.arm.FEAT_LSE" => 1,
        "hw.optional.arm.FEAT_LSE2" | "hw.optional.arm.FEAT_LRCPC" | "hw.optional.arm.FEAT_LRCPC2" => 1,
        "hw.optional.arm.FEAT_PAuth" | "hw.optional.arm.FEAT_JSCVT" | "hw.optional.arm.FEAT_FlagM"
        | "hw.optional.arm.FEAT_FlagM2" | "hw.optional.arm.FEAT_DotProd" | "hw.optional.arm.FEAT_FRINTTS"
        | "hw.optional.arm.FEAT_SB" | "hw.optional.arm.FEAT_DPB" | "hw.optional.arm.FEAT_DPB2" => 1,
        "hw.optional.arm.FEAT_FP16" | "hw.optional.arm.FEAT_FHM" | "hw.optional.arm.FEAT_FCMA"
        | "hw.optional.arm.FEAT_RDM" | "hw.optional.arm.FEAT_SHA3" | "hw.optional.arm.FEAT_SHA512"
        | "hw.optional.arm.FEAT_SME" | "hw.optional.arm.FEAT_SME2" | "hw.optional.arm.FEAT_BF16"
        | "hw.optional.arm.FEAT_I8MM" | "hw.optional.armv8_2_fhm" | "hw.optional.armv8_2_sha512"
        | "hw.optional.armv8_2_sha3" | "hw.optional.armv8_3_compnum" | "hw.optional.arm.FEAT_SHA256"
        | "hw.optional.arm.FEAT_AES" | "hw.optional.arm.FEAT_PMULL" | "hw.optional.arm.FEAT_SHA1"
        | "hw.optional.arm.FEAT_AFP" | "hw.optional.arm.FEAT_RPRES" | "hw.optional.arm.FEAT_ECV"
        | "hw.optional.arm.FEAT_WFxT" | "hw.optional.arm.FEAT_CSSC" | "hw.optional.arm.FEAT_HBC" => 0,
        "hw.pagesize" | "hw.pagesize32" | "vm.pagesize" => 16384,
        "hw.cpufamily" => crate::commpage::CPUFAMILY as i64,
        "hw.optional.x86_64" | "hw.optional.sse" | "hw.optional.sse2" | "hw.optional.sse3"
        | "hw.optional.supplementalsse3" | "hw.optional.sse4_1" | "hw.optional.sse4_2"
        | "hw.optional.avx1_0" | "hw.optional.avx2_0" => 0,
        _ => return None,
    })
}

fn mach_trap(cpu: &mut Cpu, n: u64) {
    let trace = TRACE.load(Ordering::Relaxed);
    match n {
        3 => {
            cpu.x[0] = unsafe { mach_absolute_time() };
            return;
        }
        4 => {
            cpu.x[0] = unsafe { mach_continuous_time() };
            return;
        }
        _ => {}
    }
    let args = [cpu.x[0], cpu.x[1], cpu.x[2], cpu.x[3], cpu.x[4], cpu.x[5], cpu.x[6], cpu.x[7]];
    if n == 10 && args[3] & 1 != 0 && args[1] != 0 && args[2] != 0 {
        // mach_vm_allocate(VM_FLAGS_ANYWHERE): place it from a deterministic arena so that
        // dyld-loaded libraries land at the same addresses every run (persistent AOT cache).
        // The arena is a large PROT_NONE reservation; allocations overwrite pieces of it.
        const ARENA_SIZE: u64 = 64 << 30;
        static ARENA: std::sync::OnceLock<(u64, std::sync::atomic::AtomicU64)> = std::sync::OnceLock::new();
        let (base, next) = ARENA.get_or_init(|| {
            for cand in [0x40_0000_0000u64, 0x20_0000_0000, 0x10_0000_0000, 0x0c_0000_0000, 0x08_0000_0000] {
                let p = unsafe {
                    libc::mmap(cand as *mut _, ARENA_SIZE as usize, libc::PROT_NONE, libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE, -1, 0)
                };
                if p as u64 == cand {
                    return (cand, std::sync::atomic::AtomicU64::new(0));
                }
                if p != libc::MAP_FAILED {
                    unsafe { libc::munmap(p, ARENA_SIZE as usize) };
                }
            }
            (0, std::sync::atomic::AtomicU64::new(0))
        });
        let sz = (args[2] + 0x3fff) & !0x3fff;
        if *base != 0 {
            let off = next.fetch_add(sz, Ordering::SeqCst);
            if off + sz <= ARENA_SIZE {
                let at = base + off;
                let orig_hint = unsafe { *(args[1] as *const u64) };
                unsafe { *(args[1] as *mut u64) = at };
                let mut fixed = args;
                fixed[3] = (fixed[3] & !1) | 0x4000; // fixed + VM_FLAGS_OVERWRITE
                let r = hostsys::mach(n, &fixed);
                if r.rax == 0 {
                    cpu.x[0] = 0;
                    return;
                }
                if trace {
                    eprintln!("[mach] arena vm_allocate at {at:#x} size {sz:#x} failed: {:#x}", r.rax);
                }
                unsafe { *(args[1] as *mut u64) = orig_hint };
            }
        }
    }
    if n == 47 && crate::iokit::intercept(args[0], args[1]) {
        if trace {
            eprintln!("[mach] msg2 answered by iokit shim");
        }
        cpu.x[0] = 0;
        return;
    }
    // mach_msg2: remember the request id/size before the reply overwrites the header.
    let req_id = if n == 47 && args[0] != 0 { unsafe { *((args[0] + 20) as *const u32) } } else { 0 };
    if trace && n == 47 && args[0] != 0 {
        unsafe {
            let h = args[0] as *const u32;
            let size = (*h.add(1)).min(512) as usize;
            let body = std::slice::from_raw_parts(args[0] as *const u8, size);
            let mut strs = String::new();
            let mut cur = String::new();
            for &b in body {
                if (0x20..0x7f).contains(&b) {
                    cur.push(b as char);
                } else {
                    if cur.len() >= 4 {
                        strs.push_str(&format!(" \"{cur}\""));
                    }
                    cur.clear();
                }
            }
            eprintln!("[mach] msg2> bits={:#x} size={} rport={:#x} lport={:#x} id={} opt={:#x}{}", *h, *h.add(1), *h.add(2), *h.add(3), *h.add(5), args[1], strs);
        }
    }
    let r = hostsys::mach(n, &args);
    if n == 47 && args[0] != 0 {
        let reply_id = unsafe { *((args[0] + 20) as *const u32) };
        let retcode = unsafe { *((args[0] + 32) as *const u32) };
        if trace {
            eprintln!("[mach] msg2 id={req_id} reply_id={reply_id} retcode={retcode:#x}");
            if (2800..3000).contains(&req_id) {
                let w: Vec<String> = (0..16).map(|i| format!("{:08x}", unsafe { *((args[0] + i * 4) as *const u32) })).collect();
                eprintln!("[mach] reply words: {}", w.join(" "));
            }
        }
        // task_restartable_ranges_register: the Intel host kernel does not support it, and
        // libobjc treats failure as fatal. Restartable ranges only matter for arm64 threads
        // we already run in the interpreter/JIT, so pretend it worked.
        if req_id == 8000 && reply_id == req_id + 100 && retcode == 46 {
            unsafe { *((args[0] + 32) as *mut u32) = 0 };
        }
    }
    if trace {
        eprintln!("[mach] trap {}({:#x}, {:#x}, {:#x}, {:#x}) -> {:#x}", n, args[0], args[1], args[2], args[3], r.rax);
    }
    cpu.x[0] = r.rax;
}

extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_continuous_time() -> u64;
}

pub fn syscall_name(n: u64) -> &'static str {
    match n {
        1 => "exit",
        2 => "fork",
        3 => "read",
        4 => "write",
        5 => "open",
        6 => "close",
        20 => "getpid",
        33 => "access",
        46 => "sigaction",
        48 => "sigprocmask",
        54 => "ioctl",
        59 => "execve",
        73 => "munmap",
        74 => "mprotect",
        92 => "fcntl",
        116 => "gettimeofday",
        153 => "pread",
        169 => "csops",
        184 => "sigreturn",
        194 => "getrlimit",
        197 => "mmap",
        202 => "sysctl",
        220 => "getattrlist",
        244 => "posix_spawn",
        274 => "sysctlbyname",
        294 => "shared_region_check_np",
        327 => "issetugid",
        336 => "proc_info",
        338 => "stat64",
        339 => "fstat64",
        340 => "lstat64",
        344 => "getdirentries64",
        360 => "bsdthread_create",
        361 => "bsdthread_terminate",
        366 => "bsdthread_register",
        367 => "workq_open",
        368 => "workq_kernreturn",
        372 => "thread_selfid",
        396 => "read_nocancel",
        397 => "write_nocancel",
        398 => "open_nocancel",
        399 => "close_nocancel",
        463 => "openat",
        464 => "openat_nocancel",
        500 => "getentropy",
        515 => "ulock_wait",
        516 => "ulock_wake",
        520 => "terminate_with_payload",
        521 => "abort_with_payload",
        536 => "shared_region_map_and_slide_2_np",
        550 => "map_with_linking_np",
        _ => "?",
    }
}
