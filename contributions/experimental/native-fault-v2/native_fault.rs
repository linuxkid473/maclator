//! Staged Darwin x86-64 native data-fault API; deliberately not declared by
//! main.rs, linked by Cargo, installed at startup, or used by platform.rs yet.
//! A Captured result must be delivered as a synchronous guest signal. Treating
//! it as success/decline or restarting the original overlapping copy is wrong.
//! InternalError may also follow partial writes; never use it to retry/fallback.

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct ResultRecord {
    pub destination_next: u64,
    pub source_next: u64,
    pub remaining: u64,
    pub completed: u64,
    pub host_address: u64,
    pub host_pc: u64,
    pub host_rflags: u64,
    pub signal_number: i32,
    pub signal_code: i32,
    pub writing: i32,
    pub backward: i32,
    pub exception_error: u32,
    pub trap_number: u32,
    pub original_mask: u32,
    pub reserved: u32,
}
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<ResultRecord>() == 88);
    assert!(offset_of!(ResultRecord, destination_next) == 0);
    assert!(offset_of!(ResultRecord, source_next) == 8);
    assert!(offset_of!(ResultRecord, remaining) == 16);
    assert!(offset_of!(ResultRecord, completed) == 24);
    assert!(offset_of!(ResultRecord, host_address) == 32);
    assert!(offset_of!(ResultRecord, host_pc) == 40);
    assert!(offset_of!(ResultRecord, host_rflags) == 48);
    assert!(offset_of!(ResultRecord, signal_number) == 56);
    assert!(offset_of!(ResultRecord, signal_code) == 60);
    assert!(offset_of!(ResultRecord, writing) == 64);
    assert!(offset_of!(ResultRecord, backward) == 68);
    assert!(offset_of!(ResultRecord, exception_error) == 72);
    assert!(offset_of!(ResultRecord, trap_number) == 76);
    assert!(offset_of!(ResultRecord, original_mask) == 80);
    assert!(offset_of!(ResultRecord, reserved) == 84);
};

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct InstallReport {
    pub primary_error: i32,
    pub rollback_error: i32,
    pub changed_mask: u32,
    pub restored_mask: u32,
}
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<InstallReport>() == 16);
    assert!(offset_of!(InstallReport, primary_error) == 0);
    assert!(offset_of!(InstallReport, rollback_error) == 4);
    assert!(offset_of!(InstallReport, changed_mask) == 8);
    assert!(offset_of!(InstallReport, restored_mask) == 12);
};

extern "C" {
    fn mcl_nf_install_with_report(report: *mut InstallReport) -> i32;
    fn mcl_nf_enter_thread();
    fn mcl_nf_call(operation: u32, destination: usize, source: usize,
        length: usize, value: u32, result: *mut ResultRecord) -> i32;
}

#[derive(Debug)]
pub enum Outcome {
    Complete(ResultRecord),
    Captured(ResultRecord),
    Declined,
    Reentrant,
    InternalError,
}

/// Startup-only, before guest threads and without concurrent native sigaction
/// mutation. Both native dispositions must be DFL; all unknown custom/ignored
/// actions are rejected before writes. Unmatched signals terminate with canonical
/// DFL/flags0/empty-mask behavior. Successful install canonicalizes prior DFL
/// flags/masks. Failure rollback restores query-visible flags/masks and DFL
/// behavior, not hidden raw Darwin SA_RESETHAND state. Every failure must stop
/// startup; a rollback_error requires caller-owned default restoration before
/// any retry. The runtime needs authoritative canonical host initialization.
pub unsafe fn install() -> Result<InstallReport, InstallReport> {
    let mut report = InstallReport::default();
    match mcl_nf_install_with_report(&mut report) {
        0 => Ok(report),
        _ => Err(report),
    }
}

pub unsafe fn enter_thread() { mcl_nf_enter_thread(); }

/// The caller has already normalized complete guest intervals/commpage ranges.
/// No Rust reference into either guest buffer survives this C/assembly call.
pub unsafe fn call(operation: u32, destination: u64, source: u64,
    length: u64, value: u32) -> Outcome {
    let (Ok(destination), Ok(source), Ok(length)) =
        (usize::try_from(destination), usize::try_from(source), usize::try_from(length))
        else { return Outcome::Declined; };
    let mut record = ResultRecord::default();
    match mcl_nf_call(operation, destination, source, length, value, &mut record) {
        0 => Outcome::Complete(record),
        1 => Outcome::Captured(record),
        -1 => Outcome::Declined,
        -2 => Outcome::Reentrant,
        _ => Outcome::InternalError,
    }
}
