//! JIT self-test: translate single instructions and compare the result with
//! the reference interpreter (`maclator --selftest-jit [class] [count]`).

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

pub fn run_selftest(class: &str, count: usize) {
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
    let is_mem = class == "ldst";
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x1000, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64
    };
    let mut buf = vec![0u8; 1 << 20];
    let bufp = buf.as_mut_ptr() as u64;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut ok, mut bad, mut skipped) = (0, 0, 0);
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
        if is_mem && insn & 0xBF00_0000 == 0x0C00_0000 {
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
        let host = lookup_or_translate(page);
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
    println!("jit selftest [{class}]: ok={ok} mismatch={bad} skipped={skipped}");
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
