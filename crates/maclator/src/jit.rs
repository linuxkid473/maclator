//! Translation engine (placeholder until the x86-64 backend lands).

use maclator_core::cpu::Cpu;

pub fn run(cpu: &mut Cpu) {
    crate::engine::interp_loop(cpu);
}

pub fn invalidate_range(_addr: u64, _len: u64) {}

pub fn flush_profile() {}
