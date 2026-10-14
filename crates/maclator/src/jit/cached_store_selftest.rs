//! Ordinary cached GPR stores, compared with the interpreter at every budget
//! suspension. All four host cache slots and all four store sizes must emit
//! the new path. Release/RCpc, vector, pair and uncached stores must not.

use super::*;
use maclator_core::mem;
use std::sync::atomic::AtomicUsize;

static COMMPAGE_CALLS: AtomicUsize = AtomicUsize::new(0);
static INTERP_CALLS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn counting_interp(cpu: *mut Cpu, word: u32, pc: u64) -> u64 {
    INTERP_CALLS.fetch_add(1, Ordering::Relaxed);
    helper_interp(cpu, word, pc)
}

extern "C" fn noisy_commpage(address: u64) -> u64 {
    COMMPAGE_CALLS.fetch_add(1, Ordering::Relaxed);
    let host = mem::host(address) as u64;
    unsafe {
        std::arch::asm!("xor ecx, ecx", "xor edx, edx", "pxor xmm8, xmm8", "pxor xmm15, xmm15",
            out("rcx") _, out("rdx") _, out("xmm8") _, out("xmm15") _, options(nostack));
    }
    host
}

fn clone_arch(cpu: &Cpu) -> Cpu {
    let mut copy = Cpu::default(); copy.x = cpu.x; copy.v = cpu.v; copy.pc = cpu.pc;
    copy.fpcr = cpu.fpcr; copy.fpsr = cpu.fpsr;
    copy.tpidr_el0 = cpu.tpidr_el0; copy.tpidrro_el0 = cpu.tpidrro_el0;
    copy.set_nzcv(cpu.nzcv()); copy
}

fn snapshot(cpu: &Cpu) -> Vec<u64> {
    let mut result = cpu.x.to_vec();
    for vector in cpu.v { result.extend(vector); }
    result.extend([cpu.nzcv() as u64, cpu.fpcr, cpu.fpsr, cpu.tpidr_el0, cpu.tpidrro_el0]);
    result
}

fn add_zero(register: u32, wide: bool) -> u32 {
    (if wide { 0x9100_0000 } else { 0x1100_0000 }) | (register << 5) | register
}

fn same_base_index(address: u64, shift: u32) -> u64 {
    if shift == 0 { assert_eq!(address & 1, 0); return address >> 1; }
    let coefficient = 1 + (1u64 << shift);
    let mut inverse = 1u64;
    for _ in 0..6 { inverse = inverse.wrapping_mul(2u64.wrapping_sub(coefficient.wrapping_mul(inverse))); }
    let index = address.wrapping_mul(inverse);
    assert_eq!(index.wrapping_add(index.wrapping_shl(shift)), address); index
}

fn store_word(mode: usize, size: u32, rn: u32, rm: u32, rt: u32) -> u32 {
    match mode {
        0 | 9 => 0x3900_0000 | (size << 30) | (rn << 5) | rt, // unsigned STR #0
        1 => 0x3820_7800 | (size << 30) | (rm << 16) | (rn << 5) | rt, // LSL index
        2 => 0x3800_0000 | (size << 30) | ((-16i32 as u32 & 0x1ff) << 12) | (rn << 5) | rt,
        3 => 0x3800_0c00 | (size << 30) | (16 << 12) | (rn << 5) | rt, // pre +16
        4 => 0x3800_0400 | (size << 30) | (16 << 12) | (rn << 5) | rt, // post +16
        5 => 0x089f_fc00 | (size << 30) | (rn << 5) | rt, // STLR
        6 => 0x1900_0000 | (size << 30) | ((-16i32 as u32 & 0x1ff) << 12) | (rn << 5) | rt, // STLUR
        7 => (if size == 3 { 0xa900_0000 } else { 0x2900_0000 }) | (7 << 10) | (rn << 5) | rt, // STP
        8 => 0x3d00_0000 | (size << 30) | (rn << 5) | rt, // vector STR B/H/S/D
        _ => panic!("unknown store fixture mode"),
    }
}

