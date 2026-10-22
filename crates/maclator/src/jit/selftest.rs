//! JIT self-test: translate single instructions and compare the result with
//! the reference interpreter (`maclator --selftest-jit [class] [count]`).

use super::*;
use maclator_core::disasm::disasm;
use super::emit::DEAD_FLAG_SKIPS;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn val(&mut self) -> u64 {
        match self.next() % 8 {
            0 => 0,
            1 => u64::MAX,
            2 => self.next() % 64,
            3 => 1u64 << (self.next() % 64),
            4 => self.next() as u32 as u64,
            5 => 0x8000_0000_0000_0000 | (self.next() % 5),
            _ => self.next(),
        }
    }
}

fn snapshot(c: &Cpu) -> Vec<u64> {
    let mut v: Vec<u64> = c.x.to_vec();
    v.push(c.nzcv() as u64);
    for i in 0..32 {
        v.push(c.v[i][0]);
        v.push(c.v[i][1]);
    }
    v.push(c.tpidr_el0);
    v
}

/// Pairs of (flag-setting instruction, flag consumer) to exercise compare-and-branch / select fusion.
fn run_selftest_fuse(count: usize, deep: bool) {
    // (mask, value) of the flag-setting encodings: add/sub imm, shifted reg, extended reg (all with S),
    // logical imm, logical shifted reg (ANDS/BICS).
    const SETTERS: [(u32, u32); 5] = [
        (0x3F80_0000, 0x3100_0000),
        (0x3F20_0000, 0x2B00_0000),
        (0x3F20_0000, 0x2B20_0000),
        (0x7F80_0000, 0x7200_0000),
        (0x7F00_0000, 0x6A00_0000),
    ];
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x1000, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64
    };
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let (mut ok, mut bad, mut skipped, mut native, mut fused) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for it in 0..count {
        let (mask, val) = SETTERS[(rng.next() % SETTERS.len() as u64) as usize];
        let setter = (rng.next() as u32 & !mask) | val;
        let branch = deep || it % 2 == 0;
        let consumer: u32 = if branch {
            // b.cond +8 (skips one instruction)
            0x5400_0000 | (2 << 5) | (rng.next() as u32 % 14)
        } else {
            let base = 0x1A80_0000u32; // csel family
            let f = rng.next() as u32;
            (f & !0x3FE0_0800 & !0x0000_0000) | base | (f & 0xC000_0000) | (f & 0x0000_0400)
        };
        if !branch && (consumer & 0x3FE0_0800) != 0x1A80_0000 {
            skipped += 1;
            continue;
        }
        let mut init = Cpu::new();
        for i in 0..31 {
            init.x[i] = rng.val();
        }
        init.x[31] = rng.val();
        init.set_nzcv((rng.next() % 16) as u32);
        init.pc = page;
        let mut r = Cpu::new();
        *r = clone_cpu(&init);
        // deep: [setter, b.cond +8, A, B, svc] with A and B overwriting all flags
        let (wa, wb) = {
            let (m1, v1) = SETTERS[(rng.next() % SETTERS.len() as u64) as usize];
            let (m2, v2) = SETTERS[(rng.next() % SETTERS.len() as u64) as usize];
            ((rng.next() as u32 & !m1) | v1, (rng.next() as u32 & !m2) | v2)
        };
        unsafe {
            *(page as *mut u32) = setter;
            *((page + 4) as *mut u32) = consumer;
            if deep {
                *((page + 8) as *mut u32) = wa;
                *((page + 12) as *mut u32) = wb;
                *((page + 16) as *mut u32) = 0xD400_1001;
            } else {
                *((page + 8) as *mut u32) = 0xD400_1001;
                *((page + 12) as *mut u32) = 0xD400_1001;
            }
        }
        if deep {
            // reference: run to the SVC
            let mut steps = 0;
            let mut bad_ref = false;
            r.pc = page;
            loop {
                let insn = unsafe { *(r.pc as *const u32) };
                match interp::exec(&mut r, insn) {
                    Ok(()) => {}
                    Err(interp::Exit::Svc(_)) => break,
                    Err(_) => { bad_ref = true; break; }
                }
                steps += 1;
                if steps > 8 { bad_ref = true; break; }
            }
            if bad_ref {
                skipped += 1;
                continue;
            }
        } else {
            if interp::exec(&mut r, setter).is_err() {
                skipped += 1;
                continue;
            }
            r.pc = page + 4;
            if interp::exec(&mut r, consumer).is_err() {
                skipped += 1;
                continue;
            }
        }
        invalidate_range(page, 32);
        let mut j = Cpu::new();
        *j = clone_cpu(&init);
        setup_cpu(&mut j);
        let fb0 = STATS_FALLBACK.load(Ordering::Relaxed);
        let host = lookup_or_translate(page);
        if STATS_FALLBACK.load(Ordering::Relaxed) == fb0 {
            native += 1;
        }
        unsafe { maclator_jit_enter(&mut *j, host) };
        let mut hops = 0;
        while deep && j.exit_reason != EXIT_SVC && j.exit_reason != EXIT_PENDING && hops < 6 {
            let h2 = lookup_or_translate(j.pc);
            unsafe { maclator_jit_enter(&mut *j, h2) };
            hops += 1;
        }
        let pending = PENDING.with(|p| p.take());
        if j.exit_reason == EXIT_PENDING || pending.is_some() {
            skipped += 1;
            continue;
        }
        let mut a = snapshot(&r);
        let mut b = snapshot(&j);
        if branch && !deep {
            a.push(r.pc);
            b.push(j.pc);
        }
        if a != b || ((!branch || deep) && j.exit_reason != EXIT_SVC) {
            bad += 1;
            if bad <= 20 {
                println!("MISMATCH {:08x} {} ; {:08x} {}", setter, disasm(setter), consumer, disasm(consumer));
                for i in 0..a.len() {
                    if a[i] != b[i] {
                        println!("    slot {i}: interp={:#x} jit={:#x}", a[i], b[i]);
                    }
                }
            }
        } else {
            ok += 1;
            fused += 1;
        }
    }
    println!("jit selftest [fuse{}]: ok={ok} mismatch={bad} skipped={skipped} native={native} pairs={fused} dead_flag_skips={}", if deep { "2" } else { "" }, DEAD_FLAG_SKIPS.load(Ordering::Relaxed));
}

