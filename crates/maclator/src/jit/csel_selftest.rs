//! Valid conditional-select chains compared with the reference interpreter.
//! Prefix emission also checks that unsupported conditions and helper fallbacks
//! discard fusion, so a passing differential test cannot hide an unused path.

use super::*;
use super::emit::{FuseKind, StubKind, Unit};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0
    }
}

fn clone_arch(cpu: &Cpu) -> Cpu {
    let mut out = Cpu::default();
    out.x = cpu.x; out.v = cpu.v; out.pc = cpu.pc;
    out.fpcr = cpu.fpcr; out.fpsr = cpu.fpsr;
    out.tpidr_el0 = cpu.tpidr_el0; out.tpidrro_el0 = cpu.tpidrro_el0;
    out.set_nzcv(cpu.nzcv());
    out
}

fn snapshot(cpu: &Cpu) -> Vec<u64> {
    let mut out = cpu.x.to_vec();
    for v in cpu.v { out.extend(v); }
    out.extend([cpu.nzcv() as u64, cpu.fpcr, cpu.fpsr, cpu.tpidr_el0, cpu.tpidrro_el0, cpu.pc]);
    out
}

fn select(sf: bool, mode: u32, cond: u32, rd: u32, rn: u32, rm: u32) -> u32 {
    0x1a80_0000 | ((sf as u32) << 31) | ((mode >> 1) << 30) | ((mode & 1) << 10)
        | (rm << 16) | (cond << 12) | (rn << 5) | rd
}

fn emitted_fusion(words: &[u32], pc: u64) -> (Option<FuseKind>, u64) {
    let mut unit = Unit::new(StubKind::Plain, false);
    unit.allow_loop_cache = false;
    let label = unit.a.create_label();
    unit.labels.insert(pc, label);
    let read = |address: u64| {
        let offset = address.checked_sub(pc)?;
        if offset & 3 != 0 { return None; }
        words.get((offset / 4) as usize).copied()
    };
    emit::emit_block(&mut unit, pc, &read);
    (unit.fuse, unit.insns_fallback)
}

