//! Opt-in, compilation-time JIT attribution. No execution instrumentation.
//!
//! iced-x86 reports u32::MAX for rewritten instructions. Keep such offsets
//! unknown rather than guessing host lengths from pre-assembly instruction size.

use super::emit::Unit;
use iced_x86::code_asm::CodeAssemblerResult;
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

pub(super) struct GuestAssemblyRange {
    pub pc: u64,
    pub word: u32,
    pub asm_start: usize,
    pub asm_end: usize,
    pub native: bool,
}

pub(super) struct RetAssemblyRange {
    pub pc: u64,
    pub asm_start: usize,
    pub asm_hit_start: usize,
    pub asm_hit_end: usize,
    pub asm_end: usize,
}

fn output_path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| std::env::var_os("MACLATOR_DIAGNOSTIC_JIT_MAP")
        .filter(|path| !path.is_empty()).map(PathBuf::from)).as_ref()
}

pub(super) fn enabled() -> bool { output_path().is_some() }

fn all_units() -> bool {
    static ALL: OnceLock<bool> = OnceLock::new();
    *ALL.get_or_init(|| std::env::var("MACLATOR_DIAGNOSTIC_JIT_MAP_SCOPE").as_deref() == Ok("all"))
}

static MAIN_IMAGE: OnceLock<(u64, u64)> = OnceLock::new();

pub(crate) fn set_main_image(start: u64, end: u64) {
    if enabled() { let _ = MAIN_IMAGE.set((start, end)); }
}

fn writer() -> Option<&'static Mutex<File>> {
    static WRITER: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    WRITER.get_or_init(|| {
        let path = output_path()?;
        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => Some(Mutex::new(file)),
            Err(error) => {
                eprintln!("maclator: diagnostic JIT map unavailable: {error}");
                None
            }
        }
    }).as_ref()
}

fn offset(offsets: &[u32], index: usize) -> Option<u64> {
    offsets.get(index).copied().filter(|&value| value != u32::MAX).map(u64::from)
}

fn interval(offsets: &[u32], start: usize, end: usize, code_end: Option<u64>, host: u64) -> Value {
    let lo = offset(offsets, start);
    let hi = offset(offsets, end);
    let exact = match (lo, hi, code_end) {
        (Some(lo), Some(hi), Some(code_end)) => lo <= hi && hi <= code_end,
        _ => false,
    };
    let rewritten = offsets.get(start..end).map(|slice|
        slice.iter().any(|&value| value == u32::MAX)).unwrap_or(true);
    json!({
        "asm_start": start, "asm_end": end,
        "boundary_offsets": [lo, hi],
        "host_start": if exact { lo.map(|value| host + value) } else { None },
        "host_len": if exact { lo.zip(hi).map(|(lo, hi)| hi - lo) } else { None },
        "contains_rewritten_instruction": rewritten,
        "status": if !exact { "unknown_boundary" } else if rewritten {
            "exact_boundaries_rewritten_interior"
        } else { "exact" },
        "instruction_offsets": offsets.get(start..end).map(|slice| slice.iter().map(|&value|
            if value == u32::MAX { None } else { Some(value) }).collect::<Vec<_>>()),
    })
}

