//! Register-offset memory loops compared with the interpreter after every
//! forced budget suspension. Valid page-boundary accesses are covered; this
//! test does not claim synchronous guest fault/RESUME coverage.

use super::*;
use maclator_core::mem;
use std::sync::atomic::AtomicUsize;

static COMMPAGE_CALLS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn noisy_commpage(address: u64) -> u64 {
    COMMPAGE_CALLS.fetch_add(1, Ordering::Relaxed);
    let host = mem::host(address) as u64;
    unsafe {
        std::arch::asm!(
            "xor ecx, ecx", "xor edx, edx", "pxor xmm8, xmm8", "pxor xmm15, xmm15",
            out("rcx") _, out("rdx") _, out("xmm8") _, out("xmm15") _, options(nostack));
    }
    host
}

fn clone_arch(cpu: &Cpu) -> Cpu {
    let mut copy = Cpu::default();
    copy.x = cpu.x; copy.v = cpu.v; copy.pc = cpu.pc;
    copy.fpcr = cpu.fpcr; copy.fpsr = cpu.fpsr;
    copy.tpidr_el0 = cpu.tpidr_el0; copy.tpidrro_el0 = cpu.tpidrro_el0;
    copy.set_nzcv(cpu.nzcv());
    copy
}

fn snapshot(cpu: &Cpu) -> Vec<u64> {
    let mut result = cpu.x.to_vec();
    for vector in cpu.v { result.extend(vector); }
    result.extend([cpu.nzcv() as u64, cpu.fpcr, cpu.fpsr, cpu.tpidr_el0, cpu.tpidrro_el0]);
    result
}

#[derive(Clone, Copy)]
struct Access { vector: bool, size: u32, opc: u32, bytes: usize }

fn add_zero(register: u32) -> u32 { 0x9100_0000 | (register << 5) | register }

fn register_offset(access: Access, rn: u32, rm: u32, rt: u32, option: u32, scaled: bool) -> u32 {
    0x3820_0800 | (access.size << 30) | ((access.vector as u32) << 26)
        | (access.opc << 22) | (rm << 16) | (option << 13)
        | ((scaled as u32) << 12) | (rn << 5) | rt
}

fn extended_index(value: u64, option: u32, shift: u32) -> u64 {
    let extended = match option {
        2 => value as u32 as u64,
        6 => value as u32 as i32 as i64 as u64,
        3 | 7 => value,
        _ => panic!("unsupported fixture option"),
    };
    extended.wrapping_shl(shift)
}

