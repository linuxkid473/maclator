//! Private constructor boundary only: no native primitive, guest or fault delivery.
#[cfg(not(all(target_os="macos",target_arch="x86_64")))]
compile_error!("This probe requires real Darwin x86-64");
use std::mem::{align_of,offset_of,size_of};
#[repr(C)]
#[derive(Clone,Copy,Default,Debug,PartialEq,Eq)]
struct Action { handler:u64, flags:u32, mask:u32 }
#[repr(C)]
#[derive(Clone,Copy,Default,Debug,PartialEq,Eq)]
struct Altstack { pointer:u64, size:u64, flags:i32, query_error:i32 }
#[repr(C)]
#[derive(Default,Debug,PartialEq,Eq)]
struct InstallReport { primary_error:i32, rollback_error:i32, changed_mask:u32, restored_mask:u32 }
#[repr(C)]
#[derive(Default)]
struct Record {
    scenario:u32, constructor_entered:u32, constructor_finished:u32, main_seen:u32,
    original_query_error:i32, fixture_error:i32, installer_return:i32,
    install_attempted:u32, decision:u32, native_queries:u32, native_writes:u32,
    after_query_error:i32, post_fixture_error:i32, post_fixture_applied:u32,
    hidden_reset_requested:u32, native_installed:u32, report:InstallReport,
    original:[Action;2], before_install:[Action;2], after_install:[Action;2], at_ctor_exit:[Action;2],
    original_altstack:Altstack, sequence_count:u32, sequence:[u32;8],
}
const _:()={
    assert!(size_of::<Action>()==16 && align_of::<Action>()==8);
    assert!(size_of::<Altstack>()==24 && align_of::<Altstack>()==8);
    assert!(size_of::<InstallReport>()==16 && align_of::<InstallReport>()==4);
    assert!(size_of::<Record>()==272 && align_of::<Record>()==8);
    assert!(offset_of!(Record,report)==64 && offset_of!(Record,original)==80);
    assert!(offset_of!(Record,before_install)==112 && offset_of!(Record,after_install)==144);
    assert!(offset_of!(Record,at_ctor_exit)==176 && offset_of!(Record,original_altstack)==208);
    assert!(offset_of!(Record,sequence_count)==232 && offset_of!(Record,sequence)==236);
};
extern "C" {
    fn mcl_eo_record_at_main(out:*mut Record);
    fn mcl_eo_current(out:*mut Action,stack:*mut Altstack)->i32;
    fn mcl_eo_visible_recipe_matches(pair:*const Action)->i32;
    fn mcl_eo_idempotent_report(out:*mut InstallReport,counts:*mut u32);
    fn mcl_eo_reset_flag()->u32;
    fn mcl_eo_onstack_flag()->u32;
    fn mcl_eo_siginfo_flag()->u32;
    fn mcl_eo_altstack_disabled_flag()->i32;
    fn mcl_eo_enotsup()->i32;
}
const CASES:[&str;9]=["baseline-observe","natural-early","prior-custom-segv","prior-custom-bus",
    "prior-ignore-segv","prior-ignore-bus","late-custom","late-owned-default","late-same-owner-reset"];
