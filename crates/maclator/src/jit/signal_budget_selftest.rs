//! NZCV at the independent signal-delivery budget boundary.
//!
//! This probe exercises flag stores that the old dead-flag analysis would omit. It compares each
//! external JIT return with a separately stepped interpreter, including returns
//! before any instruction and between a loop backedge and the next flag setter.
//! A final fallthrough CMP deliberately makes the loop's flags look dead to
//! both successors, so an end-of-program-only check cannot cover this boundary.

use super::*;
use std::fmt::Write as FmtWrite;
use std::io::Write;

const BLOCK_INSNS: u64 = emit::budget_charge(2);

fn quoted(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""), '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"), '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch < '\u{20}' => write!(out, "\\u{:04x}", ch as u32).unwrap(),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn clone_arch(cpu: &Cpu) -> Cpu {
    let mut out = Cpu::default();
    out.x = cpu.x;
    out.v = cpu.v;
    out.pc = cpu.pc;
    out.set_nzcv(cpu.nzcv());
    out.fpcr = cpu.fpcr;
    out.fpsr = cpu.fpsr;
    out.tpidr_el0 = cpu.tpidr_el0;
    out.tpidrro_el0 = cpu.tpidrro_el0;
    out.excl_addr = cpu.excl_addr;
    out.excl_val = cpu.excl_val;
    out.excl_val2 = cpu.excl_val2;
    out.excl_size = cpu.excl_size;
    out
}

fn architectural_fields(cpu: &Cpu) -> Vec<(String, u64)> {
    let mut fields = Vec::new();
    for n in 0..32 {
        fields.push((format!("x{n}"), cpu.x[n]));
        fields.push((format!("v{n}.lo"), cpu.v[n][0]));
        fields.push((format!("v{n}.hi"), cpu.v[n][1]));
    }
    fields.extend([
        ("pc".into(), cpu.pc), ("nzcv".into(), cpu.nzcv() as u64),
        ("nf".into(), cpu.nf as u64), ("zf".into(), cpu.zf as u64),
        ("cf".into(), cpu.cf as u64), ("vf".into(), cpu.vf as u64),
        ("fpcr".into(), cpu.fpcr), ("fpsr".into(), cpu.fpsr),
        ("tpidr_el0".into(), cpu.tpidr_el0), ("tpidrro_el0".into(), cpu.tpidrro_el0),
        ("excl_addr".into(), cpu.excl_addr), ("excl_val".into(), cpu.excl_val),
        ("excl_val2".into(), cpu.excl_val2), ("excl_size".into(), cpu.excl_size),
    ]);
    fields
}

/// Simulate one *JIT entry's* architectural execution, using only the reference
/// interpreter. A unit contains one two-instruction block charged at MAX_BLOCK_INSNS. The loop's backedge
/// stays within that unit; fallthrough exits through its unchained stub. The
/// following CMP/SVC unit is entered only on a subsequent external call.
/// No actual JIT state is used to decide how far the reference should execute.
fn reference_entry(cpu: &mut Cpu, entry: u64, words: &[u32; 4], mut budget: u64) -> u64 {
    assert!(cpu.pc == entry || cpu.pc == entry + 8, "unexpected reference block entry");
    if cpu.pc == entry + 8 {
        if budget < BLOCK_INSNS { return EXIT_CONTINUE; }
        assert!(interp::exec(cpu, words[2]).is_ok());
        match interp::exec(cpu, words[3]) {
            Err(interp::Exit::Svc(_)) => return EXIT_SVC,
            result => panic!("reference tail SVC: {result:?}"),
        }
    }
    while budget >= BLOCK_INSNS {
        budget -= BLOCK_INSNS;
        assert!(interp::exec(cpu, words[0]).is_ok());
        assert!(interp::exec(cpu, words[1]).is_ok());
        if cpu.pc != entry {
            assert_eq!(cpu.pc, entry + 8, "reference branch fallthrough");
            return EXIT_CONTINUE;
        }
    }
    EXIT_CONTINUE
}

