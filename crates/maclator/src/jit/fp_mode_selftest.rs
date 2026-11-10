//! Non-default scalar FPCR differential tests. The core helper has independent
//! analytical / hardware rounding tests; this suite verifies that generated
//! code actually dispatches to it and preserves caches, FPSR and aliases.

use super::*;
use maclator_core::disasm::disasm;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn clone_arch(cpu: &Cpu) -> Cpu {
    let mut out = Cpu::default();
    out.x = cpu.x;
    out.v = cpu.v;
    out.pc = cpu.pc;
    out.fpcr = cpu.fpcr;
    out.fpsr = cpu.fpsr;
    out.tpidr_el0 = cpu.tpidr_el0;
    out.tpidrro_el0 = cpu.tpidrro_el0;
    out.set_nzcv(cpu.nzcv());
    out
}

fn snapshot(cpu: &Cpu) -> Vec<u64> {
    let mut out = cpu.x.to_vec();
    for v in cpu.v { out.extend(v); }
    out.extend([cpu.nzcv() as u64, cpu.fpcr, cpu.fpsr, cpu.tpidr_el0, cpu.tpidrro_el0]);
    out
}

/// Integrate as `--selftest-jit fp-modes [count]`. Count adds random cases to a
/// deterministic 32768-case edge matrix; a mismatch fails the process.
pub fn run(count: usize) {
    const EDGE64: [u64; 16] = [
        0, 1 << 63, 1, (1 << 63) | 1,
        0x0010_0000_0000_0000, 0x8010_0000_0000_0000,
        0x3ff0_0000_0000_0000, 0xbff0_0000_0000_0000,
        0x3fef_ffff_ffff_ffff, 0x7fef_ffff_ffff_ffff,
        0x7ff0_0000_0000_0000, 0xfff0_0000_0000_0000,
        0x7ff8_0000_0000_0123, 0xfff8_0000_0000_0456,
        0x7ff0_0000_0000_0789, 0xfff0_0000_0000_0abc,
    ];
    const EDGE32: [u64; 16] = [
        0, 1 << 31, 1, (1 << 31) | 1, 0x0080_0000, 0x8080_0000,
        0x3f80_0000, 0xbf80_0000, 0x3f7f_ffff, 0x7f7f_ffff,
        0x7f80_0000, 0xff80_0000, 0x7fc0_0123, 0xffc0_0456,
        0x7f80_0789, 0xff80_0abc,
    ];
    const CONTROL: [u64; 8] = [
        1 << 25, 1 << 22, 2 << 22, 3 << 22, 1 << 24,
        (1 << 24) | (1 << 25) | (1 << 22),
        (1 << 24) | (1 << 25) | (2 << 22),
        (1 << 24) | (1 << 25) | (3 << 22),
    ];
    const OPCODES: [u32; 8] = [0, 2, 3, 4, 5, 6, 7, 8];
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0)
    };
    assert_ne!(page, libc::MAP_FAILED);
    let page = page as u64;
    let edges = 16 * 16 * CONTROL.len() * OPCODES.len() * 2;
    let (mut ok, mut bad, mut default_fast) = (0usize, 0usize, 0usize);
    let mut rng = Rng(0xf0c2_339a_7832_471f);
    for it in 0..edges + count {
        let double = it % 2 != 0;
        let opcode = OPCODES[it / 2 % OPCODES.len()];
        let control = CONTROL[it / 16 % CONTROL.len()];
        let (rn, rm, rd) = if it < edges {
            (0, 1, [0, 1, 2, 31][it / 128 % 4])
        } else {
            (rng.next() as u32 % 32, rng.next() as u32 % 32, rng.next() as u32 % 32)
        };
        let mut insn = 0x1e20_0800 | ((double as u32) << 22) | (rm << 16)
            | (opcode << 12) | (rn << 5) | rd;
        let mut init = Cpu::new();
        for i in 0..32 { init.x[i] = rng.next(); init.v[i] = [rng.next(), rng.next()]; }
        init.pc = page;
        init.fpcr = control;
        init.fpsr = rng.next() & 0xf800_009f;
        init.set_nzcv(rng.next() as u32 & 15);
        if it < edges {
            let edge = if double { &EDGE64 } else { &EDGE32 };
            let pair = it / 128;
            init.v[0][0] = edge[pair % 16];
            init.v[1][0] = edge[pair / 16 % 16];
        } else if it % 5 == 0 {
            // FMA non-default tests include aliases among every vector register.
            let ra = rng.next() as u32 % 32;
            let mode = rng.next() as u32 % 4;
            insn = 0x1f00_0000 | ((double as u32) << 22) | ((mode / 2) << 21)
                | (rm << 16) | ((mode % 2) << 15) | (ra << 10) | (rn << 5) | rd;
        } else if it % 5 == 1 && matches!(opcode, 0 | 2 | 3) {
            // Include the hardware fast path as a control. Finite normal inputs
            // avoid asking the old default-mode contract to emulate FPSR/NaNs.
            init.fpcr = 0;
            init.v[rn as usize][0] = if double { 0x3ff0_0000_0000_0001 } else { 0x3f80_0001 };
            init.v[rm as usize][0] = if double { 0x4000_0000_0000_0001 } else { 0x4000_0001 };
            default_fast += 1;
        }
        // Fill and reuse the GPR cache around both paths, and consume the scalar
        // output with FMOV so missing vector-cache reloads cannot hide in memory.
        let to_gpr = (if double { 0x9e66_0000 } else { 0x1e26_0000 }) | (rd << 5) | 10;
        let program = [0xaa0c_03e0, 0xaa0d_03e1, 0xaa0e_03e2, insn,
            to_gpr, 0x8b01_0003, 0x9b02_7c64, 0x6b00_0085];
        let mut reference = clone_arch(&init);
        for &word in &program { interp::exec(&mut reference, word).expect("valid scalar FPCR/cache program"); }
        unsafe {
            for (i, &word) in program.iter().enumerate() { *((page as *mut u32).add(i)) = word; }
            *((page as *mut u32).add(program.len())) = 0xd400_1001;
        }
        invalidate_range(page, ((program.len() + 1) * 4) as u64);
        let mut translated = clone_arch(&init);
        setup_cpu(&mut translated);
        let host = lookup_or_translate(page);
        unsafe { maclator_jit_enter(&mut translated, host); }
        let pending = PENDING.with(|p| p.take());
        let a = snapshot(&reference);
        let b = snapshot(&translated);
        if a != b || translated.exit_reason != EXIT_SVC || pending.is_some() {
            bad += 1;
            if bad <= 12 {
                println!("MISMATCH FPCR {insn:08x} {} fpcr={:#x} n={:x} m={:x}",
                    disasm(insn), init.fpcr, init.v[rn as usize][0], init.v[rm as usize][0]);
                for i in 0..a.len() { if a[i] != b[i] { println!("    slot {i}: reference={:x} jit={:x}", a[i], b[i]); } }
            }
        } else { ok += 1; }
    }
    unsafe { libc::munmap(page as *mut _, 4096); }
    println!("jit selftest [fp-modes]: ok={ok} mismatch={bad} edges={edges} random={count} default_fast={default_fast}");
    assert_eq!(bad, 0, "scalar FPCR differential mismatch");
}