fn current()->([Action;2],Altstack) {
    let mut pair=[Action::default();2];let mut stack=Altstack::default();
    assert_eq!(unsafe{mcl_eo_current(pair.as_mut_ptr(),&mut stack)},0);
    assert_eq!(stack.query_error,0);(pair,stack)
}
fn all_default(pair:&[Action;2])->bool { pair.iter().all(|a|a.handler==0) }
fn print_actions(stage:&str,pair:&[Action;2]) {
    println!("{{\"action_stage\":\"{}\",\"segv_handler\":{},\"segv_flags\":{},\"segv_mask\":{},\"bus_handler\":{},\"bus_flags\":{},\"bus_mask\":{}}}",
        stage,pair[0].handler,pair[0].flags,pair[0].mask,pair[1].handler,pair[1].flags,pair[1].mask);
}
fn main() {
    // First user-main operation observes that the C constructor already finished.
    let mut record=Record::default();unsafe{mcl_eo_record_at_main(&mut record)};
    let scenario=record.scenario as usize;assert!(scenario<CASES.len());let name=CASES[scenario];
    assert_eq!((record.constructor_entered,record.constructor_finished,record.main_seen),(1,1,1));
    assert_eq!((record.original_query_error,record.fixture_error,record.after_query_error,record.post_fixture_error),(0,0,0,0));
    assert_eq!(record.original_altstack.query_error,0);
    let original_default=all_default(&record.original);
    let before_default=all_default(&record.before_install);
    let private_prior=original_default && (2..=5).contains(&scenario);
    let private_post=record.decision==1 && scenario>=6;
    let mut expected_sequence:Vec<u32>=vec![1];if private_prior{expected_sequence.push(2)};
    expected_sequence.extend([3,4]);if private_post{expected_sequence.push(5)};
    expected_sequence.extend([6,7]);
    assert_eq!(record.sequence_count as usize,expected_sequence.len());
    assert_eq!(&record.sequence[..expected_sequence.len()],expected_sequence.as_slice());
    if !private_prior {assert_eq!(record.before_install,record.original)}
    else {
        let index=if scenario==2 || scenario==4 {0}else{1};
        assert_eq!(record.before_install[1-index],record.original[1-index]);
        assert_ne!(record.before_install[index].handler,0);
        assert_eq!(record.before_install[index].flags,0);
        assert_ne!(record.before_install[index].mask,0);
        if scenario>=4 {assert_eq!(record.before_install[index].handler,1)}
        else {assert_ne!(record.before_install[index].handler,1)}
    }
    let (pair,main_stack)=current();let visible=unsafe{mcl_eo_visible_recipe_matches(pair.as_ptr())}==1;
    match record.decision {
        0=>{
            assert_eq!(scenario,0);assert_eq!(record.install_attempted,0);
            assert_eq!((record.native_queries,record.native_writes,record.native_installed),(0,0,0));
            assert_eq!(record.installer_return,0);assert_eq!(record.report,InstallReport::default());
            assert_eq!(record.after_install,record.before_install);assert_eq!(record.at_ctor_exit,record.before_install);
        }
        1=>{
            assert!(before_default);assert_eq!(record.install_attempted,1);
            assert_eq!((record.native_queries,record.native_writes,record.native_installed),(2,2,1));
            assert_eq!(record.installer_return,0);
            assert_eq!(record.report,InstallReport{changed_mask:3,..InstallReport::default()});
            assert_eq!(unsafe{mcl_eo_visible_recipe_matches(record.after_install.as_ptr())},1);
            assert_eq!(record.post_fixture_applied,if private_post{1}else{0});
            // installed=1 makes a later install idempotent even if a test writer
            // replaced the real action. This is NOT an ownership attestation.
            let mut report=InstallReport::default();let mut counts=[u32::MAX;2];
            unsafe{mcl_eo_idempotent_report(&mut report,counts.as_mut_ptr())};
            assert_eq!(report,InstallReport::default());assert_eq!(counts,[0,0]);
            if !private_post {assert!(visible);assert_eq!(pair,record.at_ctor_exit)}
            else if scenario!=8 {assert!(!visible)}
            else {
                assert_eq!(record.hidden_reset_requested,1);
                assert_eq!(pair,record.at_ctor_exit);
                let reset_visible=pair[1].flags&unsafe{mcl_eo_reset_flag()}!=0;
                assert_eq!(visible,!reset_visible);
                println!("{{\"known_same_owner_reset_write\":true,\"reset_visible_in_query\":{},\"visible_recipe_matches\":{},\"attestation_authorized\":false}}",reset_visible,visible);
            }
        }
        2=>{
            assert!(!before_default);assert_eq!(record.install_attempted,1);
            assert_eq!((record.native_queries,record.native_writes,record.native_installed),(2,0,0));
            assert_eq!(record.installer_return,unsafe{mcl_eo_enotsup()});
            assert_eq!(record.report,InstallReport{primary_error:unsafe{mcl_eo_enotsup()},..InstallReport::default()});
            assert_eq!(record.after_install,record.before_install);
            assert_eq!(record.at_ctor_exit,record.before_install);assert_eq!(record.post_fixture_applied,0);
            assert!(!visible);
        }
        _=>panic!("unsupported recorded decision"),
    }
    if scenario!=8 || !private_post {assert_eq!(record.hidden_reset_requested,0)}
    // std only installs its callback for dispositions left DFL at constructor exit.
    let std_expected=record.at_ctor_exit.iter().any(|a|a.handler==0);
    let info_onstack=unsafe{mcl_eo_siginfo_flag()|mcl_eo_onstack_flag()};
    for index in 0..2 {
        if record.at_ctor_exit[index].handler==0 {
            assert_ne!(pair[index].handler,0);
            assert_eq!(pair[index].flags&info_onstack,info_onstack);
        } else {assert_eq!(pair[index],record.at_ctor_exit[index])}
    }
    let disabled=unsafe{mcl_eo_altstack_disabled_flag()};
    if std_expected {assert_eq!(main_stack.flags&disabled,0)}
    else {assert_eq!(main_stack,record.original_altstack)}
    // Ordinary Rust host thread, no native TLV/primitive or stack-overflow trigger.
    let (thread_pair,thread_stack)=std::thread::spawn(current).join().expect("private host thread");
    assert_eq!(thread_pair,pair);
    assert_eq!(thread_stack.flags&disabled,if std_expected{0}else{disabled});
    print_actions("constructor-original",&record.original);
    print_actions("before-native-install",&record.before_install);
    print_actions("after-native-install",&record.after_install);
    print_actions("constructor-exit",&record.at_ctor_exit);
    print_actions("rust-user-main",&pair);
    println!("{{\"constructor_finished_before_user_main\":true,\"original_both_default\":{},\"decision\":{},\"native_queries\":{},\"native_writes\":{},\"private_prior_fixture\":{},\"private_post_fixture\":{},\"visible_recipe_matches\":{},\"idempotent_is_attestation\":false}}",
        original_default,record.decision,record.native_queries,record.native_writes,private_prior,private_post,visible);
    println!("{{\"std_callback_expected_from_default_exit\":{},\"original_altstack_flags\":{},\"main_altstack_flags\":{},\"new_host_thread_altstack_flags\":{},\"main_altstack_pointer\":{},\"main_altstack_size\":{},\"thread_altstack_pointer\":{},\"thread_altstack_size\":{},\"stackoverflow_triggered\":false}}",
        std_expected,record.original_altstack.flags,main_stack.flags,thread_stack.flags,main_stack.pointer,main_stack.size,thread_stack.pointer,thread_stack.size);
    println!("{{\"case\":\"{}\",\"mode\":\"early-owner\",\"pass\":true,\"production_ready\":false,\"guest_resume\":false,\"performance_measured\":false}}",name);
}
