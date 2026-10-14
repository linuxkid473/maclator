//! Whole-loop differential tests, including forced budget suspension and
//! external re-entry. The commpage callback intentionally destroys vector
//! registers to exercise reloads required by the host calling convention.

use super::*;
use maclator_core::mem;
use std::sync::atomic::AtomicUsize;

static COMMPAGE_CALLS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn noisy_commpage(addr: u64) -> u64 {
    COMMPAGE_CALLS.fetch_add(1, Ordering::Relaxed);
    let host = mem::host(addr) as u64;
    unsafe {
        std::arch::asm!(
            "pxor xmm8, xmm8", "pxor xmm9, xmm9", "pxor xmm10, xmm10", "pxor xmm11, xmm11",
            "pxor xmm12, xmm12", "pxor xmm13, xmm13", "pxor xmm14, xmm14", "pxor xmm15, xmm15",
            out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _,
            out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _, options(nostack));
    }
    host
}

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
    out.excl_addr = cpu.excl_addr; out.excl_val = cpu.excl_val;
    out.excl_val2 = cpu.excl_val2; out.excl_size = cpu.excl_size;
    out.set_nzcv(cpu.nzcv());
    out
}

fn snapshot(cpu: &Cpu) -> Vec<u64> {
    let mut out = cpu.x.to_vec();
    for v in cpu.v { out.extend(v); }
    out.extend([cpu.nzcv() as u64, cpu.fpcr, cpu.fpsr, cpu.tpidr_el0, cpu.tpidrro_el0]);
    out.extend([cpu.excl_addr, cpu.excl_val, cpu.excl_val2, cpu.excl_size,
        cpu.nf as u64, cpu.zf as u64, cpu.cf as u64, cpu.vf as u64]);
    out
}

fn cbnz_back(words: usize) -> u32 {
    0xb500_0000 | (((-(words as i32)) as u32 & 0x7ffff) << 5) | 20
}

fn gpr_pair(load: bool, rt: u32, rt2: u32, rn: u32, index: u32, offset: i32) -> u32 {
    assert_eq!(offset % 8, 0);
    0xa800_0000 | (index << 23) | ((load as u32) << 22)
        | (((offset / 8) as u32 & 0x7f) << 15) | (rt2 << 10) | (rn << 5) | rt
}

fn add_imm(rd: u32, rn: u32, value: i32) -> u32 {
    let opcode = if value < 0 { 0xd100_0000 } else { 0x9100_0000 };
    opcode | (value.unsigned_abs() << 10) | (rn << 5) | rd
}

