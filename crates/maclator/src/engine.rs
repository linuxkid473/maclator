//! Execution engine: runs a guest thread until it exits.
//!
//! Tiering: translated host code (AOT cache or JIT) when available, with the
//! reference interpreter as the fallback for anything not translated.

use maclator_core::cpu::Cpu;
use maclator_core::disasm::disasm;
use maclator_core::interp::{self, Exit};
use std::cell::Cell;

thread_local! {
    static EXIT_REQUESTED: Cell<bool> = const { Cell::new(false) };
}

/// Ask the current guest thread to stop after the current instruction.
pub fn exit_current_thread() {
    EXIT_REQUESTED.with(|f| f.set(true));
}

pub fn invalidate_range(_addr: u64, _len: u64) {
    crate::jit::invalidate_range(_addr, _len);
}

pub fn flush_profile() {
    crate::jit::flush_profile();
}

/// Registry of running guest CPUs (for SIGUSR1 diagnostics).
static CPUS: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

thread_local! {
    static CURRENT_CPU: Cell<usize> = const { Cell::new(0) };
}

/// SIGUSR2: print this thread's guest registers without taking any locks
/// (usable in forked children where other threads' locks may be stuck).
extern "C" fn dump_current(_: i32) {
    let p = CURRENT_CPU.with(|c| c.get());
    if p == 0 {
        return;
    }
    let cpu = unsafe { &*(p as *const Cpu) };
    let mut buf = [0u8; 1024];
    unsafe {
        let n = libc::snprintf(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            b"[usr2] pid %d pc=%llx lr=%llx sp=%llx x0=%llx x1=%llx x2=%llx x3=%llx x8=%llx x16=%llx x19=%llx x20=%llx x21=%llx x22=%llx\n\0".as_ptr() as *const libc::c_char,
            libc::getpid(),
            cpu.pc,
            cpu.x[30],
            cpu.x[31],
            cpu.x[0],
            cpu.x[1],
            cpu.x[2],
            cpu.x[3],
            cpu.x[8],
            cpu.x[16],
            cpu.x[19],
            cpu.x[20],
            cpu.x[21],
            cpu.x[22],
        );
        libc::write(2, buf.as_ptr() as *const libc::c_void, n.max(0) as usize);
        let (blocks, ms, interp, entries, fb) = crate::jit::stats_snapshot();
        let n = libc::snprintf(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            b"[stats] pid %d translated=%llu blocks in %llu ms, cold-interpreted=%llu blocks, block-entries=%llu, fallback-insns=%llu\n\0".as_ptr() as *const libc::c_char,
            libc::getpid(),
            blocks,
            ms,
            interp,
            entries,
            fb,
        );
        libc::write(2, buf.as_ptr() as *const libc::c_void, n.max(0) as usize);
    }
}

pub fn run_thread(cpu: &mut Cpu) {
    CURRENT_CPU.with(|c| c.set(cpu as *mut Cpu as usize));
    EXIT_REQUESTED.with(|f| f.set(false));
    let key = cpu as *mut Cpu as usize;
    CPUS.lock().unwrap().push(key);
    crate::jit::run(cpu);
    CPUS.lock().unwrap().retain(|&k| k != key);
}

extern "C" fn dump_threads(_: i32) {
    crate::workq::dump_log();
    // Best effort, racy by design: this is a debugging aid.
    let list = CPUS.try_lock().map(|g| g.clone()).unwrap_or_default();
    for (i, &p) in list.iter().enumerate() {
        let cpu = unsafe { &*(p as *const Cpu) };
        eprintln!("=== guest thread {i}: pc {:#x} {}  lr {}", cpu.pc, crate::symbols::describe(cpu.pc), crate::symbols::describe(cpu.x[30]));
        eprintln!("{}", cpu.dump());
        for i in 0..14u64 {
            let a = cpu.pc.wrapping_sub(10 * 4) + i * 4;
            if let Some(w) = crate::guestmem::read_u64(a & !7) {
                let insn = if a & 4 != 0 { (w >> 32) as u32 } else { w as u32 };
                eprintln!("  {} {:#x}: {:08x}  {}", if a == cpu.pc { "=>" } else { "  " }, a, insn, disasm(insn));
            }
        }
        crate::symbols::backtrace(cpu);
        for r in [0usize, 1, 19, 20, 21, 22] {
            if let Some(v) = crate::guestmem::read_u64(cpu.x[r]) {
                let v2 = crate::guestmem::read_u64(cpu.x[r] + 8).unwrap_or(0);
                eprintln!("  [x{r}] = {:#x} {:#x}", v, v2);
            }
        }
    }
}

pub fn install_debug_handler() {
    unsafe { libc::signal(libc::SIGUSR1, dump_threads as usize) };
    unsafe { libc::signal(libc::SIGUSR2, dump_current as usize) };
    if std::env::var_os("MACLATOR_DUMP_ON_FAULT").is_some() {
        unsafe {
            libc::signal(libc::SIGSEGV, fault_dump as usize);
            libc::signal(libc::SIGBUS, fault_dump as usize);
        }
    }
}