/// Random straight-line blocks over a small register set (so values are reused constantly): exercises
/// the JIT's register cache and flag fusion across instructions. Memory operations use x20-x23 as
/// bases, which no other instruction may write except through their own writeback.
fn run_selftest_blocks(count: usize) {
    const CLASSES: [(u32, u32); 6] = [
        (0x1C00_0000, 0x1000_0000), // dp immediate
        (0x0E00_0000, 0x0A00_0000), // dp register
        (0x0E00_0000, 0x0A00_0000),
        (0x3B00_0000, 0x3900_0000), // load/store unsigned offset
        (0x3B20_0C00, 0x3800_0400), // post-index
        (0x3B20_0C00, 0x3800_0C00), // pre-index
    ];
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x1000, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64
    };
    let mut buf = vec![0u8; 1 << 20];
    let bufp = buf.as_mut_ptr() as u64;
    let mut rng = Rng(0x0dd_ba11_f00d_cafe);
    let (mut ok, mut bad, mut skipped, mut insns, mut fallbacks) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for it in 0..count {
        let len = 2 + (rng.next() % 10) as usize;
        let mut prog: Vec<u32> = Vec::new();
        let mut tries = 0;
        while prog.len() < len && tries < 200 {
            tries += 1;
            let (mask, val) = CLASSES[(rng.next() % CLASSES.len() as u64) as usize];
            let mut insn = (rng.next() as u32 & !mask) | val;
            let mem = mask != 0x1C00_0000 && mask != 0x0E00_0000;
            // small register set: rd, rn, rm in 0..6 (rn in 20..23 for memory operations)
            insn = (insn & !31) | (rng.next() as u32 % 6);
            if mem {
                insn = (insn & !(31 << 5)) | ((20 + rng.next() as u32 % 4) << 5);
                let rt2 = (rng.next() as u32) % 6;
                if insn & 0x3A00_0000 == 0x2800_0000 {
                    insn = (insn & !(31 << 10)) | (rt2 << 10);
                }
            } else {
                insn = (insn & !(31 << 5)) | ((rng.next() as u32 % 6) << 5);
                insn = (insn & !(31 << 16)) | ((rng.next() as u32 % 6) << 16);
            }
            if emit::is_terminator(insn) {
                continue;
            }
            prog.push(insn);
        }
        let mut init = Cpu::new();
        for i in 0..31 {
            init.x[i] = rng.val();
        }
        for i in 20..24 {
            init.x[i] = bufp + 0x40000 + (rng.next() % 0x60000 & !15);
        }
        init.x[31] = bufp + 0x80000;
        init.set_nzcv((rng.next() % 16) as u32);
        for i in 0..32 {
            init.v[i] = [rng.val(), rng.val()];
        }
        init.pc = page;
        let mem0: Vec<u8> = buf.clone();
        let mut r = Cpu::new();
        *r = clone_cpu(&init);
        let mut valid = true;
        for (i, w) in prog.iter().enumerate() {
            r.pc = page + 4 * i as u64;
            if interp::exec(&mut r, *w).is_err() {
                valid = false;
                break;
            }
        }
        if !valid {
            buf.copy_from_slice(&mem0);
            skipped += 1;
            continue;
        }
        let mem_ref = buf.clone();
        buf.copy_from_slice(&mem0);
        unsafe {
            for (i, w) in prog.iter().enumerate() {
                *((page + 4 * i as u64) as *mut u32) = *w;
            }
            *((page + 4 * prog.len() as u64) as *mut u32) = 0xD400_1001;
        }
        invalidate_range(page, (4 * prog.len() + 4) as u64);
        let mut j = Cpu::new();
        *j = clone_cpu(&init);
        setup_cpu(&mut j);
        let fb0 = STATS_FALLBACK.load(Ordering::Relaxed);
        let host = lookup_or_translate(page);
        fallbacks += STATS_FALLBACK.load(Ordering::Relaxed) - fb0;
        unsafe { maclator_jit_enter(&mut *j, host) };
        let pending = PENDING.with(|p| p.take());
        if j.exit_reason == EXIT_PENDING || pending.is_some() {
            skipped += 1;
            buf.copy_from_slice(&mem0);
            continue;
        }
        let a = snapshot(&r);
        let b = snapshot(&j);
        let mem_diff = buf != mem_ref;
        if a != b || mem_diff || j.exit_reason != EXIT_SVC {
            bad += 1;
            if bad <= 15 {
                println!("MISMATCH block #{it}:");
                for w in &prog {
                    println!("    {:08x} {}", w, disasm(*w));
                }
                for i in 0..a.len() {
                    if a[i] != b[i] {
                        let name = if i < 32 { format!("x{i}") } else if i == 32 { "nzcv".into() } else { format!("slot{i}") };
                        println!("    {name}: interp={:#x} jit={:#x}", a[i], b[i]);
                    }
                }
                if mem_diff {
                    println!("    memory differs");
                }
                if j.exit_reason != EXIT_SVC {
                    println!("    exit reason {}", j.exit_reason);
                }
            }
        } else {
            ok += 1;
            insns += prog.len() as u64;
        }
        buf.copy_from_slice(&mem0);
    }
    println!("jit selftest [blocks]: ok={ok} mismatch={bad} skipped={skipped} insns={insns} fallback_insns={fallbacks}");
}