/// Exercise adjacent Cpu-slot transfers on both sides of the alignment join.
/// Compare the complete architectural state and guest memory at every budget
/// suspension, rather than only when the loop terminates. Source addresses are
/// shared by the reference and JIT; their memory snapshots are restored before
/// each side runs, so pointer-chasing registers compare without address rebasing.
fn run_pair_fixtures(code: u64) -> (usize, usize) {
    // (name, load, reversed operands, addressing mode, base alias, stride, read-only)
    // base alias 1/2 means rn == rt/rt2. Writeback overlap forms intentionally
    // test the interpreter's chosen behavior for constrained-unpredictable ISA
    // encodings; they do not advertise an architectural guarantee for that form.
    const FIXTURES: [(&str, bool, bool, u32, u32, i32, bool); 24] = [
        ("ldp-offset", true, false, 2, 0, 16, false),
        ("ldp-offset-reversed", true, true, 2, 0, 16, false),
        ("ldp-pre", true, false, 3, 0, 16, false),
        ("ldp-post-reversed", true, true, 1, 0, 16, false),
        ("ldp-pre-negative", true, false, 3, 0, -16, false),
        ("ldp-post-negative-reversed", true, true, 1, 0, -16, false),
        ("ldp-base-rt", true, false, 2, 1, 16, false),
        ("ldp-base-rt-reversed", true, true, 2, 1, 16, false),
        ("ldp-base-rt2", true, false, 2, 2, 16, false),
        ("ldp-base-rt2-reversed", true, true, 2, 2, 16, false),
        ("ldp-pre-base-rt", true, false, 3, 1, 16, false),
        ("ldp-post-base-rt-reversed", true, true, 1, 1, 16, false),
        ("stp-offset", false, false, 2, 0, 16, false),
        ("stp-offset-reversed", false, true, 2, 0, 16, false),
        ("stp-pre", false, false, 3, 0, 16, false),
        ("stp-post-reversed", false, true, 1, 0, 16, false),
        ("stp-post-negative", false, false, 1, 0, -16, false),
        ("stp-pre-negative-reversed", false, true, 3, 0, -16, false),
        ("stp-base-rt", false, false, 2, 1, 16, false),
        ("stp-base-rt-reversed", false, true, 2, 1, 16, false),
        ("stp-post-base-rt-reversed", false, true, 1, 1, 16, false),
        ("stp-pre-base-rt2", false, false, 3, 2, 16, false),
        ("ldp-readonly-base-rt", true, false, 2, 1, 16, true),
        ("ldp-readonly-base-rt-reversed", true, true, 2, 1, 16, true),
    ];
    const ITERATIONS: u64 = 7;
    const DATA_LEN: usize = 4096;
    let data = unsafe { libc::mmap(std::ptr::null_mut(), DATA_LEN,
        libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
    assert_ne!(data, libc::MAP_FAILED);
    let data_addr = data as u64;
    let mut rng = Rng(0x61d6_5834_23bc_59ae);
    let (mut cases, mut yields, mut checkpoints) = (0, 0, 0);
    for &(name, load, reverse, index, alias, stride, read_only) in &FIXTURES {
        for misalignment in [0u64, 8] {
            assert_eq!(unsafe { libc::mprotect(data, DATA_LEN, libc::PROT_READ | libc::PROT_WRITE) }, 0);
            unsafe { std::ptr::write_bytes(data as *mut u8, 0xa5, DATA_LEN); }
            let (rt, rt2) = if reverse { (11, 10) } else { (10, 11) };
            let rn = match alias { 1 => rt, 2 => rt2, _ => 24 };
            let first = data_addr + 0x200 + misalignment
                + if stride < 0 { (ITERATIONS - 1) * 16 } else { 0 };
            for i in 0..ITERATIONS {
                let address = first.wrapping_add((i as i64 * stride as i64) as u64);
                let mut values = [0xdead_beef_4321_0000 + i, 0x1234_5678_9abc_0000 + i * 3];
                if load && alias != 0 && matches!(index, 0 | 2) {
                    // rn==rt is the benchmark's LDP x10,x11,[x10] list shape.
                    values[(alias - 1) as usize] = address.wrapping_add(stride as u64);
                }
                unsafe {
                    std::ptr::write_unaligned(address as *mut u64, values[0]);
                    std::ptr::write_unaligned((address + 8) as *mut u64, values[1]);
                }
            }
            let mut init = Cpu::new();
            for i in 0..32 { init.x[i] = rng.next(); init.v[i] = [rng.next(), rng.next()]; }
            init.pc = code;
            init.x[20] = ITERATIONS;
            init.x[rn as usize] = if index == 3 { first.wrapping_sub(stride as u64) } else { first };
            init.x[25] = data_addr + 0x800;
            init.fpcr = 0;
            init.fpsr = 0x0800_009f;
            init.set_nzcv(0xb);
            let offset = if matches!(index, 1 | 3) { stride } else { 0 };
            let mut program = Vec::new();
            if !load {
                // Cpu slots must contain the latest write-through values even
                // when both source registers are fixed-cache residents.
                if rt != rn { program.push(add_imm(rt, rt, 1)); }
                if rt2 != rn { program.push(add_imm(rt2, rt2, 3)); }
            }
            program.push(gpr_pair(load, rt, rt2, rn, index, offset));
            program.push(0x8b00_0000 | (rt << 16) | (8 << 5) | 8);
            program.push(0xca00_0000 | (rt2 << 16) | (9 << 5) | 9);
            if load && !read_only { program.push(gpr_pair(false, rt, rt2, 25, 2, 0)); }
            if matches!(index, 0 | 2) && (!load || alias == 0) {
                program.push(add_imm(rn, rn, stride));
            }
            program.push(add_imm(20, 20, -1));
            program.push(cbnz_back(program.len()));
            let loop_len = program.len();
            program.push(0xd400_1001);
            unsafe { for (i, &word) in program.iter().enumerate() { *((code as *mut u32).add(i)) = word; } }
            invalidate_range(code, (program.len() * 4) as u64);
            let mut expected_memory = unsafe { std::slice::from_raw_parts(data as *const u8, DATA_LEN) }.to_vec();
            let mut reference = clone_arch(&init);
            let mut translated = clone_arch(&init);
            setup_cpu(&mut translated);
            if read_only { assert_eq!(unsafe { libc::mprotect(data, DATA_LEN, libc::PROT_READ) }, 0); }
            let budgets = [loop_len - 1, loop_len, 2 * loop_len - 1, 0,
                3 * loop_len, 2 * loop_len, 1];
            let mut hops = 0;
            loop {
                translated.budget = budgets[hops.min(budgets.len() - 1)] as u64;
                let host = lookup_or_translate(translated.pc);
                unsafe { maclator_jit_enter(&mut translated, host); }
                assert!(PENDING.with(|p| p.take()).is_none(), "pair {name}/{misalignment} pending exception");
                assert!(translated.x[20] <= reference.x[20], "pair {name}/{misalignment} counter increased");
                let completed = reference.x[20] - translated.x[20];
                let actual_memory = unsafe { std::slice::from_raw_parts(data as *const u8, DATA_LEN) }.to_vec();
                if !read_only {
                    unsafe { std::ptr::copy_nonoverlapping(expected_memory.as_ptr(), data as *mut u8, DATA_LEN); }
                }
                for _ in 0..completed {
                    assert_eq!(reference.pc, code, "pair reference did not resume at loop entry");
                    for &word in &program[..loop_len] {
                        interp::exec(&mut reference, word)
                            .unwrap_or_else(|error| panic!("pair reference {name}/{misalignment} {word:x}: {error:?}"));
                    }
                }
                expected_memory = unsafe { std::slice::from_raw_parts(data as *const u8, DATA_LEN) }.to_vec();
                assert_eq!(snapshot(&translated), snapshot(&reference),
                    "pair {name}/{misalignment} architectural state at checkpoint {hops}");
                assert_eq!(actual_memory, expected_memory,
                    "pair {name}/{misalignment} guest memory at checkpoint {hops}");
                checkpoints += 1;
                if !read_only {
                    unsafe { std::ptr::copy_nonoverlapping(actual_memory.as_ptr(), data as *mut u8, DATA_LEN); }
                }
                if translated.exit_reason == EXIT_SVC {
                    assert_eq!(translated.x[20], 0, "pair exited before finishing");
                    break;
                }
                assert_eq!(translated.exit_reason, EXIT_CONTINUE);
                assert_eq!(translated.pc, reference.pc, "pair {name}/{misalignment} resume PC at checkpoint {hops}");
                if hops == 0 { assert_eq!(snapshot(&translated), snapshot(&init), "initial pair budget changed state"); }
                yields += 1;
                hops += 1;
                assert!(hops < 16, "pair {name}/{misalignment} did not terminate");
            }
            cases += 1;
        }
    }
    unsafe { libc::munmap(data, DATA_LEN); }
    println!("jit selftest [loopcache-pairs]: ok={cases} checkpoints={checkpoints} yields={yields} pair_fast={} loop_cache={} register_budget={}",
        emit::pair_fast(), emit::loop_cache(), emit::register_budget());
    (cases, yields)
}

/// Integrate as `--selftest-jit loopcache [count]`. There are 640 deterministic
/// FP loop shapes, 96 integer loops, 48 pair fixtures, and additional random cases.
pub fn run(count: usize) {
    const CONTROL: [u64; 8] = [0, 1 << 25, 1 << 22, 2 << 22, 3 << 22,
        1 << 24, (1 << 24) | (1 << 25), (1 << 24) | (1 << 25) | (1 << 22)];
    let code = unsafe { libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
    assert_ne!(code, libc::MAP_FAILED);
    let code = code as u64;
    // A standalone selftest process owns this synthetic commpage for the test.
    let mut redirected = vec![0u8; 0xc000];
    let old_redirect = mem::COMMPAGE_REDIRECT.swap(redirected.as_mut_ptr() as u64, Ordering::Relaxed);
    let mut input_a = [0u64; 32];
    let mut input_b = [0u64; 32];
    let mut output = [0u8; 16];
    let mut rng = Rng(0x7e29_ada2_103c_8975);
    let (mut ok, mut bad, mut yields) = (0usize, 0usize, 0usize);
    let before_loops = emit::LOOP_CACHE_BLOCKS.load(Ordering::Relaxed);
    let before_vectors = emit::VECTOR_LOOP_CACHE_BLOCKS.load(Ordering::Relaxed);
    let before_calls = COMMPAGE_CALLS.load(Ordering::Relaxed);
    let (pair_cases, pair_yields) = run_pair_fixtures(code);
    ok += pair_cases;
    yields += pair_yields;
    for it in 0..736 + count {
        // Integer cases do not store memory. Start them from the same bytes as
        // FP cases so resetting the JIT run cannot erase a prior case's result.
        output.fill(0xa5);
        redirected[0x500..0x510].fill(0xa5);
        let fp = it < 640 || (it >= 736 && it % 2 == 0);
        let iterations = 1 + (rng.next() % 16);
        let mut init = Cpu::new();
        for i in 0..32 { init.x[i] = rng.next(); init.v[i] = [rng.next(), rng.next()]; }
        init.pc = code;
        init.x[20] = iterations;
        init.fpsr = rng.next() & 0xf800_009f;
        init.set_nzcv(rng.next() as u32 & 15);
        let mut program;
        let mut commpage = false;
        if fp {
            let mut shape = it;
            let double = shape % 2 != 0; shape /= 2;
            let mode = shape % 4; shape /= 4;
            init.fpcr = CONTROL[shape % 8]; shape /= 8;
            let alias = shape % 5; shape /= 5;
            commpage = shape % 2 != 0;
            let (rn, rm, rd, ra) = match alias {
                0 => (0, 1, 2, 2), 1 => (2, 1, 2, 2), 2 => (0, 2, 2, 2),
                3 => (2, 2, 2, 2), _ => (0, 1, 0, 2),
            };
            let bytes = if double { 8 } else { 4 };
            let edge = it / 8 % 8;
            let n = if double {
                [0x3fe0_0000_0000_0000, 0x0010_0000_0000_0000, 1, 0x7ff0_0000_0000_0000,
                 0x7ff8_0000_0000_0042, 0x7ff0_0000_0000_0013, 0, 1 << 63][edge]
            } else {
                [0x3f00_0000, 0x0080_0000, 1, 0x7f80_0000, 0x7fc0_0042, 0x7f80_0013, 0, 1 << 31][edge]
            };
            let m = if double { 0x3fe8_0000_0000_0000 } else { 0x3f40_0000 };
            let initial_accumulator = if double { 0x3fc0_0000_0000_0000 } else { 0x3e00_0000 };
            init.v[2] = [initial_accumulator, u64::MAX];
            input_a.fill(n); input_b.fill(m);
            if commpage {
                for i in 0..32 {
                    let offset = i * bytes;
                    redirected[0x100 + offset..0x100 + offset + bytes].copy_from_slice(&n.to_le_bytes()[..bytes]);
                    redirected[0x300 + offset..0x300 + offset + bytes].copy_from_slice(&m.to_le_bytes()[..bytes]);
                }
                init.x[24] = mem::COMMPAGE_RO + 0x100;
                init.x[25] = mem::COMMPAGE_RO + 0x300;
                init.x[26] = mem::COMMPAGE_RO + 0x500;
                redirected[0x500..0x510].fill(0xa5);
            } else {
                // Singles are packed as u32 lanes; doubles as u64 lanes.
                if !double {
                    for i in 0..16 { input_a[i] = n | (n << 32); input_b[i] = m | (m << 32); }
                }
                init.x[24] = input_a.as_ptr() as u64;
                init.x[25] = input_b.as_ptr() as u64;
                init.x[26] = output.as_mut_ptr() as u64;
                output.fill(0xa5);
            }
            let load = if double { 0xfd40_0000 } else { 0xbd40_0000 };
            let store = if double { 0xfd00_0000 } else { 0xbd00_0000 };
            let fma = 0x1f00_0000 | ((double as u32) << 22) | (((mode / 2) as u32) << 21)
                | (rm << 16) | (((mode % 2) as u32) << 15) | (ra << 10) | (rn << 5) | rd;
            program = vec![load | (24 << 5), load | (25 << 5) | 1, fma, store | (26 << 5) | rd,
                0x9100_0000 | ((bytes as u32) << 10) | (24 << 5) | 24,
                0x9100_0000 | ((bytes as u32) << 10) | (25 << 5) | 25, 0xd100_0694];
        } else {
            let wide = it % 2 == 0;
            let sf = if wide { 1 << 31 } else { 0 };
            let imm = rng.next() as u32 & 0xffff;
            program = vec![0x5280_0005 | sf | (imm << 5),
                0x0b01_0000 | sf, 0x4b02_0021 | sf, 0x4a00_0042 | sf,
                0x1b01_0803 | sf, 0x6b05_007f | sf, 0x1a81_c004 | sf,
                0x1100_0c84 | sf, 0xd100_0694];
        }
        program.push(cbnz_back(program.len()));
        let loop_len = program.len();
        program.push(0xd400_1001);
        unsafe { for (i, &word) in program.iter().enumerate() { *((code as *mut u32).add(i)) = word; } }
        invalidate_range(code, (program.len() * 4) as u64);
        let mut reference = clone_arch(&init);
        let mut steps = 0;
        loop {
            let word = unsafe { *(reference.pc as *const u32) };
            match interp::exec(&mut reference, word) {
                Ok(()) => (), Err(Exit::Svc(_)) => break,
                Err(error) => panic!("invalid loop reference {it}, word {word:x}: {error:?}"),
            }
            steps += 1;
            assert!(steps < 1024, "reference loop did not terminate");
        }
        let expected_memory = if commpage { redirected[0x500..0x510].to_vec() } else { output.to_vec() };
        if commpage { redirected[0x500..0x510].fill(0xa5); } else { output.fill(0xa5); }
        let mut translated = clone_arch(&init);
        setup_cpu(&mut translated);
        translated.helpers[HELPER_COMMPAGE] = noisy_commpage as usize as u64;
        // The first entry cannot execute even one instruction; the return must
        // preserve all architectural state and identify the external entry PC.
        translated.budget = loop_len as u64 - 1;
        let host = lookup_or_translate(code);
        unsafe { maclator_jit_enter(&mut translated, host); }
        assert_eq!(translated.exit_reason, EXIT_CONTINUE);
        assert_eq!(translated.pc, code);
        assert_eq!(snapshot(&translated), snapshot(&init), "budget changed guest state at case {it}");
        yields += 1;
        let mut hops = 0;
        loop {
            translated.budget = (loop_len * 2) as u64;
            let host = lookup_or_translate(translated.pc);
            unsafe { maclator_jit_enter(&mut translated, host); }
            let pending = PENDING.with(|p| p.take());
            assert!(pending.is_none(), "loop raised a pending exception");
            if translated.exit_reason == EXIT_SVC { break; }
            assert_eq!(translated.exit_reason, EXIT_CONTINUE);
            yields += 1; hops += 1;
            assert!(hops < 64, "translated loop did not terminate");
        }
        let a = snapshot(&reference);
        let b = snapshot(&translated);
        let actual_memory = if commpage { &redirected[0x500..0x510] } else { &output[..] };
        if a != b || actual_memory != expected_memory {
            bad += 1;
            if bad <= 12 {
                println!("MISMATCH loop {it} fp={fp} commpage={commpage} fpcr={:#x} program={program:x?}", init.fpcr);
                for i in 0..a.len() { if a[i] != b[i] { println!("    slot {i}: reference={:x} jit={:x}", a[i], b[i]); } }
                if actual_memory != expected_memory { println!("    memory reference={expected_memory:x?} jit={actual_memory:x?}"); }
            }
        } else { ok += 1; }
    }
    mem::COMMPAGE_REDIRECT.store(old_redirect, Ordering::Relaxed);
    unsafe { libc::munmap(code as *mut _, 4096); }
    let loop_blocks = emit::LOOP_CACHE_BLOCKS.load(Ordering::Relaxed) - before_loops;
    let vector_blocks = emit::VECTOR_LOOP_CACHE_BLOCKS.load(Ordering::Relaxed) - before_vectors;
    let helper_calls = COMMPAGE_CALLS.load(Ordering::Relaxed) - before_calls;
    println!("jit selftest [loopcache]: ok={ok} mismatch={bad} yields={yields} cached={loop_blocks} vector_cached={vector_blocks} clobbering_helper_calls={helper_calls}");
    assert!(helper_calls != 0, "synthetic commpage helper was not exercised");
    assert_eq!(bad, 0, "whole-loop differential mismatch");
}

// Source-only addition for the scalar-writeback experiment. The parent runs
// this in bounded standalone processes, separately for each feature setting.
thread_local! {
    static SCALAR_HELPER_EXPECTED: RefCell<std::collections::VecDeque<(u64, u32, Vec<u64>)>> =
        RefCell::new(std::collections::VecDeque::new());
}
static SCALAR_HELPER_CHECKS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn scalar_checked_interp(cpu: *mut Cpu, word: u32, pc: u64) -> u64 {
    SCALAR_HELPER_EXPECTED.with(|queue| {
        let (want_pc, want_word, want_state) = queue.borrow_mut().pop_front()
            .expect("unexpected scalar interpreter helper");
        assert_eq!((pc, word), (want_pc, want_word), "scalar helper order/operands");
        assert_eq!(snapshot(unsafe { &*cpu }), want_state,
            "helper must see every latest ORIGINAL vector before the FMA, including NaN accumulator");
    });
    SCALAR_HELPER_CHECKS.fetch_add(1, Ordering::Relaxed);
    helper_interp(cpu, word, pc)
}

/// Independent block-budget simulation. The chosen unit contains two scalar
/// self-loops with different maps, a bridge, and SVC. Every emitted successor
/// is internal, so all block entries consume their own instruction budget.
fn scalar_reference_entry(cpu: &mut Cpu, blocks: &[(u64, Vec<u32>)], mut budget: u64,
    helper_expected: bool) -> (u64, std::collections::VecDeque<(u64, u32, Vec<u64>)>)
{
    let mut expected = std::collections::VecDeque::new();
    loop {
        let (_, words) = blocks.iter().find(|(pc, _)| *pc == cpu.pc)
            .expect("unknown scalar reference entry");
        if budget < words.len() as u64 { return (EXIT_CONTINUE, expected); }
        budget -= words.len() as u64;
        for &word in words {
            if helper_expected && word & 0xff00_0000 == 0x1f00_0000 {
                expected.push_back((cpu.pc, word, snapshot(cpu)));
            }
            match interp::exec(cpu, word) {
                Ok(()) => (),
                Err(Exit::Svc(_)) => return (EXIT_SVC, expected),
                other => panic!("scalar reference instruction {word:08x}: {other:?}"),
            }
        }
    }
}

/// Compare every entry budget against an independently stepped interpreter;
/// check helper input state before the reference executes a NaN/nondefault-mode
/// FMA. Two loops in one Plain unit catch cold exits that incorrectly use the
/// mapping of the final block. Patchable units exercise the same entry API.
pub fn run_scalar_writeback() {
    assert!(emit::native_fma(), "scalarwriteback requires host FMA/AVX support");
    assert!(emit::loop_cache(), "scalarwriteback needs fixed loop caching");
    assert!(std::env::var_os("MACLATOR_JIT_VERIFY").is_none());
    assert!(std::env::var("MACLATOR_JIT_DISABLE").unwrap_or_default().is_empty());
    let raw = unsafe { libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
    assert_ne!(raw, libc::MAP_FAILED);
    let page = raw as u64;
    let mut redirected = vec![0u8; 0xc000];
    let old_redirect = mem::COMMPAGE_REDIRECT.swap(redirected.as_mut_ptr() as u64, Ordering::Relaxed);
    let mut input_a = [0u64; 2];
    let mut input_b = [0u64; 2];
    let mut output = [0xa5u8; 16];
    let before_blocks = emit::SCALAR_WRITEBACK_BLOCKS.load(Ordering::Relaxed);
    let before_stores = emit::SCALAR_WRITEBACK_SKIPPED_STORES.load(Ordering::Relaxed);
    let before_helpers = SCALAR_HELPER_CHECKS.load(Ordering::Relaxed);
    let before_commpage = COMMPAGE_CALLS.load(Ordering::Relaxed);
    let mut checked = 0usize;
    let mut rng = Rng(0xe1da_74c0_c875_1fd0);
    for stub_kind in [StubKind::Plain, StubKind::Patchable] {
        for double in [false, true] { for mode in 0..4 { for alias in 0..5 { for memory in 0..3 {
            let (rn, rm, rd, ra) = match alias {
                0 => (0, 1, 2, 2), 1 => (2, 1, 2, 2), 2 => (0, 2, 2, 2),
                3 => (2, 2, 2, 2), _ => (0, 1, 0, 2),
            };
            let load = if double { 0xfd40_0000 } else { 0xbd40_0000 };
            let store = if double { 0xfd00_0000 } else { 0xbd00_0000 };
            let fma = |base: u32| 0x1f00_0000 | ((double as u32) << 22)
                | ((mode / 2) << 21) | ((base + rm) << 16) | ((mode % 2) << 15)
                | ((base + ra) << 10) | ((base + rn) << 5) | (base + rd);
            let loop_words = |base: u32, counter: u32| vec![
                load | (24 << 5) | base, load | (25 << 5) | (base + 1), fma(base),
                store | (26 << 5) | (base + rd), 0xd100_0000 | (1 << 10) | (counter << 5) | counter,
                0xb500_0000 | (((-5i32) as u32 & 0x7ffff) << 5) | counter];
            let blocks = vec![
                (page, loop_words(0, 20)),
                (page + 24, vec![0x1400_0000 | ((0x100 - 24) / 4)]),
                (page + 0x100, loop_words(10, 21)),
                (page + 0x118, vec![0xd400_1001]),
            ];
            unsafe { for (pc, words) in &blocks { for (i, &word) in words.iter().enumerate() {
                *((*pc as *mut u32).add(i)) = word;
            } } }
            let read = |pc: u64| blocks.iter().find_map(|(start, words)| {
                pc.checked_sub(*start).filter(|off| off % 4 == 0)
                    .and_then(|off| words.get((off / 4) as usize).copied())
            });
            let mut unit = Unit::new(stub_kind, true);
            for (pc, _) in &blocks { let label = unit.a.create_label(); unit.labels.insert(*pc, label); }
            for (pc, _) in &blocks { emit::emit_block(&mut unit, *pc, &read); }
            unit.finish();
            // All successors are internal: no uninitialized patchable slots.
            assert!(unit.finished_stubs.is_empty());
            let cache = code_cache(); let host = cache.cursor();
            let assembled = unit.a.assemble(host).expect("scalar multi-block assembly");
            assert_eq!(cache.alloc(assembled.len() as u64), host);
            unsafe { std::ptr::copy_nonoverlapping(assembled.as_ptr(), host as *mut u8, assembled.len()); }
            for value_case in 0..3 {
                let n: u64 = if double { 0x3fe0_0000_0000_0000 } else { 0x3f00_0000 };
                let m: u64 = if double { 0x3fe8_0000_0000_0000 } else { 0x3f40_0000 };
                let acc: u64 = if value_case == 2 {
                    if double { 0x7ff0_0000_0000_0042 } else { 0x7f80_0042 }
                } else if double { 0x3fc0_0000_0000_0000 } else { 0x3e00_0000 };
                input_a.fill(n); input_b.fill(m);
                let bytes = if double { 8 } else { 4 };
                redirected[0x100..0x100 + bytes].copy_from_slice(&n.to_le_bytes()[..bytes]);
                redirected[0x200..0x200 + bytes].copy_from_slice(&m.to_le_bytes()[..bytes]);
                let mut initial = Cpu::default();
                for i in 0..32 { initial.x[i] = rng.next(); initial.v[i] = [rng.next(), rng.next()]; }
                initial.pc = page; initial.x[20] = 2; initial.x[21] = 3;
                initial.v[2] = [acc, u64::MAX]; initial.v[12] = [acc, u64::MAX];
                initial.fpcr = if value_case == 1 { 1 << 25 } else { 0 };
                initial.fpsr = 1 << 27; initial.set_nzcv(11);
                initial.tpidr_el0 = rng.next(); initial.tpidrro_el0 = rng.next();
                initial.excl_addr = rng.next(); initial.excl_val = rng.next();
                initial.excl_val2 = rng.next(); initial.excl_size = 16;
                if memory == 2 {
                    initial.x[24] = mem::COMMPAGE_RO + 0x100;
                    initial.x[25] = mem::COMMPAGE_RO + 0x200;
                    initial.x[26] = mem::COMMPAGE_RO + 0x400;
                } else {
                    let tag = if memory == 1 { 0xff00_0000_0000_0000 } else { 0 };
                    initial.x[24] = input_a.as_ptr() as u64 | tag;
                    initial.x[25] = input_b.as_ptr() as u64 | tag;
                    initial.x[26] = output.as_mut_ptr() as u64 | tag;
                }
                for budget in 0..=35u64 {
                    output.fill(0xa5); redirected[0x400..0x410].fill(0xa5);
                    let mut reference = clone_arch(&initial);
                    let (reason, helper_expected) = scalar_reference_entry(&mut reference, &blocks, budget, value_case != 0);
                    let expected_memory = if memory == 2 { redirected[0x400..0x410].to_vec() } else { output.to_vec() };
                    output.fill(0xa5); redirected[0x400..0x410].fill(0xa5);
                    SCALAR_HELPER_EXPECTED.with(|queue| *queue.borrow_mut() = helper_expected);
                    let mut actual = clone_arch(&initial); setup_cpu(&mut actual); actual.budget = budget;
                    actual.helpers[HELPER_INTERP] = scalar_checked_interp as usize as u64;
                    actual.helpers[HELPER_COMMPAGE] = noisy_commpage as usize as u64;
                    unsafe { maclator_jit_enter(&mut actual, host); }
                    assert!(PENDING.with(|pending| pending.take()).is_none());
                    assert_eq!((actual.pc, actual.exit_reason), (reference.pc, reason), "scalar boundary PC/reason at budget {budget}");
                    assert_eq!(snapshot(&actual), snapshot(&reference), "scalar complete state at budget {budget}; mode={mode} alias={alias} memory={memory} value={value_case}");
                    let actual_memory = if memory == 2 { &redirected[0x400..0x410] } else { &output[..] };
                    assert_eq!(actual_memory, &expected_memory[..], "scalar guest memory at budget {budget}");
                    SCALAR_HELPER_EXPECTED.with(|queue| assert!(queue.borrow().is_empty(), "missing scalar helper"));
                    checked += 1;
                }
            }
        } } } }
    }
    mem::COMMPAGE_REDIRECT.store(old_redirect, Ordering::Relaxed);
    unsafe { libc::munmap(raw, 4096); }
    let blocks = emit::SCALAR_WRITEBACK_BLOCKS.load(Ordering::Relaxed) - before_blocks;
    let stores = emit::SCALAR_WRITEBACK_SKIPPED_STORES.load(Ordering::Relaxed) - before_stores;
    let helper_checks = SCALAR_HELPER_CHECKS.load(Ordering::Relaxed) - before_helpers;
    let commpage = COMMPAGE_CALLS.load(Ordering::Relaxed) - before_commpage;
    assert!(helper_checks > 0 && commpage > 0, "scalar helper paths must execute");
    if emit::scalar_writeback() { assert!(blocks > 0 && stores > 0, "writeback must be emitted"); }
    else { assert_eq!((blocks, stores), (0, 0), "OFF path must not emit writeback"); }
    println!("jit selftest [scalarwriteback]: checked={checked} mismatch=0 blocks={blocks} skipped_cpu_store_emissions={stores} original_helper_checks={helper_checks} clobbering_commpage_calls={commpage}");
}
