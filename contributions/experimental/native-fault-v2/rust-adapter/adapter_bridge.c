// Test-only white-box SOURCE inclusion. No native source file is changed.
// Direct build leaves mcl_nf_call's definition unchanged. Observed build only
// renames that definition so an assembly thunk can record immediate C-return
// flags before any Rust instruction. sigaction wrapper counts but never fails
// or substitutes the requested action; all calls still use real Darwin libc.
#include "adapter_bridge.h"
#include <errno.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
static int mcl_ra_count_sigaction(int, const struct sigaction *, struct sigaction *);
#if MCL_RA_OBSERVE
#define mcl_nf_call mcl_ra_impl_call
#endif
#define sigaction(sig, action, old) mcl_ra_count_sigaction((sig), (action), (old))
#include "../native-fault/native_fault.c"
#undef sigaction
#if MCL_RA_OBSERVE
#undef mcl_nf_call
#endif
_Static_assert(sizeof(struct mcl_ra_state) == 20, "state bridge ABI");
_Static_assert(sizeof(struct mcl_ra_action) == 16, "action summary bridge ABI");
_Static_assert(sizeof(struct mcl_ra_fixture) == 64, "fixture bridge ABI");
static atomic_uint query_count, write_count;
uint64_t mcl_ra_c_flags = UINT64_MAX;
int32_t mcl_ra_c_status = INT32_MIN;
uint32_t mcl_ra_c_calls;
static int mcl_ra_count_sigaction(int sig, const struct sigaction *action, struct sigaction *old) {
    atomic_fetch_add_explicit(action ? &write_count : &query_count, 1, memory_order_relaxed);
    return sigaction(sig, action, old);
}
size_t mcl_ra_layout(uint32_t *out, size_t capacity) {
    const uint32_t values[] = {
        sizeof(struct mcl_nf_install_report), _Alignof(struct mcl_nf_install_report),
        sizeof(struct mcl_nf_result), _Alignof(struct mcl_nf_result),
        offsetof(struct mcl_nf_install_report, primary_error), offsetof(struct mcl_nf_install_report, rollback_error),
        offsetof(struct mcl_nf_install_report, changed_mask), offsetof(struct mcl_nf_install_report, restored_mask),
        offsetof(struct mcl_nf_result, destination_next), offsetof(struct mcl_nf_result, source_next),
        offsetof(struct mcl_nf_result, remaining), offsetof(struct mcl_nf_result, completed),
        offsetof(struct mcl_nf_result, host_address), offsetof(struct mcl_nf_result, host_pc),
        offsetof(struct mcl_nf_result, host_rflags), offsetof(struct mcl_nf_result, signal_number),
        offsetof(struct mcl_nf_result, signal_code), offsetof(struct mcl_nf_result, writing),
        offsetof(struct mcl_nf_result, backward), offsetof(struct mcl_nf_result, exception_error),
        offsetof(struct mcl_nf_result, trap_number), offsetof(struct mcl_nf_result, original_mask),
        offsetof(struct mcl_nf_result, reserved)
    };
    const size_t count = sizeof(values) / sizeof(values[0]);
    if (out && capacity >= count) memcpy(out, values, sizeof(values));
    return count;
}
void mcl_ra_state_snapshot(struct mcl_ra_state *out) {
    struct mcl_ra_state value = {0};
    value.armed = atomic_load_explicit(&local_context.armed, memory_order_acquire);
    for (unsigned i = 0; i < MCL_NF_MAX_ACTIVE; ++i) {
        struct mcl_nf_context *ctx = atomic_load_explicit(&active_contexts[i].context, memory_order_acquire);
        value.active += ctx && (uintptr_t)ctx != 1;
        value.reserved += (uintptr_t)ctx == 1;
        value.local_registered += ctx == &local_context;
        value.nonzero_stack_cells += atomic_load_explicit(&active_contexts[i].stack, memory_order_acquire) != 0;
    }
    *out = value;
}
void mcl_ra_set_armed(uint32_t value) { atomic_store_explicit(&local_context.armed, value, memory_order_release); }
int mcl_ra_prepare_defaults(void) {
    // Explicitly owns ONLY this isolated test process. This may replace Rust
    // std's stack-overflow handlers; it is never an application integration.
    struct sigaction action = {0}; action.sa_handler = SIG_DFL;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGSEGV, &action, NULL) || sigaction(SIGBUS, &action, NULL)) return errno;
    return mcl_ra_set_mask(0);
}
static void known_custom(int sig) { (void)sig; }
int mcl_ra_prepare_custom(void) {
    int error = mcl_ra_prepare_defaults(); if (error) return error;
    struct sigaction action = {0}; action.sa_handler = known_custom;
    sigaddset(&action.sa_mask, SIGUSR2);
    if (sigaction(SIGBUS, &action, NULL)) return errno;
    return 0;
}
int mcl_ra_actions(struct mcl_ra_action *out) {
    const int numbers[2] = {SIGSEGV, SIGBUS};
    for (int i=0; i<2; ++i) {
        struct sigaction action;
        if (sigaction(numbers[i], NULL, &action)) return errno;
        out[i] = (struct mcl_ra_action){(uint64_t)(uintptr_t)action.sa_handler, action.sa_flags, action.sa_mask};
    }
    return 0;
}
void mcl_ra_sigaction_counts_reset(void) { atomic_store(&query_count, 0); atomic_store(&write_count, 0); }
void mcl_ra_sigaction_counts(uint32_t *out) { out[0] = atomic_load(&query_count); out[1] = atomic_load(&write_count); }
int mcl_ra_get_errno(void) { return errno; }
void mcl_ra_set_errno(int error) { errno = error; }
int mcl_ra_edom(void) { return EDOM; }
int mcl_ra_enotsup(void) { return ENOTSUP; }
uint32_t mcl_ra_mask(void) { sigset_t mask = 0; if (pthread_sigmask(SIG_SETMASK, NULL, &mask)) return UINT32_MAX; return mask; }
int mcl_ra_set_mask(uint32_t bits) { sigset_t mask = bits; return pthread_sigmask(SIG_SETMASK, &mask, NULL); }
uint32_t mcl_ra_usr1_mask(void) { sigset_t mask=0; sigaddset(&mask,SIGUSR1); return mask; }
uint32_t mcl_ra_segv_mask(void) { sigset_t mask=0; sigaddset(&mask,SIGSEGV); return mask; }
int mcl_ra_observation_mode(void) { return MCL_RA_OBSERVE; }
void mcl_ra_observation_reset(void) { mcl_ra_c_flags=UINT64_MAX; mcl_ra_c_status=INT32_MIN; mcl_ra_c_calls=0; }
uint64_t mcl_ra_observed_flags(void) { return mcl_ra_c_flags; }
int mcl_ra_observed_status(void) { return mcl_ra_c_status; }
uint32_t mcl_ra_observed_calls(void) { return mcl_ra_c_calls; }