/// CBZ/TBZ after a register write, including cached values, W/X width, all bit positions and ZR.
/// Compare guest flags as well as the successor PC: testing a host flag must not alter ARM NZCV.
fn run_selftest_branchcache(count: usize) {
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x1000, libc::PROT_READ | libc::PROT_WRITE,
                   libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64
    };
    let mut rng = Rng(0xcafe_1234_5678_beef);
    let (mut ok, mut bad, mut native) = (0u64, 0u64, 0u64);
    for it in 0..count {
        let rt = rng.next() as u32 % 32;
        let rm = rng.next() as u32 % 32;
        let nz = (rng.next() as u32 & 1) << 24;
        let cached = it % 2 == 0;
        let setter = 0xAA00_03E0 | (rm << 16) | rt; // MOV Xrt, Xrm; rt=31 discards
        let branch = if it % 4 < 2 {
            let sf = (rng.next() as u32 & 1) << 31;
            0x3400_0000 | sf | nz | (3 << 5) | rt // CBZ/CBNZ +12
        } else {
            let b = rng.next() as u32 % 64;
            0x3600_0000 | ((b >> 5) << 31) | ((b & 31) << 19) | nz | (3 << 5) | rt
        };
        let mut init = Cpu::new();
        for i in 0..32 { init.x[i] = rng.val(); }
        init.set_nzcv((rng.next() % 16) as u32);
        init.pc = page;
        let mut r = clone_cpu(&init);
        if cached { interp::exec(&mut r, setter).unwrap(); }
        let branch_pc = page + if cached { 4 } else { 0 };
        r.pc = branch_pc;
        interp::exec(&mut r, branch).unwrap();
        unsafe {
            *(page as *mut u32) = if cached { setter } else { branch };
            *((page + 4) as *mut u32) = if cached { branch } else { 0xD400_1001 };
            *((page + 8) as *mut u32) = 0xD400_1001;
            *((page + 12) as *mut u32) = 0xD400_1001;
            *((page + 16) as *mut u32) = 0xD400_1001;
        }
        invalidate_range(page, 20);
        let mut j = Cpu::new();
        *j = clone_cpu(&init);
        setup_cpu(&mut j);
        let fb0 = STATS_FALLBACK.load(Ordering::Relaxed);
        let host = lookup_or_translate(page);
        if STATS_FALLBACK.load(Ordering::Relaxed) == fb0 { native += 1; }
        unsafe { maclator_jit_enter(&mut *j, host) };
        let pending = PENDING.with(|p| p.take());
        if snapshot(&r) != snapshot(&j) || r.pc != j.pc || pending.is_some()
            || j.exit_reason != EXIT_CONTINUE
        {
            bad += 1;
            if bad <= 20 {
                println!("MISMATCH branchcache #{it} cached={cached} {branch:08x} {} interp_pc={:#x} jit_pc={:#x}",
                         disasm(branch), r.pc, j.pc);
            }
        } else { ok += 1; }
    }
    unsafe { libc::munmap(page as *mut libc::c_void, 0x1000); }
    println!("jit selftest [branchcache]: ok={ok} mismatch={bad} native={native}");
}