/// Solve index + (index << shift) == address modulo 2^64. At shift zero
/// the target is chosen even; all other coefficients are odd and invertible.
fn same_base_index(address: u64, shift: u32) -> u64 {
    if shift == 0 {
        assert_eq!(address & 1, 0);
        return address >> 1;
    }
    let coefficient = 1u64 + (1u64 << shift);
    let mut inverse = 1u64;
    for _ in 0..6 { inverse = inverse.wrapping_mul(2u64.wrapping_sub(coefficient.wrapping_mul(inverse))); }
    let index = address.wrapping_mul(inverse);
    assert_eq!(index.wrapping_add(index.wrapping_shl(shift)), address);
    index
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
    let before_counts = emit::effective_address_counts();
    let before_helpers = COMMPAGE_CALLS.load(Ordering::Relaxed);
    let mut accesses = Vec::new();
    for size in 0..=3 {
        accesses.push(Access { vector: false, size, opc: 0, bytes: 1 << size });
        accesses.push(Access { vector: false, size, opc: 1, bytes: 1 << size });
    }
    for size in 0..=2 { accesses.push(Access { vector: false, size, opc: 2, bytes: 1 << size }); }
    for size in 0..=1 { accesses.push(Access { vector: false, size, opc: 3, bytes: 1 << size }); }
    for size in 0..=3 {
        accesses.push(Access { vector: true, size, opc: 0, bytes: 1 << size });
        accesses.push(Access { vector: true, size, opc: 1, bytes: 1 << size });
    }
    accesses.push(Access { vector: true, size: 0, opc: 2, bytes: 16 });
    accesses.push(Access { vector: true, size: 0, opc: 3, bytes: 16 });
    let (mut cases, mut checkpoints, mut yields) = (0usize, 0usize, 0usize);
    let mut option_cases = [0usize; 8];
    let mut shift_cases = [0usize; 5];
    let mut shape_cases = [0usize; 6];
    let mut location_cases = [0usize; 4];
    for access in accesses {
        for option in [3u32, 2, 6, 7] {
            for scaled in [false, true] {
                let scale = if access.vector && access.opc & 2 != 0 { 4 } else { access.size };
                let shift = if scaled { scale } else { 0 };
                // normal, cache pressure (memory base), rn==rm, SP base,
                // XZR index (must reject the fast path), GPR XZR data.
                for shape in 0..6 {
                    if shape == 2 && !matches!(option, 3 | 7) { continue; }
                    if shape == 5 && access.vector { continue; }
                    for location in 0..4 {
                        let is_commpage = location == 2;
                        for (i, byte) in arena.iter_mut().enumerate() { *byte = (i as u8).wrapping_mul(37).wrapping_add(0x91); }
                        for (i, byte) in redirected.iter_mut().enumerate() { *byte = (i as u8).wrapping_mul(37).wrapping_add(0x91); }
                        // A two-page allocation permits accesses spanning a
                        // real 16-KiB boundary, with complete arena comparison.
                        let mut offset = if location == 3 { 0x4000 - (access.bytes / 2).max(1) } else { 0x500 };
                        // rn==rm with shift zero has an even address. For a
                        // byte/halfword boundary fixture, use the nearest even
                        // valid address; other fixtures still cross the page.
                        if shape == 2 && shift == 0 { offset &= !1; }
                        let raw_address = if is_commpage { mem::COMMPAGE_RO + offset as u64 }
                            else { data as u64 + offset as u64 };
                        let address = raw_address | if matches!(location, 1 | 2) { 0xa500_0000_0000_0000 } else { 0 };
                        let rn = if shape == 2 { 8 } else if shape == 3 { 31 } else { 24 };
                        let rm = if shape == 4 { 31 } else { 8 };
                        let rt = if shape == 5 { 31 } else { 10 };
                        let mut initial = Cpu::new();
                        for i in 0..32 {
                            initial.x[i] = 0xe7ab_1234_5678_0000u64.wrapping_add(i as u64 * 7919);
                            initial.v[i] = [0xfedc_ba98_7654_3210 ^ i as u64,
                                0x0123_4567_89ab_cdef ^ i as u64];
                        }
                        initial.pc = code; initial.x[20] = ITERATIONS;
                        initial.fpcr = 0; initial.fpsr = 0x0800_009f;
                        initial.set_nzcv(((cases * 7) & 15) as u32);
                        let index = if rm == 31 { 0 } else if shape == 2 { same_base_index(address, shift) }
                            else if location == 3 && matches!(option, 3 | 7) { u64::MAX - 7 }
                            else if option == 6 { 0xfeed_face_ffff_fff8 }
                            else if option == 2 { 0xfeed_face_0000_0028 } else { 40 };
                        if rm != 31 { initial.x[rm as usize] = index; }
                        if rn != rm { initial.x[rn as usize] = address.wrapping_sub(extended_index(index, option, shift)); }
                        assert_eq!(initial.x[rn as usize].wrapping_add(extended_index(index, option, shift)), address);
                        let mut program = vec![register_offset(access, rn, rm, rt, option, scaled),
                            add_zero(8), add_zero(8)];
                        if shape == 1 {
                            // Four hotter GPRs force x24 out of the fixed map
                            // while x8 remains resident for the new index path.
                            program.extend([add_zero(9), add_zero(9), add_zero(11), add_zero(11)]);
                        }
                        program.push(0xd100_0694); // SUB x20,x20,#1
                        let branch_offset = -(program.len() as i32);
                        program.push(0xb500_0014 | (((branch_offset as u32) & 0x7ffff) << 5));
                        let loop_len = program.len();
                        program.push(0xd400_1001); // terminal SVC
                        unsafe { for (i, &word) in program.iter().enumerate() { *((code as *mut u32).add(i)) = word; } }
                        invalidate_range(code, (program.len() * 4) as u64);
                        let case_emissions_before = emit::effective_address_counts();
                        let mut reference = clone_arch(&initial);
                        let mut translated = clone_arch(&initial);
                        setup_cpu(&mut translated);
                        translated.helpers[HELPER_COMMPAGE] = noisy_commpage as usize as u64;
                        let mut expected = if is_commpage { redirected.clone() } else { arena.to_vec() };
                        let budgets = [0, loop_len - 1, loop_len, 2 * loop_len - 1, 2 * loop_len, 1, 3 * loop_len];
                        let mut hops = 0usize;
                        loop {
                            translated.budget = budgets[hops.min(budgets.len() - 1)] as u64;
                            let host = lookup_or_translate(translated.pc);
                            unsafe { maclator_jit_enter(&mut translated, host); }
                            assert!(PENDING.with(|pending| pending.take()).is_none(), "EA case {cases} pending exception");
                            assert!(translated.x[20] <= reference.x[20], "EA case {cases} counter increased");
                            let completed = reference.x[20] - translated.x[20];
                            let actual = if is_commpage { redirected.clone() } else { arena.to_vec() };
                            if is_commpage { redirected.copy_from_slice(&expected); } else { arena.copy_from_slice(&expected); }
                            for _ in 0..completed {
                                assert_eq!(reference.pc, code);
                                for &word in &program[..loop_len] {
                                    interp::exec(&mut reference, word).unwrap_or_else(|error|
                                        panic!("invalid EA reference case {cases} word={word:x}: {error:?}"));
                                }
                            }
                            expected = if is_commpage { redirected.clone() } else { arena.to_vec() };
                            assert_eq!(snapshot(&translated), snapshot(&reference),
                                "EA case {cases} option={option} shift={shift} shape={shape} location={location} checkpoint={hops}");
                            assert_eq!(actual, expected, "EA case {cases} complete memory checkpoint={hops}");
                            if is_commpage { redirected.copy_from_slice(&actual); } else { arena.copy_from_slice(&actual); }
                            checkpoints += 1;
                            if translated.exit_reason == EXIT_SVC {
                                assert_eq!(translated.x[20], 0); break;
                            }
                            assert_eq!(translated.exit_reason, EXIT_CONTINUE);
                            assert_eq!(translated.pc, reference.pc);
                            yields += 1; hops += 1;
                            assert!(hops < 16, "EA case {cases} did not finish");
                        }
                        let case_emissions_after = emit::effective_address_counts();
                        let eligible = emit::effective_address() && emit::loop_cache()
                            && option == 3 && rm != 31 && shift <= 3;
                        let emitted = case_emissions_after[0] + case_emissions_after[1]
                            - case_emissions_before[0] - case_emissions_before[1];
                        if eligible { assert!(emitted != 0, "EA eligible case {cases} did not emit the new path"); }
                        else { assert_eq!(emitted, 0, "EA rejected case {cases} emitted the new path"); }
                        cases += 1; option_cases[option as usize] += 1;
                        shift_cases[shift as usize] += 1; shape_cases[shape] += 1; location_cases[location] += 1;
                    }
                }
            }
        }
    }
    let after_counts = emit::effective_address_counts();
    let emissions = [after_counts[0] - before_counts[0], after_counts[1] - before_counts[1]];
    let helper_calls = COMMPAGE_CALLS.load(Ordering::Relaxed) - before_helpers;
    assert!(helper_calls != 0, "EA synthetic commpage helper was not called");
    if emit::effective_address() && emit::loop_cache() {
        assert!(emissions[0] != 0 && emissions[1] != 0, "EA cached and memory-base code paths need real emission coverage");
    } else { assert_eq!(emissions, [0, 0], "disabled EA paths emitted code"); }
    mem::COMMPAGE_REDIRECT.store(saved_redirect, Ordering::Relaxed);
    unsafe { libc::munmap(code as *mut _, 4096); libc::munmap(data, DATA_LEN); }
    println!("jit selftest [effectiveaddress]: ok={cases} mismatch=0 checkpoints={checkpoints} yields={yields} cached_base_emissions={} memory_base_emissions={} helper_calls={helper_calls} options={option_cases:?} shifts={shift_cases:?} shapes={shape_cases:?} locations={location_cases:?}", emissions[0], emissions[1]);
}
