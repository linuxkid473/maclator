//! Standalone test program: no Cargo/runtime registration, no guest callbacks.
#![cfg_attr(not(all(target_os = "macos", target_arch = "x86_64")), allow(unused))]
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
compile_error!("This probe requires real Darwin x86-64");
#[path = "../native_fault.rs"]
mod native;
use std::mem::{align_of, offset_of, size_of};
use std::sync::atomic::{AtomicU32, Ordering};

#[repr(C)]
#[derive(Default, Debug)]
struct State { armed: u32, active: u32, reserved: u32, local_registered: u32, nonzero_stack_cells: u32 }
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Action { handler: u64, flags: i32, mask: u32 }
#[repr(C)]
#[derive(Default)]
struct Fixture {
    destination: u64, source: u64, length: u64, completed: u64, fault_address: u64, page_size: u64,
    operation: u32, value: u32, backward: u32, writing: u32,
}
const _: () = {
    assert!(size_of::<State>() == 20 && align_of::<State>() == 4);
    assert!(size_of::<Action>() == 16 && align_of::<Action>() == 8);
    assert!(size_of::<Fixture>() == 64 && align_of::<Fixture>() == 8);
};
extern "C" {
    fn mcl_ra_layout(out: *mut u32, capacity: usize) -> usize;
    fn mcl_ra_state_snapshot(out: *mut State);
    fn mcl_ra_set_armed(value: u32);
    fn mcl_ra_prepare_defaults() -> i32;
    fn mcl_ra_prepare_custom() -> i32;
    fn mcl_ra_actions(out: *mut Action) -> i32;
    fn mcl_ra_sigaction_counts_reset();
    fn mcl_ra_sigaction_counts(out: *mut u32);
    fn mcl_ra_get_errno() -> i32;
    fn mcl_ra_set_errno(value: i32);
    fn mcl_ra_edom() -> i32;
    fn mcl_ra_enotsup() -> i32;
    fn mcl_ra_mask() -> u32;
    fn mcl_ra_set_mask(value: u32) -> i32;
    fn mcl_ra_usr1_mask() -> u32;
    fn mcl_ra_segv_mask() -> u32;
    fn mcl_ra_observation_mode() -> i32;
    fn mcl_ra_observation_reset();
    fn mcl_ra_observed_flags() -> u64;
    fn mcl_ra_observed_status() -> i32;
    fn mcl_ra_observed_calls() -> u32;
    fn mcl_ra_read_flags() -> u64;
    fn mcl_ra_fixture_setup(kind: u32, out: *mut Fixture) -> i32;
    fn mcl_ra_fixture_unprotect() -> i32;
    fn mcl_ra_fixture_matches(full: u32) -> i32;
    fn mcl_ra_fixture_close();
}
struct Scope<'a>(&'a AtomicU32);
impl Drop for Scope<'_> { fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); } }
struct FixtureOwner;
impl Drop for FixtureOwner { fn drop(&mut self) { unsafe { mcl_ra_fixture_close() }; } }
struct Seen { outcome: native::Outcome, rust_flags: u64, c_flags: u64, c_status: i32, c_calls: u32, errno: i32, drops: u32 }