pub fn run() {
    const ITERATIONS: u64 = 7;
    const DATA_LEN: usize = 0x8000;
    let code = unsafe { libc::mmap(std::ptr::null_mut(), 4096,
        libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
    let data = unsafe { libc::mmap(std::ptr::null_mut(), DATA_LEN,
        libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
    assert_ne!(code, libc::MAP_FAILED); assert_ne!(data, libc::MAP_FAILED);
    let code = code as u64;
    let arena = unsafe { std::slice::from_raw_parts_mut(data as *mut u8, DATA_LEN) };
    let mut redirected = vec![0u8; 0xc000];
    let saved_redirect = mem::COMMPAGE_REDIRECT.swap(redirected.as_mut_ptr() as u64, Ordering::Relaxed);
    let before_counts = emit::cached_store_counts();
    let before_helpers = COMMPAGE_CALLS.load(Ordering::Relaxed);
    let before_interp = INTERP_CALLS.load(Ordering::Relaxed);
    let (mut cases, mut checkpoints, mut yields) = (0usize, 0usize, 0usize);
    let mut mode_cases = [0usize; 10]; let mut shape_cases = [0usize; 7];
    for size in 0..=3 {
        for slot in 0..4 {
            let source = slot as u32 + 1;
            for mode in 0..10 {
                if mode == 7 && size < 2 { continue; }
                // normal, rt==base, rt==index, rt==base==index, SP base,
                // XZR data, and cache pressure excluding the data register.
                for shape in 0..7 {
                    if matches!(shape, 2 | 3) && mode != 1 { continue; }
                    // Pre/post rt==base is constrained-unpredictable; leave
                    // that form to the existing interpreter/pair contract.
                    if shape == 1 && matches!(mode, 3 | 4) { continue; }
                    for location in 0..4 {
                        // Add an explicit W-source write for ordinary stores
                        // with no pointer alias, proving 32-bit clearing and
                        // subsequent cache refresh. All other cases retain
                        // deliberately nonzero high 32-bit source contents.
                        let patterns = if shape == 0 && mode == 0 { 2 } else { 1 };
                        for pattern in 0..patterns {
                            for (i, byte) in arena.iter_mut().enumerate() { *byte = (i as u8).wrapping_mul(29).wrapping_add(0xb3); }
                            for (i, byte) in redirected.iter_mut().enumerate() { *byte = (i as u8).wrapping_mul(29).wrapping_add(0xb3); }
                            let is_commpage = location == 2;
                            let bytes = (1usize << size) * if mode == 7 { 2 } else { 1 };
                            let mut offset = if location == 3 { 0x4000 - (bytes / 2).max(1) } else { 0x500 };
                            if shape == 3 && size == 0 { offset &= !1; }
                            let raw_address = if is_commpage { mem::COMMPAGE_RO + offset as u64 }
                                else { data as u64 + offset as u64 };
                            let address = raw_address | if matches!(location, 1 | 2) { 0xa500_0000_0000_0000 } else { 0 };
                            let rt = if shape == 5 { 31 } else if shape == 6 { 10 } else { source };
                            let rn = if matches!(shape, 1 | 3) { rt } else if shape == 4 { 31 } else { 24 };
                            let rm = if matches!(shape, 2 | 3) { rt } else { 8 };
                            let mut initial = Cpu::new();
                            for i in 0..32 {
                                initial.x[i] = 0xfedc_1234_9876_0010u64.wrapping_add(i as u64 * 7919);
                                initial.v[i] = [0xfedc_ba98_7654_3210 ^ i as u64,
                                    0x0123_4567_89ab_cdef ^ i as u64];
                            }
                            initial.pc = code; initial.x[20] = ITERATIONS;
                            initial.fpcr = 0; initial.fpsr = 0x0800_009f;
                            if mode == 9 {
                                // A non-default FPCR forces a native-supported
                                // FMA through helper_interp before the store.
                                // The fixed cache must be reloaded on return.
                                initial.fpcr = 1 << 22;
                                initial.v[0] = [0x3fe0_0000_0000_0000, 0];
                                initial.v[1] = [0x3fe8_0000_0000_0000, 0];
                                initial.v[2] = [0x3fd0_0000_0000_0000, 0];
                            }
                            initial.set_nzcv(((cases * 11) & 15) as u32);
                            let index = if shape == 3 { same_base_index(address, size) }
                                else if location == 3 { u64::MAX - 7 } else { 40 };
                            if mode == 1 { initial.x[rm as usize] = index; }
                            if rn != rm || mode != 1 {
                                initial.x[rn as usize] = match mode {
                                    1 => address.wrapping_sub(index.wrapping_shl(size)),
                                    2 | 6 => address.wrapping_add(16),
                                    3 => address.wrapping_sub(16),
                                    _ => address,
                                };
                            }
                            // Weighted counts fix guest x1/x2/x3/x4 into
                            // RBX/RBP/R12/R13, even with one extra source or
                            // address read. x10 remains deliberately uncached.
                            let mut program = Vec::new();
                            for (register, repeat) in [(1, 8), (2, 6), (3, 4), (4, 2)] {
                                for _ in 0..repeat { program.push(add_zero(register, true)); }
                            }
                            if pattern != 0 { program.push(add_zero(rt, false)); }
                            if mode == 9 { program.push(0x1f41_0802); } // FMADD D2,D0,D1,D2
                            program.push(store_word(mode, size, rn, rm, rt));
                            program.push(0xd100_0694); // SUB x20,x20,#1
                            let branch_offset = -(program.len() as i32);
                            program.push(0xb500_0014 | (((branch_offset as u32) & 0x7ffff) << 5));
                            let loop_len = program.len(); program.push(0xd400_1001);
                            unsafe { for (i, &word) in program.iter().enumerate() { *((code as *mut u32).add(i)) = word; } }
                            invalidate_range(code, (program.len() * 4) as u64);
                            let case_emissions_before = emit::cached_store_counts();
                            let mut reference = clone_arch(&initial); let mut translated = clone_arch(&initial);
                            setup_cpu(&mut translated);
                            translated.helpers[HELPER_COMMPAGE] = noisy_commpage as usize as u64;
                            translated.helpers[HELPER_INTERP] = counting_interp as usize as u64;
                            let mut expected = if is_commpage { redirected.clone() } else { arena.to_vec() };
                            let budgets = [0, loop_len - 1, loop_len, 2 * loop_len - 1, 2 * loop_len, 1, 3 * loop_len];
                            let mut hops = 0usize;
                            loop {
                                translated.budget = budgets[hops.min(budgets.len() - 1)] as u64;
                                let host = lookup_or_translate(translated.pc);
                                unsafe { maclator_jit_enter(&mut translated, host); }
                                assert!(PENDING.with(|pending| pending.take()).is_none(), "cached store case {cases} pending exception");
                                assert!(translated.x[20] <= reference.x[20]);
                                let completed = reference.x[20] - translated.x[20];
                                let actual = if is_commpage { redirected.clone() } else { arena.to_vec() };
                                if is_commpage { redirected.copy_from_slice(&expected); } else { arena.copy_from_slice(&expected); }
                                for _ in 0..completed {
                                    assert_eq!(reference.pc, code);
                                    for &word in &program[..loop_len] {
                                        interp::exec(&mut reference, word).unwrap_or_else(|error|
                                            panic!("invalid cached store reference case {cases} word={word:x}: {error:?}"));
                                    }
                                }
                                expected = if is_commpage { redirected.clone() } else { arena.to_vec() };
                                assert_eq!(snapshot(&translated), snapshot(&reference),
                                    "cached store case {cases} size={size} slot={slot} mode={mode} shape={shape} location={location} checkpoint={hops}");
                                assert_eq!(actual, expected, "cached store case {cases} complete memory checkpoint={hops}");
                                if is_commpage { redirected.copy_from_slice(&actual); } else { arena.copy_from_slice(&actual); }
                                checkpoints += 1;
                                if translated.exit_reason == EXIT_SVC { assert_eq!(translated.x[20], 0); break; }
                                assert_eq!(translated.exit_reason, EXIT_CONTINUE); assert_eq!(translated.pc, reference.pc);
                                yields += 1; hops += 1; assert!(hops < 16);
                            }
                            let case_emissions_after = emit::cached_store_counts();
                            let emitted: u64 = (0..16).map(|i| case_emissions_after[i] - case_emissions_before[i]).sum();
                            let eligible = emit::cached_store() && emit::loop_cache()
                                && (mode <= 4 || (mode == 9 && emit::native_fma())) && shape < 5;
                            if eligible {
                                assert!(emitted != 0, "eligible cached store case {cases} emitted no fast path");
                                assert!(case_emissions_after[slot * 4 + size as usize] > case_emissions_before[slot * 4 + size as usize],
                                    "cached store case {cases} used wrong host source slot");
                            } else { assert_eq!(emitted, 0, "rejected cached store case {cases} emitted fast code"); }
                            cases += 1; mode_cases[mode] += 1; shape_cases[shape] += 1;
                        }
                    }
                }
            }
        }
    }
    let after_counts = emit::cached_store_counts();
    let emissions: [u64; 16] = std::array::from_fn(|i| after_counts[i] - before_counts[i]);
    if emit::cached_store() && emit::loop_cache() {
        assert!(emissions.iter().all(|&count| count != 0), "all four source slots and four sizes need emission coverage");
    } else { assert_eq!(emissions, [0; 16]); }
    let helper_calls = COMMPAGE_CALLS.load(Ordering::Relaxed) - before_helpers;
    let interp_calls = INTERP_CALLS.load(Ordering::Relaxed) - before_interp;
    assert!(helper_calls != 0);
    assert!(interp_calls != 0, "FMA interpreter helper/reload path was not executed");
    mem::COMMPAGE_REDIRECT.store(saved_redirect, Ordering::Relaxed);
    unsafe { libc::munmap(code as *mut _, 4096); libc::munmap(data, DATA_LEN); }
    println!("jit selftest [cachedstore]: ok={cases} mismatch=0 checkpoints={checkpoints} yields={yields} slot_size_emissions={emissions:?} helper_calls={helper_calls} interp_calls={interp_calls} modes={mode_cases:?} shapes={shape_cases:?}");
}