/// Differential scalar FMA tests, including NaN priority, signed zero, aliases,
/// upper-lane clearing and the guarded non-default-FPCR path. `native` is a
/// feature switch, not a claim that every operand takes the hardware path.
fn run_selftest_fma(count: usize) {
    const EDGE64: [u64; 16] = [
        0, 0x8000_0000_0000_0000, 1, 0x8000_0000_0000_0001,
        0x0010_0000_0000_0000, 0x8010_0000_0000_0000,
        0x3ff0_0000_0000_0000, 0xbff0_0000_0000_0000,
        0x3ff0_0000_0000_0001, 0x7fef_ffff_ffff_ffff,
        0x7ff0_0000_0000_0000, 0xfff0_0000_0000_0000,
        0x7ff8_0000_0000_0123, 0xfff8_0000_0000_0456,
        0x7ff0_0000_0000_0789, 0xfff0_0000_0000_0abc,
    ];
    const EDGE32: [u64; 16] = [
        0, 0x8000_0000, 1, 0x8000_0001, 0x0080_0000, 0x8080_0000,
        0x3f80_0000, 0xbf80_0000, 0x3f80_0001, 0x7f7f_ffff,
        0x7f80_0000, 0xff80_0000, 0x7fc0_0123, 0xffc0_0456,
        0x7f80_0789, 0xff80_0abc,
    ];
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x1000, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64
    };
    assert_ne!(page, libc::MAP_FAILED as u64);
    let mut rng = Rng(0xf00d_faa0_1234_5678u64);
    let edges = EDGE64.len().pow(3) * 8;
    let (mut ok, mut bad) = (0usize, 0usize);
    for it in 0..edges + count {
        let double = it % 8 >= 4;
        let mode = it % 4;
        let (rn, rm, ra, rd) = if it < edges {
            (0u32, 1u32, 2u32, (it / 8 % 4) as u32)
        } else {
            (rng.next() as u32 % 32, rng.next() as u32 % 32,
             rng.next() as u32 % 32, rng.next() as u32 % 32)
        };
        let insn = 0x1f00_0000 | ((double as u32) << 22) | (((mode / 2) as u32) << 21)
            | (rm << 16) | (((mode % 2) as u32) << 15) | (ra << 10) | (rn << 5) | rd;
        let mut init = Cpu::new();
        for i in 0..32 {
            init.x[i] = rng.next();
            init.v[i] = [rng.next(), rng.next()];
        }
        if it < edges {
            let e = if double { &EDGE64 } else { &EDGE32 };
            let n = it / 8;
            init.v[0][0] = e[n % 16];
            init.v[1][0] = e[n / 16 % 16];
            init.v[2][0] = e[n / 256 % 16];
        }
        init.pc = page;
        init.set_nzcv(rng.next() as u32 & 15);
        init.fpsr = rng.next() & 0xf800_009f;
        if it >= edges && it % 3 == 0 {
            init.fpcr = [1 << 22, 1 << 23, 1 << 24, 1 << 25][it % 4];
        }
        // Cached GPRs must survive both the fast and interpreter-FMA paths.
        // The suffix consumes them, includes aliases and a W write over X bits.
        let program = [0xaa0c_03e0, 0xaa0d_03e1, 0xaa0e_03e2, insn,
            0x8b01_0003, 0x9b02_7c64, 0x6b00_0085];
        let mut r = clone_cpu(&init);
        for &word in &program { interp::exec(&mut r, word).expect("valid FMA/cache test"); }
        unsafe {
            for (i, &word) in program.iter().enumerate() { *((page as *mut u32).add(i)) = word; }
            *((page as *mut u32).add(program.len())) = 0xd400_1001;
        }
        invalidate_range(page, (program.len() as u64 + 1) * 4);
        let mut j = clone_cpu(&init);
        setup_cpu(&mut j);
        let host = lookup_or_translate(page);
        unsafe { maclator_jit_enter(&mut j, host) };
        let pending = PENDING.with(|p| p.take());
        let mut a = snapshot(&r);
        let mut b = snapshot(&j);
        a.extend([r.fpcr, r.fpsr]);
        b.extend([j.fpcr, j.fpsr]);
        if a != b || j.exit_reason != EXIT_SVC || pending.is_some() {
            bad += 1;
            if bad <= 20 {
                println!("MISMATCH FMA {insn:08x} {} fpcr={:#x}", disasm(insn), init.fpcr);
                println!("    n={:x} m={:x} a={:x} ref={:x?} jit={:x?}",
                    init.v[rn as usize][0], init.v[rm as usize][0], init.v[ra as usize][0],
                    r.v[rd as usize], j.v[rd as usize]);
            }
        } else { ok += 1; }
    }
    unsafe { libc::munmap(page as *mut _, 0x1000); }
    println!("jit selftest [fma]: ok={ok} mismatch={bad} edge_cases={edges} random={count} native_fma={}", emit::native_fma());
    assert_eq!(bad, 0, "FMA differential mismatch");
}