extern "C" fn fault_dump(sig: i32) {
    // Helper processes often have stderr closed or /dev/null: write the dump to a file.
    unsafe {
        let path = format!("/tmp/maclator-fault-{}.log\0", libc::getpid());
        let fd = libc::open(path.as_ptr() as *const libc::c_char, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o644);
        if fd >= 0 {
            libc::dup2(fd, 2);
        }
    }
    eprintln!("maclator: host signal {sig} in emulator; guest state:");
    dump_threads(sig);
    unsafe { libc::_exit(128 + sig) };
}

/// Handle an exit from interpreter/JIT. Returns false when the thread should stop.
pub fn handle_exit(cpu: &mut Cpu, e: Exit) -> bool {
    match e {
        Exit::Svc(_) => {
            crate::syscalls::handle_svc(cpu);
            !EXIT_REQUESTED.with(|f| f.get())
        }
        Exit::IcInvalidate(addr) => {
            crate::jit::invalidate_range(addr & !63, 64);
            true
        }
        Exit::Brk(imm) => {
            crash(cpu, &format!("guest executed BRK #{imm:#x} (trap)"));
            false
        }
        Exit::Udf(insn) => {
            crash(cpu, &format!("guest executed undefined instruction {insn:#010x}"));
            false
        }
        Exit::Unimplemented(insn) => {
            crash(cpu, &format!("unimplemented instruction {insn:#010x}  {}", disasm(insn)));
            false
        }
    }
}

pub fn crash(cpu: &Cpu, why: &str) -> ! {
    eprintln!("\nmaclator: {why}");
    eprintln!("  at pc {:#x} {}", cpu.pc, crate::symbols::describe(cpu.pc));
    eprintln!("{}", cpu.dump());
    eprintln!("  lr {}", crate::symbols::describe(cpu.x[30]));
    let n: u64 = std::env::var("MACLATOR_DISAS").ok().and_then(|v| v.parse().ok()).unwrap_or(12);
    let start = cpu.pc.wrapping_sub(n * 4);
    for i in 0..(n + 4) {
        let a = start + i * 4;
        if let Some(w) = crate::guestmem::read_u64(a & !7) {
            let insn = if a & 4 != 0 { (w >> 32) as u32 } else { w as u32 };
            eprintln!("  {} {:#x}: {:08x}  {}", if a == cpu.pc { "=>" } else { "  " }, a, insn, disasm(insn));
        }
    }
    crate::symbols::backtrace(cpu);
    // Strings materialised by ADRP+ADD just before the crash (crash messages).
    for i in 1..24u64 {
        let a = cpu.pc.wrapping_sub(i * 4);
        let (Some(w1), Some(w2)) = (read_insn(a), read_insn(a + 4)) else { continue };
        if w1 & 0x9F00_0000 == 0x9000_0000 && w2 & 0xFFC0_0000 == 0x9100_0000 && (w1 & 31) == ((w2 >> 5) & 31) {
            let immlo = ((w1 >> 29) & 3) as u64;
            let immhi = ((w1 >> 5) & 0x7ffff) as u64;
            let imm = (((immhi << 2) | immlo) << 43) as i64 >> 31;
            let page = (a & !0xfff).wrapping_add(imm as u64);
            let target = page + ((w2 >> 10) & 0xfff) as u64;
            let mut b = [0u8; 160];
            if crate::guestmem::read(target, &mut b) {
                let len = b.iter().position(|&c| c == 0).unwrap_or(0);
                if len >= 6 && b[..len].iter().all(|&c| (0x20..0x7f).contains(&c)) {
                    eprintln!("  message? \"{}\"", String::from_utf8_lossy(&b[..len]));
                }
            }
        }
    }
    // Registers that look like C strings often carry the crash reason.
    for i in 0..31 {
        let mut b = [0u8; 96];
        if crate::guestmem::read(cpu.x[i], &mut b) {
            let len = b.iter().position(|&c| c == 0).unwrap_or(0);
            if len >= 6 && b[..len].iter().all(|&c| (0x20..0x7f).contains(&c) || c == b'\n') {
                eprintln!("  x{i} -> \"{}\"", String::from_utf8_lossy(&b[..len]));
            }
        }
    }
    std::process::exit(134);
}

fn read_insn(a: u64) -> Option<u32> {
    let w = crate::guestmem::read_u64(a & !7)?;
    Some(if a & 4 != 0 { (w >> 32) as u32 } else { w as u32 })
}

/// Plain interpreter loop (used by `--interp` and as the reference path).
pub fn interp_loop(cpu: &mut Cpu) {
    loop {
        match interp::step(cpu) {
            Ok(()) => {}
            Err(e) => {
                if !handle_exit(cpu, e) {
                    return;
                }
            }
        }
    }
}
