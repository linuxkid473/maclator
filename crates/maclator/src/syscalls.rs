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

fn bsd(cpu: &mut Cpu, n: u64, mut args: [u64; 8]) {
    match n {
        SYS_VFORK => {
            // vfork shares our address space and stack: use fork instead.
            let r = hostsys::unix(SYS_FORK, &args);
            set_res(cpu, r);
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
            args[2] &= !PROT_EXEC;
            args[3] &= !MAP_JIT;
            let r = hostsys::unix(n, &args);
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
            if let Some(v) = sysctl_override(&name) {
                write_sysctl_int(cpu, args[2], args[3], v);
                return;
            }
            let r = hostsys::unix(n, &args);
            set_res(cpu, r);
        }
        SYS_SYSCTL => {
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
            crate::engine::flush_profile();
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
        _ => {
            let r = hostsys::unix(n, &args);
            set_res(cpu, r);
        }
    }
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
    let r = hostsys::mach(n, &args);
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