/// Valid unshifted ALU blocks deliberately exceed the five-slot cache, then
/// reuse/overwrite operands. Covers eviction, aliases, W upper bits and XZR.
fn run_selftest_operandcache(count: usize) {
    let page = unsafe { libc::mmap(std::ptr::null_mut(), 0x1000, libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64 };
    assert_ne!(page, libc::MAP_FAILED as u64);
    let mut rng = Rng(0xdeaf_cafe_0987_6543);
    let mut bad = 0;
    for _ in 0..count {
        let mut init = Cpu::new();
        for i in 0..32 { init.x[i] = rng.val(); init.v[i] = [rng.next(), rng.next()]; }
        init.set_nzcv(rng.next() as u32 & 15);
        init.pc = page;
        let mut program = Vec::new();
        for rd in 0..8 { program.push(0xaa00_03e0 | ((8 + rng.next() as u32 % 24) << 16) | rd); }
        for _ in 0..12 {
            let sf = rng.next() as u32 % 2;
            let mut reg = || if rng.next() % 8 == 0 { 31 } else { rng.next() as u32 % 8 };
            let (rn, rm, rd, ra) = (reg(), reg(), reg(), reg());
            let insn = match rng.next() % 3 {
                0 => 0x0b00_0000 | ((rng.next() as u32 % 4) << 29), // ADD/SUB, flags optional
                1 => 0x0a00_0000 | ((rng.next() as u32 % 4) << 29), // AND/ORR/EOR/ANDS
                _ => 0x1b00_0000 | ((rng.next() as u32 % 2) << 15) | (ra << 10),
            } | (sf << 31) | (rm << 16) | (rn << 5) | rd;
            program.push(insn);
        }
        let mut reference = clone_cpu(&init);
        for &insn in &program { interp::exec(&mut reference, insn).unwrap(); }
        unsafe {
            for (i, &word) in program.iter().enumerate() { *((page as *mut u32).add(i)) = word; }
            *((page as *mut u32).add(program.len())) = 0xd400_1001;
        }
        invalidate_range(page, (program.len() as u64 + 1) * 4);
        let mut j = clone_cpu(&init);
        setup_cpu(&mut j);
        let host = lookup_or_translate(page);
        unsafe { maclator_jit_enter(&mut j, host) };
        if snapshot(&reference) != snapshot(&j) || j.exit_reason != EXIT_SVC || PENDING.with(|p| p.take()).is_some() {
            bad += 1;
            if bad <= 10 { println!("MISMATCH operandcache {program:08x?}"); }
        }
    }
    unsafe { libc::munmap(page as *mut _, 0x1000); }
    println!("jit selftest [operandcache]: ok={} mismatch={bad} blocks={count} direct_operands={}", count - bad, emit::direct_operands());
    assert_eq!(bad, 0, "cached operand differential mismatch");
}

pub fn run_selftest(class: &str, count: usize) {
    if class == "budgetflags" { return super::budget_flags_selftest::run(); }
    if class == "cselchain" { return super::csel_selftest::run(count); }
    if class == "fp-modes" { return super::fp_mode_selftest::run(count); }
    if class == "loopcache" { return super::loop_selftest::run(count); }
    if class == "operandcache" {
        return run_selftest_operandcache(count);
    }
    if class == "fma" {
        return run_selftest_fma(count);
    }
    if class == "branchcache" {
        return run_selftest_branchcache(count);
    }
    if class == "fuse" {
        return run_selftest_fuse(count, false);
    }
    if class == "fuse2" {
        return run_selftest_fuse(count, true);
    }
    if class == "blocks" {
        return run_selftest_blocks(count);
    }
    let (mask, val): (u32, u32) = match class {
        "dpimm" => (0x1C00_0000, 0x1000_0000),
        "dpreg" => (0x0E00_0000, 0x0A00_0000),
        "ldst" => (0x0A00_0000, 0x0800_0000),
        "branch" => (0x1C00_0000, 0x1400_0000),
        other => {
            let p: Vec<&str> = other.split(':').collect();
            (u32::from_str_radix(p[0], 16).unwrap(), u32::from_str_radix(p[1], 16).unwrap())
        }
    };
    let is_mem = class == "ldst" || std::env::var_os("MACLATOR_SELFTEST_MEM").is_some();
    // The instruction under test plus an svc terminator live at pc. PC-relative
    // literal loads (`ldr ..., $±imm19<<2`) can reach ±1MB from pc, so centre pc
    // in a 4MB window: every literal target stays mapped (zeroed) and the result
    // no longer depends on whatever happens to be mapped around a 4K page
    // (unmapped -> SIGSEGV on some hosts, garbage -> spurious mismatches).
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x400000, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64
    };
    let pc = page + 0x200000;
    let mut buf = vec![0u8; 1 << 20];
    let bufp = buf.as_mut_ptr() as u64;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut ok, mut bad, mut skipped) = (0, 0, 0);
    let mut native = 0u64;
    for _ in 0..count {
        let mut insn = (rng.next() as u32 & !mask) | val;
        let branch = class == "branch";
        if emit::is_terminator(insn) != branch || (branch && insn & 0xFF00_0000 == 0xD400_0000) {
            skipped += 1;
            continue;
        }
        if is_mem && (insn >> 5) & 31 == 31 {
            insn &= !(31 << 5);
        }
        // This randomized harness models data accesses inside buf, not a
        // +/-1 MiB PC-relative literal pool. Reading arbitrary literal targets
        // before code initialization could fault in the reference itself.
        if is_mem && insn & 0x3B00_0000 == 0x1800_0000 {
            skipped += 1;
            continue;
        }
        // Reference first (it tells us whether the encoding is valid).
        let mut init = Cpu::new();
        for i in 0..31 {
            init.x[i] = if is_mem { bufp + 0x40000 + (rng.next() % 0x60000 & !15) } else { rng.val() };
        }
        init.x[31] = bufp + 0x80000;
        if is_mem && insn & 0x3B20_0C00 == 0x3820_0800 {
            // register offset: keep the index small so the address stays in the buffer
            let rm = ((insn >> 16) & 31) as usize;
            let rn = ((insn >> 5) & 31) as usize;
            if rm != rn && rm < 31 {
                init.x[rm] = rng.next() % 512;
            } else {
                skipped += 1;
                continue;
            }
        }
        // SIMD structure loads/stores with register post-increment
        if is_mem && insn & 0xBF00_0000 == 0x0C00_0000 && std::env::var_os("MACLATOR_SELFTEST_SIMDMEM").is_none() {
            skipped += 1;
            continue;
        }
        init.set_nzcv((rng.next() % 16) as u32);
        for i in 0..32 {
            init.v[i] = [rng.val(), rng.val()];
        }
        init.pc = pc;
        let mem0: Vec<u8> = buf.clone();
        let mut r = Cpu::new();
        *r = clone_cpu(&init);
        match interp::exec(&mut r, insn) {
            Ok(()) => {}
            Err(_) => {
                skipped += 1;
                continue;
            }
        }
        let mem_ref = buf.clone();
        buf.copy_from_slice(&mem0);
        // JIT: single instruction followed by SVC (block terminator).
        unsafe {
            *(pc as *mut u32) = insn;
            *((pc + 4) as *mut u32) = 0xD400_1001; // svc #0x80
        }
        invalidate_range(pc, 8);
        if std::env::var_os("SELFTEST_VERBOSE").is_some() {
            eprintln!("test {:08x} {}", insn, disasm(insn));
        }
        let mut j = Cpu::new();
        *j = clone_cpu(&init);
        setup_cpu(&mut j);
        let fb0 = STATS_FALLBACK.load(Ordering::Relaxed);
        let host = lookup_or_translate(pc);
        if STATS_FALLBACK.load(Ordering::Relaxed) == fb0 {
            native += 1;
        }
        unsafe { maclator_jit_enter(&mut *j, host) };
        let pending = PENDING.with(|p| p.take());
        if j.exit_reason == EXIT_PENDING || pending.is_some() {
            skipped += 1;
            buf.copy_from_slice(&mem0);
            continue;
        }
        let mut a = snapshot(&r);
        let mut b = snapshot(&j);
        if branch {
            a.push(r.pc);
            b.push(j.pc);
        }
        let mem_diff = buf != mem_ref;
        if a != b || mem_diff || (!branch && j.exit_reason != EXIT_SVC) {
            bad += 1;
            if bad <= 40 {
                println!("MISMATCH {:08x} {}", insn, disasm(insn));
                for i in 0..a.len() {
                    if a[i] != b[i] {
                        let name = if i < 32 { format!("x{i}") } else if i == 32 { "nzcv".into() } else if i < 97 { format!("v{}.{}", (i - 33) / 2, (i - 33) % 2) } else if i == 97 { "tpidr".into() } else { "pc".into() };
                        println!("    {name}: interp={:#x} jit={:#x}", a[i], b[i]);
                    }
                }
                if mem_diff {
                    println!("    memory differs");
                }
                if j.exit_reason != EXIT_SVC {
                    println!("    exit reason {}", j.exit_reason);
                }
            }
        } else {
            ok += 1;
        }
        buf.copy_from_slice(&mem0);
    }
    println!("jit selftest [{class}]: ok={ok} mismatch={bad} skipped={skipped} native={native}");
}

fn clone_cpu(c: &Cpu) -> Cpu {
    let mut n = Cpu::default();
    n.x = c.x;
    n.pc = c.pc;
    n.set_nzcv(c.nzcv());
    n.v = c.v;
    n.fpcr = c.fpcr;
    n.fpsr = c.fpsr;
    n.tpidr_el0 = c.tpidr_el0;
    n.tpidrro_el0 = c.tpidrro_el0;
    n
}


/// `maclator --jit-dump <hex insn>...`: translate the instructions as one block and print the
/// generated x86 code as hex (disassemble with objdump on a .byte file).
pub fn dump_block(words: &[u32]) {
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x1000, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64
    };
    for (i, w) in words.iter().enumerate() {
        unsafe { *((page + 4 * i as u64) as *mut u32) = *w };
    }
    unsafe { *((page + 4 * words.len() as u64) as *mut u32) = 0xD400_1001 };
    let host = translate(page);
    let bytes = unsafe { std::slice::from_raw_parts(host as *const u8, 512) };
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    println!("{hex}");
}