pub(super) fn export_unit(unit: &Unit, assembled: &CodeAssemblerResult, guest: u64, host: u64) {
    let Some(guests) = unit.diagnostic_guests.as_ref() else { return; };
    if !all_units() {
        if let Some(&(start, end)) = MAIN_IMAGE.get() {
            if guest < start || guest >= end { return; }
        }
    }
    let Some(writer) = writer() else { return; };
    let offsets = &assembled.inner.new_instruction_offsets;
    let bytes = &assembled.inner.code_buffer;
    let first_slot = unit.first_slot.and_then(|label| assembled.label_ip(&label).ok())
        .and_then(|address| address.checked_sub(host));
    // finish emits slot_padding raw INT3 bytes immediately before the first
    // chaining slot. Neither padding nor mutable pointer slots are code here.
    let code_end = if unit.first_slot.is_some() {
        first_slot.and_then(|offset| offset.checked_sub(unit.slot_padding as u64))
    } else if offsets.iter().any(|&value| value == u32::MAX) {
        // Rewritten far branches can append pointer data. Without a slot
        // boundary, do not label the entire allocation as executable bytes.
        None
    } else { Some(bytes.len() as u64) };
    let code_end = code_end.filter(|&offset| offset <= bytes.len() as u64);
    let code_hex = code_end.map(|end| bytes[..end as usize].iter()
        .map(|byte| format!("{byte:02x}")).collect::<String>());
    let guest_ranges: Vec<Value> = guests.iter().map(|range| json!({
        "guest_pc": range.pc, "guest_word": format!("{:08x}", range.word),
        "ordinary_ret_x30": range.word == 0xd65f_03c0,
        "native_translation": range.native,
        "host_interval": interval(offsets, range.asm_start, range.asm_end, code_end, host),
    })).collect();
    let ret_ranges: Vec<Value> = unit.diagnostic_rets.iter().map(|range| {
        let word = guests.iter().find(|guest| guest.pc == range.pc).map(|guest| guest.word);
        json!({
            "guest_pc": range.pc, "guest_word": word.map(|word| format!("{word:08x}")),
            "ordinary_ret_x30": word == Some(0xd65f_03c0),
            "lookup_shared_interval": interval(offsets, range.asm_start, range.asm_hit_start, code_end, host),
            "lookup_hit_only_interval": interval(offsets, range.asm_hit_start, range.asm_hit_end, code_end, host),
            "lookup_hit_interval": interval(offsets, range.asm_start, range.asm_hit_end, code_end, host),
            "lookup_miss_interval": interval(offsets, range.asm_hit_end, range.asm_end, code_end, host),
            "lookup_full_interval": interval(offsets, range.asm_start, range.asm_end, code_end, host),
        })
    }).collect();
    let stubs: Vec<Value> = unit.finished_stubs.iter().map(|stub| json!({
        "guest_target": stub.target,
        "host_continuation": assembled.label_ip(&stub.cont).ok(),
        "host_slot": assembled.label_ip(&stub.slot).ok(),
    })).collect();
    let timestamp_ns = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .ok().map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64);
    let record = json!({
        "schema": "maclator-jit-map-v1", "event": "jit_unit_published",
        "pid": std::process::id(), "unix_time_ns": timestamp_ns,
        "jit_epoch": super::EPOCH.load(std::sync::atomic::Ordering::Relaxed),
        "main_image_range": MAIN_IMAGE.get().map(|&(start, end)| [start, end]),
        "filter": if all_units() { "all_units" } else if MAIN_IMAGE.get().is_some() { "main_image_only" } else { "main_image_unavailable_no_filter" },
        "guest_entry": guest, "host_start": host,
        "allocation_len": bytes.len(), "code_len": code_end,
        "code_bytes_hex": code_hex,
        "excluded_padding_slot_range": code_end.map(|offset| [host + offset, host + bytes.len() as u64]),
        "slot_table_start": first_slot.map(|offset| host + offset),
        "slot_padding_bytes": unit.slot_padding,
        "common_exit_start": unit.diagnostic_body_end.and_then(|index| offset(offsets, index)).map(|offset| host + offset),
        "common_exit_interval": unit.diagnostic_body_end.zip(unit.diagnostic_exit_end)
            .map(|(start, end)| interval(offsets, start, end, code_end, host)),
        "cold_or_stub_start": unit.diagnostic_exit_end.and_then(|index| offset(offsets, index)).map(|offset| host + offset),
        "assembler_instruction_count": unit.a.instructions().len(),
        "new_offset_count": offsets.len(),
        "new_instruction_offsets": offsets.iter().map(|&value|
            if value == u32::MAX { None } else { Some(value) }).collect::<Vec<_>>(),
        "guest_ranges": guest_ranges, "ret_lookup_ranges": ret_ranges, "stubs": stubs,
        "gpr_writeback": {
            "feature_enabled": super::emit::gpr_writeback(),
            "plans": unit.diagnostic_gpr_plans.iter().map(|&(pc, regs, enabled, reason, classes)| json!({
                "guest_pc": pc, "fixed_gpr_map": regs, "enabled": enabled, "reason": reason,
                "instruction_class_eligibility": classes,
            })).collect::<Vec<_>>(),
            "omitted_store_guest_pcs": unit.diagnostic_gpr_store_sites,
            "omitted_store_emission_count": unit.diagnostic_gpr_store_sites.len(),
            "limits": "Compile-time eligibility and omitted store sites, not execution counts. Host debug/fatal dumps may contain older mapped X0..28 mixed with newer unmapped/NZCV state.",
        },
        "mode": { "commpage_checks": unit.commpage_checks,
            "verification": unit.no_internal_jumps, "aot": false },
        "limits": "Compile-time map, not execution counts. lookup_hit_interval includes shared lookup; only lookup_hit_only_interval is exclusive to a hit. Rewritten instruction offsets remain null. Pointer slots are excluded from code bytes. Use unique map paths per process and --no-aot for attribution.",
    });
    if let Ok(mut encoded) = serde_json::to_vec(&record) {
        encoded.push(b'\n');
        match writer.lock() {
            Ok(mut file) => {
                if let Err(error) = file.write_all(&encoded) {
                    eprintln!("maclator: diagnostic JIT map write failed: {error}");
                }
            }
            Err(_) => eprintln!("maclator: diagnostic JIT map writer poisoned"),
        }
    }
}
