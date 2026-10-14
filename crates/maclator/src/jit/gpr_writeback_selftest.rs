//! Bounded fixed-GPR writeback oracle. This is prepared source, not validation
//! evidence until each separate OFF/ON and budget-mode process is executed.
//! Accepted loops deliberately contain no memory or helpers. The actual
//! runtime chain_slot path is exercised separately from all-internal units.

use super::*;
use iced_x86::BlockEncoderOptions;

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
    out.extend([cpu.pc, cpu.nzcv() as u64, cpu.fpcr, cpu.fpsr,
        cpu.tpidr_el0, cpu.tpidrro_el0, cpu.excl_addr, cpu.excl_val,
        cpu.excl_val2, cpu.excl_size, cpu.nf as u64, cpu.zf as u64,
        cpu.cf as u64, cpu.vf as u64]);
    out
}
fn add_imm(rd: u32, rn: u32, immediate: i32, sf: bool, flags: bool) -> u32 {
    0x1100_0000 | ((sf as u32) << 31) | (((immediate < 0) as u32) << 30)
        | ((flags as u32) << 29) | (immediate.unsigned_abs() << 10) | (rn << 5) | rd
}
fn backward_branch(words: usize, counter: u32, sf: bool, kind: u32) -> u32 {
    let offset = -(words as i32);
    match kind {
        0 => 0x3500_0000 | ((sf as u32) << 31) | ((offset as u32 & 0x7ffff) << 5) | counter,
        1 => 0x5400_0001 | ((offset as u32 & 0x7ffff) << 5), // B.NE
        2 => 0x3700_0000 | ((offset as u32 & 0x3fff) << 5) | counter, // TBNZ bit0
        _ => 0x1400_0000 | (offset as u32 & 0x03ff_ffff), // bounded only by budget
    }
}
fn integer_loop(base: u32, counter: u32, sf: bool, kind: u32) -> Vec<u32> {
    let size = (sf as u32) << 31;
    let mut words = vec![
        add_imm(base, base, 3, sf, false),
        0x4a00_0000 | size | (base << 16) | ((base + 1) << 5) | (base + 1),
        0x5280_0000 | size | (0xfedc << 5) | (base + 2), // MOVZ
        0x7280_0000 | size | ((if sf { 3 } else { 1 }) << 21)
            | (0x8123 << 5) | (base + 2), // MOVK, including high X immediate
        0x0b00_0000 | size | ((base + 2) << 16) | ((base + 3) << 5) | (base + 3),
        add_imm(base + 4, base + 4, 1, sf, false), // fifth GPR, may remain unmapped
        0x1b00_0000 | size | ((base + 2) << 16) | (base << 10)
            | ((base + 1) << 5) | base, // MADD; rd aliases addend
        add_imm(29, 29, 1, true, false),
        add_imm(30, 30, 1, true, false),
        add_imm(31, 31, 16, true, false), // SP stays write-through
        add_imm(counter, counter, -1, sf, false),
        add_imm(31, counter, 0, sf, true), // CMP, NZCV always authoritative
    ];
    words.push(backward_branch(words.len(), counter, sf, kind));
    words
}
fn write_blocks(blocks: &[(u64, Vec<u32>)]) {
    unsafe { for (pc, words) in blocks { for (i, &word) in words.iter().enumerate() {
        *((*pc as *mut u32).add(i)) = word;
    } } }
}
fn read_blocks(blocks: &[(u64, Vec<u32>)], pc: u64) -> Option<u32> {
    blocks.iter().find_map(|(start, words)| pc.checked_sub(*start)
        .filter(|offset| offset % 4 == 0)
        .and_then(|offset| words.get((offset / 4) as usize).copied()))
}
/// Independent block-entry charging, including the wrapping SUB on the
/// underflowing entry. Unknown successor means an unpatched external exit.
fn reference_entry(cpu: &mut Cpu, blocks: &[(u64, Vec<u32>)], mut budget: u64) -> (u64, u64) {
    for _ in 0..256 {
        let Some((_, words)) = blocks.iter().find(|(pc, _)| *pc == cpu.pc) else {
            return (EXIT_CONTINUE, budget);
        };
        let underflow = budget < words.len() as u64;
        budget = budget.wrapping_sub(words.len() as u64);
        if underflow { return (EXIT_CONTINUE, budget); }
        for &word in words {
            match interp::exec(cpu, word) {
                Ok(()) => (),
                Err(Exit::Svc(_)) => return (EXIT_SVC, budget),
                other => panic!("gpr reference {word:08x}: {other:?}"),
            }
        }
    }
    panic!("unbounded GPR oracle fixture");
}
fn seeded_cpu(rng: &mut Rng, pc: u64, sf: bool) -> Cpu {
    let mut cpu = Cpu::default();
    for i in 0..32 { cpu.x[i] = rng.next(); cpu.v[i] = [rng.next(), rng.next()]; }
    cpu.pc = pc;
    cpu.x[20] = if sf { 3 } else { 0xabcd_ef01_0000_0003 };
    cpu.x[21] = if sf { 4 } else { 0x1234_5678_0000_0004 };
    cpu.fpcr = rng.next() & 0xffff_ffff; cpu.fpsr = rng.next() & 0xffff_ffff;
    cpu.tpidr_el0 = rng.next(); cpu.tpidrro_el0 = rng.next();
    cpu.excl_addr = rng.next(); cpu.excl_val = rng.next();
    cpu.excl_val2 = rng.next(); cpu.excl_size = 16;
    cpu.set_nzcv((rng.next() & 15) as u32);
    cpu
}
fn assert_boundary(actual: &Cpu, reference: &Cpu, reason: u64, remaining: u64, name: &str) {
    assert!(PENDING.with(|pending| pending.take()).is_none(), "{name}: unexpected pending exit");
    assert_eq!(snapshot(actual), snapshot(reference), "{name}: full architectural state including PC");
    assert_eq!(actual.exit_reason, reason, "{name}: exit reason");
    assert_eq!(actual.budget, remaining, "{name}: charged/underflowing budget");
}
fn internal_unit(blocks: &[(u64, Vec<u32>)], kind: StubKind) -> (u64, Vec<(u64, u64)>) {
    let mut unit = Unit::new(kind, false);
    for (pc, _) in blocks { let l = unit.a.create_label(); unit.labels.insert(*pc, l); }
    let mut maps = Vec::new();
    for (pc, words) in blocks {
        emit::emit_block(&mut unit, *pc, &|address| read_blocks(blocks, address));
        if emit::gpr_writeback_eligibility(words).is_ok() {
            assert_eq!(unit.gpr_writeback, emit::gpr_writeback(), "fixture must accept the integer plan when ON");
            if unit.gpr_writeback {
                assert!(unit.rc.reg.iter().all(|&reg| reg == 0xff || reg < 29));
                assert!(unit.rc.reg.iter().any(|&reg| reg != 0xff));
                assert!(!unit.scalar_writeback);
                maps.push(unit.rc.reg);
            }
        }
    }
    if emit::gpr_writeback() { assert_eq!(maps.len(), 2); assert_ne!(maps[0], maps[1]); }
    assert_eq!(unit.insns_fallback, 0, "accepted fixtures have no interpreter helpers");
    unit.finish();
    assert!(unit.finished_stubs.is_empty(), "internal unit is not the external chain oracle");
    let cache = code_cache(); let host = cache.cursor();
    let assembled = unit.a.assemble_options(host, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS)
        .expect("GPR two-map assembly");
    let entries = blocks.iter().map(|(pc, _)| (*pc, assembled.label_ip(&unit.labels[pc]).unwrap())).collect();
    let bytes = &assembled.inner.code_buffer;
    assert_eq!(cache.alloc(bytes.len() as u64), host);
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), host as *mut u8, bytes.len()); }
    (host, entries)
}
fn rejection_oracle(page: u64) -> usize {
    let forbidden = [
        0xf940_0000, // LDR
        0xa940_0400, // LDP
        0xc8df_fc00, // LDAR
        0xc85f_7c00, // LDXR
        0x1f41_0802, // FMADD
        0x4e20_8400, // vector ADD
        0xd53b_d040, // MRS TPIDR_EL0
        0xd503_201f, // NOP/hint
        0x1e60_2800, // scalar FP ADD with helper guard
        0x5300_7c00, // bitfield outside v1 whitelist
        0x1ac0_0800, // division outside v1 whitelist
    ];
    for &word in &forbidden {
        let words = vec![word, add_imm(20, 20, -1, true, false), backward_branch(2, 20, true, 0)];
        assert!(emit::gpr_writeback_eligibility(&words).is_err(), "unexpected whitelist admission {word:08x}");
        let mut unit = Unit::new(StubKind::Plain, false);
        let l = unit.a.create_label(); unit.labels.insert(page, l);
        emit::emit_block(&mut unit, page, &|pc| pc.checked_sub(page)
            .filter(|off| off % 4 == 0).and_then(|off| words.get((off / 4) as usize).copied()));
        assert!(!unit.gpr_writeback, "rejected loop activated writeback {word:08x}");
    }
    for terminal in [0x9400_0000, 0xd65f_03c0, 0xd400_1001, 0x5400_0011] {
        assert!(emit::gpr_writeback_eligibility(&[add_imm(0, 0, 1, true, false), terminal]).is_err());
    }
    // Wrong-target branch has an allowed instruction class but must fail the
    // entire-selfloop geometry check. No runtime executes any rejected code.
    let words = [add_imm(0, 0, 1, true, false), 0xb500_0020];
    let mut unit = Unit::new(StubKind::Plain, false);
    let l = unit.a.create_label(); unit.labels.insert(page, l);
    emit::emit_block(&mut unit, page, &|pc| pc.checked_sub(page)
        .filter(|off| off % 4 == 0).and_then(|off| words.get((off / 4) as usize).copied()));
    assert!(!unit.gpr_writeback, "non-selfloop geometry admitted");
    // A reserved 32-bit shift reaches native preflight rejection.
    let words = [0x4a00_8000, backward_branch(1, 20, true, 0)];
    let mut unit = Unit::new(StubKind::Plain, false);
    let l = unit.a.create_label(); unit.labels.insert(page, l);
    emit::emit_block(&mut unit, page, &|pc| pc.checked_sub(page)
        .filter(|off| off % 4 == 0).and_then(|off| words.get((off / 4) as usize).copied()));
    assert!(!unit.gpr_writeback, "invalid native instruction admitted");
    forbidden.len() + 6
}
/// Normal runtime translation registers/initializes real external slots in
/// INDEX. chain_slot must patch that owned slot and the following single JIT
/// entry must execute the second, differently mapped loop without dispatch.
fn chain_and_reentry_oracle(page: u64, rng: &mut Rng) -> (usize, usize, usize) {
    let first = integer_loop(0, 20, true, 0);
    let second_pc = page + first.len() as u64 * 4;
    let second = integer_loop(10, 21, true, 0);
    let end = second_pc + second.len() as u64 * 4;
    let blocks = vec![(page, first), (second_pc, second), (end, vec![0xd400_1001])];
    write_blocks(&blocks); invalidate_range(page, 4096);
    let source_host = lookup_or_translate(page);
    let target_host = lookup_or_translate(second_pc);
    let initial = seeded_cpu(rng, page, true);
    let budget = 128u64;
    let mut reference = clone_arch(&initial);
    let (reason, remain) = reference_entry(&mut reference, &blocks[..1], budget);
    let mut actual = clone_arch(&initial); setup_cpu(&mut actual); actual.budget = budget;
    unsafe { maclator_jit_enter(&mut actual, source_host); }
    assert_boundary(&actual, &reference, reason, remain, "unpatched source continuation");
    assert_eq!(actual.pc, second_pc);
    let slot = actual.exit_data;
    assert_ne!(slot, 0, "source requires a real external patchable slot");
    assert_eq!(slot & 7, 0, "runtime slot alignment");
    assert!(INDEX.lock().unwrap().as_ref().unwrap().slot_cont.contains_key(&slot), "owned slot registration");
    assert_ne!(unsafe { std::ptr::read_volatile(slot as *const u64) }, target_host);
    chain_slot(slot, second_pc, target_host);
    assert_eq!(unsafe { std::ptr::read_volatile(slot as *const u64) }, target_host, "chain_slot patch must succeed");
    let mut checked = 1;
    let mut transfers = 0;
    for budget in 0..=128u64 {
        let mut reference = clone_arch(&initial);
        let (reason, remain) = reference_entry(&mut reference, &blocks[..2], budget);
        let mut actual = clone_arch(&initial); setup_cpu(&mut actual); actual.budget = budget;
        unsafe { maclator_jit_enter(&mut actual, source_host); }
        assert_boundary(&actual, &reference, reason, remain, "patched external transfer");
        if actual.pc == end {
            assert_eq!(actual.x[21], 0, "target loop must execute through the patched source slot");
            transfers += 1;
        }
        checked += 1;
    }
    assert!(transfers > 0, "patched entry must really run the target");
    // Re-enter the public label after changing both loop accumulators in Cpu.
    // This models the state replacement requirement of guest sigreturn; the
    // separate ARM fixture exercises the actual saved signal frame protocol.
    let mut reference = clone_arch(&initial);
    let mut actual = clone_arch(&initial); setup_cpu(&mut actual);
    let mut reentries = 0;
    for budget in [26u64, 13, 13, 13, 13, 13, 13, 13, 1] {
        reference.x[0] = reference.x[0].wrapping_add(1024);
        reference.x[10] = reference.x[10].wrapping_add(2048);
        actual.x[0] = actual.x[0].wrapping_add(1024);
        actual.x[10] = actual.x[10].wrapping_add(2048);
        let pc = actual.pc;
        let (reason, remain) = reference_entry(&mut reference, &blocks[..2], budget);
        actual.budget = budget;
        let public_host = lookup_or_translate(pc);
        unsafe { maclator_jit_enter(&mut actual, public_host); }
        assert_boundary(&actual, &reference, reason, remain, "public reload after Cpu edit");
        reentries += 1; checked += 1;
        if actual.pc == end { break; }
    }
    assert_eq!(actual.pc, end, "public reentry fixture did not finish both loops");
    assert!(reentries > 1);
    (checked, transfers, reentries)
}

