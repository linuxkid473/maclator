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

pub fn run_selftest(class: &str, count: usize) {
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
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x1000, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64
    };
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
        init.pc = page;
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
            *(page as *mut u32) = insn;
            *((page + 4) as *mut u32) = 0xD400_1001; // svc #0x80
        }
        invalidate_range(page, 8);
        if std::env::var_os("SELFTEST_VERBOSE").is_some() {
            eprintln!("test {:08x} {}", insn, disasm(insn));
        }
        let mut j = Cpu::new();
        *j = clone_cpu(&init);
        setup_cpu(&mut j);
        let fb0 = STATS_FALLBACK.load(Ordering::Relaxed);
        let host = lookup_or_translate(page);
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