/// 3 flag producers × 2 widths × 16 conditions × 4 chain shapes × 8 edge pairs,
/// followed by `count` random valid chains. There are no skipped invalid encodings.
pub fn run(count: usize) {
    const FIXED: usize = 3 * 2 * 16 * 4 * 8;
    let page = unsafe { libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
    assert_ne!(page, libc::MAP_FAILED);
    let page = page as u64;
    let mut rng = Rng(0xc5e1_90de_1234_abcd);
    let (mut ok, mut bad, mut retained, mut discarded, mut fallback_cases) = (0usize, 0usize, 0usize, 0usize, 0usize);
    for it in 0..FIXED + count {
        let (kind_index, setter_sf, cond, shape, edge) = if it < FIXED {
            let mut n = it;
            let edge = n % 8; n /= 8;
            let shape = n % 4; n /= 4;
            let cond = (n % 16) as u32; n /= 16;
            let setter_sf = n % 2 != 0; n /= 2;
            (n, setter_sf, cond, shape, edge)
        } else {
            (rng.next() as usize % 3, rng.next() & 1 != 0, rng.next() as u32 % 16,
             rng.next() as usize % 4, rng.next() as usize % 8)
        };
        let kind = [FuseKind::Add, FuseKind::Sub, FuseKind::Logic][kind_index];
        let mut init = Cpu::new();
        for n in 0..32 { init.x[n] = rng.next(); init.v[n] = [rng.next(), rng.next()]; }
        init.pc = page;
        init.set_nzcv(rng.next() as u32 & 15);
        if it < FIXED {
            let mask = if setter_sf { u64::MAX } else { u32::MAX as u64 };
            let sign = if setter_sf { 1u64 << 63 } else { 1u64 << 31 };
            let pairs = [(0,0), (mask,1), (sign-1,1), (sign,mask), (sign,sign), (mask,mask), (0,1), (1,2)];
            (init.x[0], init.x[1]) = pairs[edge];
        }
        init.x[4] = init.x[0]; init.x[5] = init.x[1]; init.x[6] = init.x[2];
        let mut prefix = Vec::new();
        if shape != 1 {
            // Repeated source/destination aliases keep these guest values cached.
            for (source, destination) in [(0,4), (1,5), (2,6)] {
                prefix.push(0xaa00_03e0 | (source << 16) | destination);
            }
        }
        let setter = [0x2b00_001f, 0x6b00_001f, 0x6a00_001f][kind_index]
            | ((setter_sf as u32) << 31) | (5 << 16) | (4 << 5);
        prefix.push(setter);
        let select_sf = setter_sf ^ (shape & 1 != 0);
        prefix.push(select(select_sf, 0, cond, 6, 4, 5));
        if shape == 2 {
            // Valid MRS NZCV is deliberately interpreted: a helper call destroys
            // host flags even though this guest instruction preserves NZCV.
            prefix.push(0xd53b_421a); // MRS X26, NZCV
            fallback_cases += 1;
        } else if shape == 3 {
            prefix.push(select(!select_sf, 0, 14 + (edge as u32 & 1), 9, 6, 5));
        }
        prefix.push(select(!select_sf, 1, cond, 4, 6, 4));
        prefix.push(select(select_sf, 2, cond, if shape == 1 { 31 } else { 5 }, 4, 5));
        prefix.push(select(!select_sf, 3, cond, 6, 5, 6));
        prefix.push(select(select_sf, 1, cond, 7, 31, 31)); // CSET alias
        prefix.push(select(!select_sf, 0, cond, 8, 7, 6));
        let want_fusion = emit::csel_chain() && shape < 2 && emit::x86_cc(kind, cond).is_some();
        let (actual_fusion, actual_fallbacks) = emitted_fusion(&prefix, page);
        assert_eq!(actual_fusion, if want_fusion { Some(kind) } else { None },
            "incorrect emitted CSEL fusion kind={kind:?} cond={cond} shape={shape}");
        assert_eq!(actual_fallbacks, if shape == 2 { 1 } else { 0 }, "unexpected CSEL helper path");
        if want_fusion { retained += 1; } else { discarded += 1; }
        let mut program = prefix;
        program.push(0x5400_0000 | (2 << 5) | cond); // final B.cond +8 consumes the same NZCV
        program.push(0xd280_003b); // MOV X27, #1 only on the not-taken path
        program.push(0xd400_1001); // SVC #0x80
        for (index, &word) in program.iter().enumerate() {
            unsafe { *((page as *mut u32).add(index)) = word; }
        }
        let mut reference = clone_arch(&init);
        let mut reached_svc = false;
        for _ in 0..program.len() + 1 {
            let index = ((reference.pc - page) / 4) as usize;
            let word = *program.get(index).expect("reference CSEL PC inside fixture");
            match interp::exec(&mut reference, word) {
                Ok(()) => {}
                Err(interp::Exit::Svc(_)) => { reached_svc = true; break; }
                Err(exit) => panic!("invalid CSEL fixture: {exit:?}"),
            }
        }
        assert!(reached_svc, "reference CSEL fixture did not terminate");
        invalidate_range(page, program.len() as u64 * 4);
        let mut translated = clone_arch(&init);
        setup_cpu(&mut translated);
        PENDING.with(|pending| pending.take());
        let mut reached_jit_svc = false;
        let mut pending_exit = false;
        for _ in 0..4 {
            let host = lookup_or_translate(translated.pc);
            unsafe { maclator_jit_enter(&mut translated, host) };
            if PENDING.with(|pending| pending.take()).is_some() { pending_exit = true; break; }
            if translated.exit_reason == EXIT_SVC { reached_jit_svc = true; break; }
            if translated.exit_reason != EXIT_CONTINUE { break; }
        }
        if !reached_jit_svc || pending_exit || snapshot(&reference) != snapshot(&translated) {
            bad += 1;
            if bad <= 12 {
                println!("MISMATCH cselchain #{it} kind={kind:?} setter_sf={setter_sf} cond={cond} shape={shape} words={program:08x?}");
                println!("    reference={:x?} translated={:x?}", snapshot(&reference), snapshot(&translated));
            }
        } else { ok += 1; }
    }
    unsafe { libc::munmap(page as *mut _, 4096); }
    println!("jit selftest [cselchain]: ok={ok} mismatch={bad} fixed={FIXED} random={count} retained={retained} discarded={discarded} fallback_cases={fallback_cases} enabled={}", emit::csel_chain());
    assert_eq!(bad, 0, "conditional-select chain differential mismatch");
}