pub fn run() {
    assert!(emit::loop_cache());
    assert!(!emit::scalar_writeback(), "isolate GPR experiment from scalar writeback");
    assert!(std::env::var_os("MACLATOR_JIT_VERIFY").is_none());
    assert!(std::env::var("MACLATOR_JIT_DISABLE").unwrap_or_default().is_empty());
    let raw = unsafe { libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
    assert_ne!(raw, libc::MAP_FAILED);
    let page = raw as u64;
    let before_blocks = emit::GPR_WRITEBACK_BLOCKS.load(Ordering::Relaxed);
    let before_stores = emit::GPR_WRITEBACK_SKIPPED_STORES.load(Ordering::Relaxed);
    let mut rng = Rng(0x44f1_55c3_a062_d80b);
    let mut checked = 0;
    for kind in [StubKind::Plain, StubKind::Patchable] {
        for sf in [false, true] { for branch in 0..4u32 {
            let first = integer_loop(0, 20, sf, branch);
            let second = integer_loop(10, 21, sf, branch);
            let bridge = page + first.len() as u64 * 4;
            let next = page + 0x100;
            let end = next + second.len() as u64 * 4;
            let blocks = vec![(page, first),
                (bridge, vec![0x1400_0000 | (((next - bridge) / 4) as u32)]),
                (next, second), (end, vec![0xd400_1001])];
            write_blocks(&blocks);
            let code_before = unsafe { std::slice::from_raw_parts(raw as *const u8, 4096) }.to_vec();
            let (_, entries) = internal_unit(&blocks, kind);
            for start in [page, next] { for _ in 0..4 {
                let initial = seeded_cpu(&mut rng, start, sf);
                for budget in 0..=128u64 {
                    let mut reference = clone_arch(&initial);
                    let (reason, remain) = reference_entry(&mut reference, &blocks, budget);
                    let mut actual = clone_arch(&initial); setup_cpu(&mut actual); actual.budget = budget;
                    let host = entries.iter().find(|(pc, _)| *pc == start).unwrap().1;
                    unsafe { maclator_jit_enter(&mut actual, host); }
                    assert_boundary(&actual, &reference, reason, remain, "two-map block budget");
                    assert_eq!(unsafe { std::slice::from_raw_parts(raw as *const u8, 4096) }, &code_before[..],
                        "pure ALU fixture unexpectedly changed guest code memory");
                    checked += 1;
                }
            } }
        } }
    }
    let rejected = rejection_oracle(page + 0x800);
    let (chain_checked, transfers, reentries) = chain_and_reentry_oracle(page, &mut rng);
    checked += chain_checked;
    invalidate_range(page, 4096);
    unsafe { libc::munmap(raw, 4096); }
    let blocks = emit::GPR_WRITEBACK_BLOCKS.load(Ordering::Relaxed) - before_blocks;
    let stores = emit::GPR_WRITEBACK_SKIPPED_STORES.load(Ordering::Relaxed) - before_stores;
    if emit::gpr_writeback() { assert!(blocks > 0 && stores > 0); }
    else { assert_eq!((blocks, stores), (0, 0), "OFF must retain original stores"); }
    println!("jit selftest [gprwriteback]: checked={checked} mismatch=0 blocks={blocks} skipped_cpu_store_emissions={stores} chained_transfers={transfers} public_reentries={reentries} rejected_shapes={rejected}");
}