pub fn run() {
    assert!(std::env::var("MACLATOR_JIT_DISABLE").unwrap_or_default().is_empty(),
        "signal-budget needs normal instruction translation and dead-flag optimization");
    assert!(std::env::var_os("MACLATOR_JIT_VERIFY").is_none(),
        "signal-budget controls multi-iteration entry boundaries itself");
    let page = unsafe { libc::mmap(std::ptr::null_mut(), 4096,
        libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
    assert_ne!(page, libc::MAP_FAILED);
    let entry = page as u64;
    // ADDS x0,x0,#1; B.CC -4 and SUBS x0,x0,#1; B.CS -4.
    // Both execute exactly three iterations from these inputs. On both branch
    // successors, flags_dead_at sees an unconditional flag setter immediately.
    let programs = [
        ("adds-carry-clear", u64::MAX - 2, [0xb100_0400, 0x54ff_ffe3, 0xf100_043f, 0xd400_1001]),
        ("subs-carry-set", 2, [0xf100_0400, 0x54ff_ffe2, 0xf100_043f, 0xd400_1001]),
    ];
    // One-shot entries distinguish no execution, one or two complete rounds,
    // and fallthrough. A generous budget still returns before the separate tail.
    let q = BLOCK_INSNS;
    let one_shot = [0u64, q-1, q, q+1, 2*q-1, 2*q, 2*q+1, 3*q-1, 3*q, 3*q+1, 4*q];
    // Repeated entries expose signal/dispatcher-visible state after every
    // backedge, then prove that tail CMP can hide earlier stale-NZCV mistakes.
    let schedules: &[(&str, &[u64])] = &[
        ("one-round-with-zero-gaps", &[0, BLOCK_INSNS, 0, BLOCK_INSNS, 0, BLOCK_INSNS, 0, BLOCK_INSNS]),
        ("one-round-with-short-gaps", &[BLOCK_INSNS-1, BLOCK_INSNS, BLOCK_INSNS-1, BLOCK_INSNS, BLOCK_INSNS-1, BLOCK_INSNS, BLOCK_INSNS-1, BLOCK_INSNS]),
        ("two-rounds-then-reentry", &[2*BLOCK_INSNS, 0, BLOCK_INSNS, 0, BLOCK_INSNS]),
    ];
    let (mut checkpoints, mut mismatches, mut skipped_flag_store_errors) = (0, 0, 0);
    for (name, initial_x0, words) in programs {
        unsafe { for (i, word) in words.iter().enumerate() { *((page as *mut u32).add(i)) = *word; } }
        invalidate_range(entry, 16);
        let mut init = Cpu::default();
        for n in 0..32 {
            init.x[n] = 0x1234_5678_0000_0000u64.wrapping_add(n as u64);
            init.v[n] = [0xdead_beef_0000_0000u64 + n as u64, 0xcafe_babe_0000_0000u64 + n as u64];
        }
        init.x[0] = initial_x0;
        init.x[1] = 0; // CMP x1,#1 sets NZCV=8, overwriting final loop flags.
        init.pc = entry;
        init.set_nzcv(0xb); // Deliberately distinct from every first-round result.
        init.fpsr = 0x0800_009f;
        init.tpidr_el0 = 0x1000;
        init.tpidrro_el0 = 0x2000;
        let host = lookup_or_translate(entry);
        let skips = emit::DEAD_FLAG_SKIPS.load(Ordering::Relaxed);
        if skips != 0 { skipped_flag_store_errors += 1; }
        println!("signal-budget program={name} charge={BLOCK_INSNS} skipped_flag_stores={skips}");
        let mut cases: Vec<(String, Vec<u64>)> = one_shot.iter()
            .map(|&budget| (format!("fresh-{budget}"), vec![budget])).collect();
        cases.extend(schedules.iter().map(|(label, budgets)| ((*label).into(), budgets.to_vec())));
        for (case, budgets) in cases {
            let mut expected = clone_arch(&init);
            let mut actual = clone_arch(&init);
            setup_cpu(&mut actual);
            for (checkpoint, budget) in budgets.into_iter().enumerate() {
                let start_pc = expected.pc;
                let expected_exit = reference_entry(&mut expected, entry, &words, budget);
                actual.budget = budget;
                actual.exit_reason = EXIT_CONTINUE;
                actual.exit_data = 0;
                let target = if actual.pc == entry { host } else { lookup_or_translate(actual.pc) };
                unsafe { maclator_jit_enter(&mut actual, target); }
                let pending = PENDING.with(|p| p.take()).is_some();
                let a = architectural_fields(&actual);
                let e = architectural_fields(&expected);
                let differences: Vec<_> = e.iter().zip(&a).filter_map(|((field, want), (_, got))|
                    if want != got { Some(format!("{{\"field\":{},\"expected\":{},\"actual\":{}}}",
                        quoted(field), want, got)) } else { None }).collect();
                let mismatch = !differences.is_empty() || actual.exit_reason != expected_exit || pending;
                checkpoints += 1;
                if mismatch { mismatches += 1; }
                println!("{{\"probe\":\"signal-budget-checkpoint\",\"program\":{},\"case\":{},\"checkpoint\":{},\"budget\":{},\"start_pc\":{},\"expected_pc\":{},\"actual_pc\":{},\"expected_nzcv\":{},\"actual_nzcv\":{},\"expected_x0\":{},\"actual_x0\":{},\"expected_exit\":{},\"actual_exit\":{},\"pending\":{},\"mismatch\":{},\"differences\":[{}]}}",
                    quoted(name), quoted(&case), checkpoint, budget, start_pc,
                    expected.pc, actual.pc, expected.nzcv(), actual.nzcv(),
                    expected.x[0], actual.x[0], expected_exit, actual.exit_reason,
                    pending, mismatch, differences.join(","));
                if expected_exit == EXIT_SVC || actual.exit_reason == EXIT_SVC || pending { break; }
                // Do not try to execute unknown code after an unrelated PC bug.
                if actual.pc != entry && actual.pc != entry + 8 { break; }
            }
        }
    }
    unsafe { libc::munmap(page, 4096); }
    println!("jit selftest [signal-budget]: checkpoints={checkpoints} mismatch={mismatches} skipped_flag_store_errors={skipped_flag_store_errors}");
    std::io::stdout().flush().unwrap();
    if mismatches != 0 || skipped_flag_store_errors != 0 { std::process::exit(1); }
}
