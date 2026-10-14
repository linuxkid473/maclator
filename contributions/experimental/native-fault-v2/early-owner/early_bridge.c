// Private source-only constructor experiment. No native source is edited.
// One coordinator controls test fixtures; no priority/cross-image order claim.
#include "early_bridge.h"
#include <errno.h>
#include <signal.h>
#include <stdatomic.h>
#include <unistd.h>
#ifndef MCL_EO_SCENARIO
#error Define exactly one private scenario when compiling this test object.
#endif
_Static_assert(MCL_EO_SCENARIO >= 0 && MCL_EO_SCENARIO <= 8, "scenario range");
static int mcl_eo_count_sigaction(int, const struct sigaction *, struct sigaction *);
#define sigaction(sig, action, old) mcl_eo_count_sigaction((sig), (action), (old))
#include "../native-fault/native_fault.c"
#undef sigaction
_Static_assert(sizeof(struct mcl_eo_action)==16, "action ABI");
_Static_assert(sizeof(struct mcl_eo_altstack)==24, "altstack ABI");
_Static_assert(sizeof(struct mcl_eo_record)==272, "record ABI");
_Static_assert(_Alignof(struct mcl_eo_record)==8, "record alignment");
_Static_assert(offsetof(struct mcl_eo_record,report)==64, "report offset");
_Static_assert(offsetof(struct mcl_eo_record,original)==80, "original offset");
_Static_assert(offsetof(struct mcl_eo_record,before_install)==112, "before offset");
_Static_assert(offsetof(struct mcl_eo_record,after_install)==144, "after offset");
_Static_assert(offsetof(struct mcl_eo_record,at_ctor_exit)==176, "exit offset");
_Static_assert(offsetof(struct mcl_eo_record,original_altstack)==208, "altstack offset");
_Static_assert(offsetof(struct mcl_eo_record,sequence_count)==232, "sequence count offset");
_Static_assert(offsetof(struct mcl_eo_record,sequence)==236, "sequence offset");
static atomic_uint native_queries, native_writes;
static struct mcl_eo_record record;
static int mcl_eo_count_sigaction(int sig, const struct sigaction *action, struct sigaction *old) {
    atomic_fetch_add_explicit(action ? &native_writes : &native_queries, 1, memory_order_relaxed);
    return sigaction(sig, action, old); // Real libc, unchanged requested operation.
}
static void sequence(uint32_t value) {
    if (record.sequence_count < 8) record.sequence[record.sequence_count++] = value;
}
static int actions(struct mcl_eo_action *out) {
    const int numbers[2]={SIGSEGV,SIGBUS};
    for (unsigned i=0;i<2;++i) {
        struct sigaction action;
        if (sigaction(numbers[i],NULL,&action)) return errno;
        out[i]=(struct mcl_eo_action){(uint64_t)(uintptr_t)action.sa_handler,
                                    (uint32_t)action.sa_flags,action.sa_mask};
    }
    return 0;
}
static struct mcl_eo_altstack altstack(void) {
    stack_t stack={0}; struct mcl_eo_altstack out={0};
    if (sigaltstack(NULL,&stack)) { out.query_error=errno; return out; }
    out.pointer=(uintptr_t)stack.ss_sp; out.size=stack.ss_size; out.flags=stack.ss_flags;
    return out;
}
static int all_default(const struct mcl_eo_action *pair) { return pair[0].handler==0 && pair[1].handler==0; }
static void private_custom(int sig) { (void)sig; }
static int write_private_prior(int sig, int ignored) {
    struct sigaction action={0};
    action.sa_handler=ignored ? SIG_IGN : private_custom;
    sigemptyset(&action.sa_mask); sigaddset(&action.sa_mask,SIGUSR2);
    return sigaction(sig,&action,NULL)==0 ? 0 : errno;
}
static int write_post_fixture(unsigned scenario) {
    struct sigaction action={0}; sigemptyset(&action.sa_mask);
    if (scenario==MCL_EO_LATE_CUSTOM) action.sa_handler=private_custom;
    else if (scenario==MCL_EO_LATE_DEFAULT) action.sa_handler=SIG_DFL;
    else {
        // Known test-owned same-pointer mutation, never an external prior reset.
        action.sa_sigaction=mcl_nf_signal_entry;
        action.sa_flags=SA_SIGINFO|SA_RESETHAND;
        sigaddset(&action.sa_mask,SIGSEGV); sigaddset(&action.sa_mask,SIGBUS);
        record.hidden_reset_requested=1;
    }
    return sigaction(SIGBUS,&action,NULL)==0 ? 0 : errno;
}
__attribute__((constructor)) static void mcl_eo_constructor(void) {
    record.scenario=MCL_EO_SCENARIO; record.constructor_entered=1; sequence(1);
    record.original_query_error=actions(record.original);
    record.original_altstack=altstack();
    if (record.original_query_error || record.original_altstack.query_error) _exit(121);
    // No DFL reset. Fixtures are permitted ONLY when BOTH original actions are DFL.
    if (all_default(record.original) && record.scenario>=MCL_EO_CUSTOM_SEGV && record.scenario<=MCL_EO_IGNORE_BUS) {
        const int sig=(record.scenario==MCL_EO_CUSTOM_SEGV || record.scenario==MCL_EO_IGNORE_SEGV) ? SIGSEGV : SIGBUS;
        record.fixture_error=write_private_prior(sig,record.scenario>=MCL_EO_IGNORE_SEGV);
        record.post_fixture_applied=0; sequence(2);
        if (record.fixture_error) _exit(122);
    }
    record.after_query_error=actions(record.before_install); if (record.after_query_error) _exit(123);
    sequence(3);
    if (record.scenario!=MCL_EO_BASELINE) {
        record.install_attempted=1;
        atomic_store(&native_queries,0); atomic_store(&native_writes,0);
        record.installer_return=mcl_nf_install_with_report(&record.report);
        record.native_queries=atomic_load(&native_queries); record.native_writes=atomic_load(&native_writes);
        if (!record.installer_return) record.decision=MCL_EO_INSTALLED;
        else if (record.installer_return==ENOTSUP && !record.report.changed_mask
                 && !record.report.rollback_error && !record.report.restored_mask && !record.native_writes)
            record.decision=MCL_EO_UNSUPPORTED;
        else _exit(120); // Any actual error stops before Rust runtime/application startup.
    }
    record.after_query_error=actions(record.after_install); if (record.after_query_error) _exit(124);
    sequence(4);
    if (record.decision==MCL_EO_INSTALLED && record.scenario>=MCL_EO_LATE_CUSTOM) {
        record.post_fixture_error=write_post_fixture(record.scenario);
        record.post_fixture_applied=1; sequence(5);
        if (record.post_fixture_error) _exit(125);
    }
    record.after_query_error=actions(record.at_ctor_exit); if (record.after_query_error) _exit(126);
    record.native_installed=atomic_load_explicit(&installed,memory_order_acquire);
    record.constructor_finished=1; sequence(6);
    // Never calls mcl_nf_enter_thread/call, allocator, Rust, dlopen or creates a thread.
    // errno/libc/lazy binding dependencies are not claimed universally TLS/allocation-free.
}
void mcl_eo_record_at_main(struct mcl_eo_record *out) {
    record.main_seen=1; sequence(7); *out=record;
}
int mcl_eo_current(struct mcl_eo_action *out, struct mcl_eo_altstack *stack) {
    const int error=actions(out); *stack=altstack(); return error ? error : stack->query_error;
}
int mcl_eo_visible_recipe_matches(const struct mcl_eo_action *pair) {
    sigset_t expected=0; sigaddset(&expected,SIGSEGV); sigaddset(&expected,SIGBUS);
    for (unsigned i=0;i<2;++i)
        if (pair[i].handler!=(uint64_t)(uintptr_t)mcl_nf_signal_entry || pair[i].flags!=SA_SIGINFO || pair[i].mask!=expected)
            return 0;
    return 1; // Visibility ONLY, not proof of hidden RESET or exclusive writer ownership.
}
void mcl_eo_idempotent_report(struct mcl_nf_install_report *out, uint32_t *counts) {
    atomic_store(&native_queries,0); atomic_store(&native_writes,0);
    (void)mcl_nf_install_with_report(out);
    counts[0]=atomic_load(&native_queries); counts[1]=atomic_load(&native_writes);
}
uint32_t mcl_eo_reset_flag(void) { return SA_RESETHAND; }
uint32_t mcl_eo_onstack_flag(void) { return SA_ONSTACK; }
uint32_t mcl_eo_siginfo_flag(void) { return SA_SIGINFO; }
int32_t mcl_eo_altstack_disabled_flag(void) { return SS_DISABLE; }
int32_t mcl_eo_enotsup(void) { return ENOTSUP; }
