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
    std::process::exit(134);
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