fn snapshot() -> State {
    let mut state = State::default();
    unsafe { mcl_ra_state_snapshot(&mut state) }; state
}
fn released(expected_armed: u32, require_published: bool) {
    let state = snapshot();
    assert_eq!(state.armed, expected_armed, "descriptor state: {state:?}");
    assert_eq!((state.active, state.reserved, state.local_registered), (0, 0, 0), "registry released: {state:?}");
    if require_published { assert!(state.nonzero_stack_cells > 0, "a real leaf published its RSP"); }
    // Inactive registry stack cells intentionally retain stale RSP. They are
    // not owners after the atomic descriptor pointer is NULL.
}
fn actions() -> [Action; 2] {
    let mut out = [Action::default(); 2];
    assert_eq!(unsafe { mcl_ra_actions(out.as_mut_ptr()) }, 0); out
}
fn counts() -> [u32; 2] {
    let mut out = [0; 2]; unsafe { mcl_ra_sigaction_counts(out.as_mut_ptr()) }; out
}
fn install_default() {
    assert_eq!(unsafe { mcl_ra_prepare_defaults() }, 0);
    unsafe { mcl_ra_sigaction_counts_reset() };
    let report = unsafe { native::install() }.expect("fresh isolated DFL installation");
    assert_eq!((report.primary_error, report.rollback_error, report.changed_mask, report.restored_mask), (0, 0, 3, 0));
    assert_eq!(counts(), [2, 2]);
    unsafe { native::enter_thread() };
    released(0, false);
}
fn invoke(operation: u32, destination: u64, source: u64, length: u64, value: u32) -> Seen {
    let drops = AtomicU32::new(0);
    let expected_errno = unsafe { mcl_ra_edom() };
    unsafe { mcl_ra_observation_reset(); mcl_ra_set_errno(expected_errno) };
    let outcome = {
        let _scope = Scope(&drops);
        unsafe { native::call(operation, destination, source, length, value) }
    };
    // Scope destruction uses only an atomic increment. No formatting/heap
    // cleanup/libc call intervenes before the Rust-return errno observation.
    let rust_flags = unsafe { mcl_ra_read_flags() };
    let error = unsafe { mcl_ra_get_errno() };
    let seen = Seen { outcome, rust_flags, c_flags: unsafe { mcl_ra_observed_flags() },
        c_status: unsafe { mcl_ra_observed_status() }, c_calls: unsafe { mcl_ra_observed_calls() },
        errno: error, drops: drops.load(Ordering::SeqCst) };
    assert_eq!(seen.errno, expected_errno, "errno after C+Rust normal returns");
    assert_eq!(seen.drops, 1, "exactly one normal Rust scope return");
    assert_eq!(seen.rust_flags & (1 << 10), 0, "Rust caller DF clear");
    if unsafe { mcl_ra_observation_mode() } == 1 {
        assert_eq!(seen.c_calls, 1, "adapter issued exactly one C primitive call");
        assert_eq!(seen.c_flags & (1 << 10), 0, "FIRST native C-return DF clear");
    } else {
        assert_eq!((seen.c_calls, seen.c_flags, seen.c_status), (0, u64::MAX, i32::MIN), "direct mode does not claim immediate C flags");
    }
    let mode = unsafe { mcl_ra_observation_mode() };
    let c_df = if mode == 1 { ((seen.c_flags >> 10) & 1).to_string() } else { "null".to_string() };
    let c_status = if mode == 1 { seen.c_status.to_string() } else { "null".to_string() };
    println!("{{\"ffi_return_observation\":true,\"errno\":{},\"rust_return_df\":{},\"immediate_c_return_df\":{},\"observed_c_status\":{},\"rust_scope_drops\":{}}}",
        seen.errno, (seen.rust_flags >> 10) & 1, c_df, c_status, seen.drops);
    seen
}
fn observed_status(seen: &Seen, expected: i32) {
    if unsafe { mcl_ra_observation_mode() } == 1 { assert_eq!(seen.c_status, expected); }
}
fn test_layout() {
    use native::{InstallReport as I, ResultRecord as R};
    let rust = [size_of::<I>() as u32, align_of::<I>() as u32, size_of::<R>() as u32, align_of::<R>() as u32,
        offset_of!(I, primary_error) as u32, offset_of!(I, rollback_error) as u32,
        offset_of!(I, changed_mask) as u32, offset_of!(I, restored_mask) as u32,
        offset_of!(R, destination_next) as u32, offset_of!(R, source_next) as u32,
        offset_of!(R, remaining) as u32, offset_of!(R, completed) as u32,
        offset_of!(R, host_address) as u32, offset_of!(R, host_pc) as u32,
        offset_of!(R, host_rflags) as u32, offset_of!(R, signal_number) as u32,
        offset_of!(R, signal_code) as u32, offset_of!(R, writing) as u32,
        offset_of!(R, backward) as u32, offset_of!(R, exception_error) as u32,
        offset_of!(R, trap_number) as u32, offset_of!(R, original_mask) as u32,
        offset_of!(R, reserved) as u32];
    let mut c = [u32::MAX; 23];
    assert_eq!(unsafe { mcl_ra_layout(c.as_mut_ptr(), c.len()) }, c.len());
    assert_eq!(rust, c, "every C/Rust size, alignment and field offset");
    assert_eq!((rust[0], rust[2]), (16, 88));
    println!("{{\"install_report_size\":16,\"result_size\":88,\"layout_words_checked\":23}}");
}
fn test_prior(custom_fixture: bool) {
    if custom_fixture { assert_eq!(unsafe { mcl_ra_prepare_custom() }, 0); }
    let before = actions();
    let unknown = before.iter().any(|a| a.handler != 0);
    unsafe { mcl_ra_sigaction_counts_reset() };
    let result = unsafe { native::install() };
    if unknown {
        let report = result.expect_err("unknown preexisting action must be rejected");
        assert_eq!((report.primary_error, report.rollback_error, report.changed_mask, report.restored_mask),
            (unsafe { mcl_ra_enotsup() }, 0, 0, 0));
        assert_eq!(counts(), [2, 0], "two reads, zero writes for unknown prior");
        assert_eq!(actions(), before, "observable actions untouched");
    } else {
        assert!(!custom_fixture);
        let report = result.expect("DFL-only startup may install");
        assert_eq!((report.primary_error, report.rollback_error, report.changed_mask, report.restored_mask), (0, 0, 3, 0));
        assert_eq!(counts(), [2, 2]);
    }
    released(0, false);
    println!("{{\"startup_custom_found\":{},\"before_segv_handler\":{},\"before_bus_handler\":{},\"before_segv_flags\":{},\"before_bus_flags\":{}}}",
        unknown, before[0].handler, before[1].handler, before[0].flags, before[1].flags);
}
fn test_report() {
    install_default();
    unsafe { mcl_ra_sigaction_counts_reset() };
    let report = unsafe { native::install() }.expect("idempotent adapter installation");
    assert_eq!((report.primary_error, report.rollback_error, report.changed_mask, report.restored_mask), (0,0,0,0));
    assert_eq!(counts(), [0,0]); released(0, false);
}
fn test_complete(copy: bool) {
    install_default();
    let baseline = unsafe { mcl_ra_usr1_mask() };
    assert_eq!(unsafe { mcl_ra_set_mask(baseline) }, 0);
    let mut bytes = [0u8; 384];
    for (i, byte) in bytes.iter_mut().enumerate() { *byte = (i.wrapping_mul(73) & 255) as u8; }
    let mut reference = bytes;
    let pointer = bytes.as_mut_ptr() as u64;
    let seen = if copy {
        reference.copy_within(0..256, 17);
        invoke(1, pointer+17, pointer, 256, 0)
    } else {
        reference.fill(0x6d); invoke(2, pointer, 0, 384, 0x6d)
    };
    observed_status(&seen, 0);
    let native::Outcome::Complete(record) = seen.outcome else { panic!("expected Complete"); };
    assert_eq!(record.completed, if copy { 256 } else { 384 });
    assert_eq!(record.backward, if copy { 1 } else { 0 });
    assert_eq!(record.original_mask, baseline);
    assert_eq!(bytes, reference); assert_eq!(unsafe { mcl_ra_mask() }, baseline); released(0, true);
}
fn test_decline(name: &str) {
    install_default();
    let baseline = unsafe { mcl_ra_usr1_mask() };
    let blocked = name == "decline-blocked";
    let armed = if name == "reentrant-preparing" { 4 } else if name == "reentrant-armed" { 1 } else { 0 };
    let mask = baseline | if blocked { unsafe { mcl_ra_segv_mask() } } else { 0 };
    assert_eq!(unsafe { mcl_ra_set_mask(mask) }, 0);
    if armed != 0 { unsafe { mcl_ra_set_armed(armed) }; }
    let mut bytes = [0x41u8; 64];
    let seen = if name == "decline-invalid" { invoke(1, u64::MAX, 1, 1, 0) }
        else { invoke(2, bytes.as_mut_ptr() as u64, 0, 64, 7) };
    observed_status(&seen, if armed == 0 { -1 } else { -2 });
    if armed == 0 { assert!(matches!(seen.outcome, native::Outcome::Declined)); }
    else { assert!(matches!(seen.outcome, native::Outcome::Reentrant)); }
    assert_eq!(bytes, [0x41; 64]); assert_eq!(unsafe { mcl_ra_mask() }, mask); released(armed, false);
    if armed != 0 { unsafe { mcl_ra_set_armed(0) }; }
    assert_eq!(unsafe { mcl_ra_set_mask(baseline) }, 0); released(0, false);
}
fn test_fault(kind: u32) {
    install_default();
    let baseline = unsafe { mcl_ra_usr1_mask() };
    assert_eq!(unsafe { mcl_ra_set_mask(baseline) }, 0);
    let mut fixture = Fixture::default();
    assert_eq!(unsafe { mcl_ra_fixture_setup(kind, &mut fixture) }, 0);
    let _fixture_owner = FixtureOwner;
    assert!(fixture.page_size >= 1024);
    let seen = invoke(fixture.operation, fixture.destination, fixture.source, fixture.length, fixture.value);
    observed_status(&seen, 1);
    let native::Outcome::Captured(record) = seen.outcome else { panic!("expected Captured"); };
    let remaining = fixture.length-fixture.completed;
    assert_eq!((record.completed,record.remaining), (fixture.completed,remaining));
    let offset = if fixture.backward != 0 { remaining-1 } else { fixture.completed };
    assert_eq!(record.destination_next,fixture.destination+offset);
    assert_eq!(record.source_next,if fixture.operation==2 { 0 } else { fixture.source+offset });
    assert_eq!(record.host_address,fixture.fault_address); assert_ne!(record.host_pc,0);
    assert_eq!((record.backward,record.writing), (fixture.backward as i32,fixture.writing as i32));
    assert_eq!((record.host_rflags>>10)&1,fixture.backward as u64);
    assert_eq!((record.exception_error>>1)&1,fixture.writing);
    assert_eq!(record.trap_number,14); assert_eq!(record.original_mask,baseline); assert_eq!(record.reserved,0);
    assert!(matches!(record.signal_number,10|11)); assert!(record.signal_code>0);
    assert_eq!(unsafe { mcl_ra_mask() },baseline); released(0,true);
    assert_eq!(unsafe { mcl_ra_fixture_unprotect() },0);
    assert_eq!(unsafe { mcl_ra_fixture_matches(0) },1,"exact whole-arena partial writes/canaries");
    // Resume ONLY remaining host interval. This is a manual test action, never
    // guest signal delivery, arbitrary continuation permission or original retry.
    let destination = if fixture.backward != 0 { record.destination_next-(remaining-1) } else { record.destination_next };
    let source = if fixture.operation==2 { 0 } else if fixture.backward != 0 { record.source_next-(remaining-1) } else { record.source_next };
    let second = invoke(fixture.operation,destination,source,remaining,fixture.value);
    observed_status(&second,0);
    let native::Outcome::Complete(residual) = second.outcome else { panic!("expected Complete residual"); };
    assert_eq!(residual.completed,remaining); assert_eq!(unsafe { mcl_ra_fixture_matches(1) },1,"one-operation reference after residual-only completion");
    assert_eq!(unsafe { mcl_ra_mask() },baseline); released(0,true);
    assert_eq!((record.completed,record.remaining,record.host_address), (fixture.completed,remaining,fixture.fault_address), "owned Rust record survives descriptor reuse");
    println!("{{\"captured_returned_to_rust\":true,\"completed\":{},\"remaining\":{},\"host_rflags\":{},\"rust_scope_drop_count_per_call\":1}}",record.completed,remaining,record.host_rflags);
}
fn main() {
    let name = std::env::args().nth(1).expect("one case name");
    match name.as_str() {
        "layout" => test_layout(), "startup-prior" => test_prior(false), "install-custom-rejected" => test_prior(true),
        "install-report" => test_report(), "complete-copy" => test_complete(true), "complete-fill" => test_complete(false),
        "decline-invalid"|"decline-blocked"|"reentrant-preparing"|"reentrant-armed" => test_decline(&name),
        "fault-copy-forward" => test_fault(0), "fault-copy-backward" => test_fault(1), "fault-fill" => test_fault(2),
        _ => panic!("unknown case"),
    }
    println!("{{\"case\":\"{}\",\"mode\":\"{}\",\"pass\":true}}",name,
        if unsafe { mcl_ra_observation_mode() } == 1 { "observed" } else { "direct" });
}