static unsigned char *arena, *partial, *reference, *protected_page;
static size_t arena_size, page_size;
void mcl_ra_fixture_close(void) {
    if (arena) (void)munmap(arena, arena_size);
    free(partial); free(reference);
    arena=partial=reference=protected_page=NULL; arena_size=page_size=0;
}
int mcl_ra_fixture_setup(uint32_t kind, struct mcl_ra_fixture *out) {
    if (kind > 2 || arena) return EINVAL;
    page_size=(size_t)sysconf(_SC_PAGESIZE); if (page_size < 1024) return EINVAL;
    arena_size=page_size*6;
    arena=mmap(NULL,arena_size,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANON,-1,0);
    if (arena == MAP_FAILED) { arena=NULL; return errno; }
    partial=malloc(arena_size); reference=malloc(arena_size);
    if (!partial || !reference) { mcl_ra_fixture_close(); return ENOMEM; }
    for (size_t i=0;i<arena_size;++i) arena[i]=(unsigned char)((i*73+(i>>3)*19+11)&255);
    memcpy(partial,arena,arena_size); memcpy(reference,arena,arena_size);
    struct mcl_ra_fixture value={0}; value.length=page_size+512; value.page_size=page_size; value.value=0x6d;
    unsigned char *src=arena+page_size+256;
    unsigned char *dst=kind==1 ? src+128 : src-128;
    value.operation=kind==2 ? MCL_NF_FILL : MCL_NF_COPY;
    value.backward=kind==1; value.writing=kind!=0;
    value.completed=kind==0 ? page_size-256 : kind==1 ? 896 : page_size-128;
    if (kind==2) { src=NULL; dst=arena+page_size+128; }
    value.destination=(uintptr_t)dst; value.source=(uintptr_t)src;
    const size_t remaining=value.length-value.completed;
    value.fault_address=(uintptr_t)(value.writing ? dst : src)+(value.backward ? remaining-1 : value.completed);
    const size_t d=(size_t)(dst-arena), s=src ? (size_t)(src-arena) : 0;
    if (kind==2) { memset(reference+d,value.value,value.length); memset(partial+d,value.value,value.completed); }
    else {
        memmove(reference+d,reference+s,value.length);
        if (value.backward) memmove(partial+d+remaining,partial+s+remaining,value.completed);
        else memmove(partial+d,partial+s,value.completed);
    }
    protected_page=arena+(kind==1 ? page_size : page_size*2);
    if (mprotect(protected_page,page_size,kind==1 ? PROT_READ : PROT_NONE)) {
        const int error=errno; mcl_ra_fixture_close(); return error;
    }
    *out=value; return 0;
}
int mcl_ra_fixture_unprotect(void) { return mprotect(protected_page,page_size,PROT_READ|PROT_WRITE)==0 ? 0 : errno; }
int mcl_ra_fixture_matches(uint32_t full) { return arena && memcmp(arena,full ? reference : partial,arena_size)==0; }
