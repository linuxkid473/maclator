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

pub fn run_thread(cpu: &mut Cpu) {
    EXIT_REQUESTED.with(|f| f.set(false));
    crate::jit::run(cpu);
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
