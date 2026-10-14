// Source-only prototype: capture owned REP faults, return normally with exact
// progress. This file deliberately does not run guest signal handlers or make
// a captured fault look like successful memcpy/memmove/memset.
#include "native_fault.h"
#include <errno.h>
#include <limits.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <sys/ucontext.h>
#include <unistd.h>

#if !defined(__APPLE__) || !defined(__x86_64__)
#error This prototype requires Darwin x86_64 SDK context definitions.
#endif
_Static_assert(ATOMIC_INT_LOCK_FREE == 2, "signal state requires lock-free atomic int");
_Static_assert(ATOMIC_POINTER_LOCK_FREE == 2, "descriptor registry requires lock-free pointers");
_Static_assert(ATOMIC_LONG_LOCK_FREE == 2, "saved stack registry requires lock-free uintptr_t");
_Static_assert(sizeof(uintptr_t) == 8, "Darwin x86-64 uintptr_t width");
_Static_assert(sizeof(atomic_uintptr_t) == 8, "assembly atomic stack cell width");
_Static_assert(_Alignof(atomic_uintptr_t) == 8, "assembly atomic stack cell alignment");
_Static_assert(sizeof(sigset_t) == sizeof(uint32_t), "Darwin signal mask width");
_Static_assert(sizeof(struct mcl_nf_result) == 88, "Rust/C result layout");
#define MCL_NF_OFFSET(type, field, expected) \
    _Static_assert(offsetof(type, field) == expected, "ABI offset: " #type "." #field)
MCL_NF_OFFSET(struct mcl_nf_result, destination_next, 0);
MCL_NF_OFFSET(struct mcl_nf_result, source_next, 8);
MCL_NF_OFFSET(struct mcl_nf_result, remaining, 16);
MCL_NF_OFFSET(struct mcl_nf_result, completed, 24);
MCL_NF_OFFSET(struct mcl_nf_result, host_address, 32);
MCL_NF_OFFSET(struct mcl_nf_result, host_pc, 40);
MCL_NF_OFFSET(struct mcl_nf_result, host_rflags, 48);
MCL_NF_OFFSET(struct mcl_nf_result, signal_number, 56);
MCL_NF_OFFSET(struct mcl_nf_result, signal_code, 60);
MCL_NF_OFFSET(struct mcl_nf_result, writing, 64);
MCL_NF_OFFSET(struct mcl_nf_result, backward, 68);
MCL_NF_OFFSET(struct mcl_nf_result, exception_error, 72);
MCL_NF_OFFSET(struct mcl_nf_result, trap_number, 76);
MCL_NF_OFFSET(struct mcl_nf_result, original_mask, 80);
MCL_NF_OFFSET(struct mcl_nf_result, reserved, 84);

_Static_assert(sizeof(struct mcl_nf_install_report) == 16, "install report ABI size");
MCL_NF_OFFSET(struct mcl_nf_install_report, primary_error, 0);
MCL_NF_OFFSET(struct mcl_nf_install_report, rollback_error, 4);
MCL_NF_OFFSET(struct mcl_nf_install_report, changed_mask, 8);
MCL_NF_OFFSET(struct mcl_nf_install_report, restored_mask, 12);

struct mcl_nf_context {
    atomic_uint armed; // 0 idle, 1 armed, 2 publishing, 3 captured, 4 preparing
    uint32_t operation;
    uintptr_t destination;
    uintptr_t source;
    size_t length;
    uint32_t backward;
    uint32_t original_mask;
    uintptr_t leaf_stack_slot;
    struct mcl_nf_result result;
};
MCL_NF_OFFSET(struct mcl_nf_context, armed, 0);
MCL_NF_OFFSET(struct mcl_nf_context, operation, 4);
MCL_NF_OFFSET(struct mcl_nf_context, destination, 8);
MCL_NF_OFFSET(struct mcl_nf_context, source, 16);
MCL_NF_OFFSET(struct mcl_nf_context, length, 24);
MCL_NF_OFFSET(struct mcl_nf_context, backward, 32);
MCL_NF_OFFSET(struct mcl_nf_context, original_mask, 36);
MCL_NF_OFFSET(struct mcl_nf_context, leaf_stack_slot, 40);
MCL_NF_OFFSET(struct mcl_nf_context, result, 48);
_Static_assert(sizeof(struct mcl_nf_context) == 136, "descriptor ABI size");
static _Thread_local struct mcl_nf_context local_context;
// A bad saved r9 must not be dereferenced inside the callback. Register only
// currently live descriptors and verify pointer membership first. Reservation
// is bounded; capacity failure declines before touching any guest buffer.
#define MCL_NF_MAX_ACTIVE 1024
struct active_slot { _Atomic(struct mcl_nf_context *) context; atomic_uintptr_t stack; };
MCL_NF_OFFSET(struct active_slot, context, 0);
MCL_NF_OFFSET(struct active_slot, stack, 8);
_Static_assert(sizeof(struct active_slot) == 16, "registry ABI stride");
// The foreign assembly leaf performs an aligned 64-bit MOV to stack. This is
// the Darwin/x86-64 ABI contract for an atomic release store under x86 TSO;
// it is not a portable ISO C way to implement stdatomic operations. Validate
// compiled layout and actual-machine publication before integrating it.
static struct active_slot active_contexts[MCL_NF_MAX_ACTIVE];
static _Thread_local unsigned preferred_slot;
static atomic_uint installed;

extern int mcl_nf_copy_leaf(struct mcl_nf_context *, uintptr_t, uintptr_t, size_t, unsigned);
extern int mcl_nf_fill_leaf(struct mcl_nf_context *, uintptr_t, unsigned, size_t);
extern const unsigned char mcl_nf_copy_forward_site[], mcl_nf_copy_backward_site[], mcl_nf_fill_site[];
extern void mcl_nf_fault_return(void);
extern void mcl_nf_signal_entry(int, siginfo_t *, void *);

static int synchronous_code(int signal_number, int code) {
    // Darwin SI_USER/SI_QUEUE are positive too. Never use only code > 0.
    return signal_number == SIGSEGV ? code == SEGV_MAPERR || code == SEGV_ACCERR
        : signal_number == SIGBUS && (code == BUS_ADRERR || code == BUS_OBJERR);
}
static int valid_range(uintptr_t start, size_t length) {
    const uintptr_t end = (uintptr_t)1 << 47;
    return start && length <= (size_t)PTRDIFF_MAX && start < end && length <= end - start;
}
static int overlaps(uintptr_t start, size_t length, uintptr_t other, size_t size) {
    return start < other + size && other < start + length;
}
static int reserve_context(void) {
    for (unsigned i = 0; i < MCL_NF_MAX_ACTIVE; ++i) {
        unsigned index = (preferred_slot + i) % MCL_NF_MAX_ACTIVE;
        struct mcl_nf_context *empty = NULL;
        if (atomic_compare_exchange_strong_explicit(&active_contexts[index].context, &empty,
                (struct mcl_nf_context *)(uintptr_t)1, memory_order_acq_rel, memory_order_relaxed)) {
            atomic_store_explicit(&active_contexts[index].stack, 0, memory_order_relaxed);
            preferred_slot = index;
            return (int)index;
        }
    }
    return -1;
}
static int owned_context(const struct mcl_nf_context *ctx, uintptr_t stack_slot, uintptr_t stack) {
    // A reserved-but-unpublished slot contains sentinel 1 and stack 0. Neither
    // can certify an owner, even if corrupted saved registers match them.
    if (!ctx || (uintptr_t)ctx == 1 || !stack) return 0;
    for (unsigned i = 0; i < MCL_NF_MAX_ACTIVE; ++i) {
        // Compare the untrusted saved r10 to known static addresses. Do not
        // dereference it (or r9) before ownership of this exact host RSP is
        // proved by the registered pointer and the leaf's aligned atomic store.
        if ((uintptr_t)&active_contexts[i].stack == stack_slot) {
            return atomic_load_explicit(&active_contexts[i].context, memory_order_acquire) == ctx
                && atomic_load_explicit(&active_contexts[i].stack, memory_order_acquire) == stack;
        }
    }
    return 0;
}

static _Noreturn void default_fatal(int signal_number) {
    struct sigaction action = {0};
    action.sa_handler = SIG_DFL;
    sigemptyset(&action.sa_mask);
    // This process is committed to a fatal signal. Default termination is the
    // only process-wide disposition mutation from a callback; never temporarily
    // uninstall a capture handler and then keep the process alive.
    if (sigaction(signal_number, &action, NULL) == 0) {
        sigset_t mask;
        if (sigprocmask(SIG_SETMASK, NULL, &mask) == 0) {
            sigdelset(&mask, signal_number);
            if (sigprocmask(SIG_SETMASK, &mask, NULL) == 0) raise(signal_number);
        }
    }
    _exit(128 + signal_number); // Only a failure of the fatal host path.
}

// v2 deliberately supports only prior DFL dispositions. No queried action
// is treated as authority for hidden Darwin one-shot state, and no custom
// callback is called by this dispatcher. Unmatched signals terminate using
// the canonical default action: handler DFL, flags 0, mask empty.
static _Noreturn void unmatched_fatal(int signal_number, siginfo_t *info, ucontext_t *context) {
    (void)info;
    (void)context;
    default_fatal(signal_number);
}

// Reached through an assembly CLD entry. Only lock-free state/scalar memory
// accesses on the capture path; no allocation, mutex, TLV lookup or Rust call.
void mcl_nf_receive(int signal_number, siginfo_t *info, void *opaque_context) {
    ucontext_t *uc = opaque_context;
    if (!info || !uc || !uc->uc_mcontext || !synchronous_code(signal_number, info->si_code)
        || uc->uc_mcsize < offsetof(struct __darwin_mcontext64, __ss) + sizeof(uc->uc_mcontext->__ss)) {
        unmatched_fatal(signal_number, info, uc);
        return;
    }
    mcontext_t mc = uc->uc_mcontext;
    const uint64_t pc = mc->__ss.__rip;
    const int copy_forward = pc == (uintptr_t)mcl_nf_copy_forward_site;
    const int copy_backward = pc == (uintptr_t)mcl_nf_copy_backward_site;
    const int fill = pc == (uintptr_t)mcl_nf_fill_site;
    // Never initialize/access TLV on an unrelated host thread. A matching leaf
    // always got its descriptor from a normal-path, initialized call in r9.
    if (!(copy_forward || copy_backward || fill) || mc->__es.__trapno != 14
        || (mc->__es.__err & (1u << 4))) { // Instruction fetch is not a data fault.
        unmatched_fatal(signal_number, info, uc);
        return;
    }
    struct mcl_nf_context *ctx = (void *)(uintptr_t)mc->__ss.__r9;
    if (!ctx || !owned_context(ctx, mc->__ss.__r10, mc->__ss.__rsp)
        || atomic_load_explicit(&ctx->armed, memory_order_acquire) != 1
        || (fill ? ctx->operation != MCL_NF_FILL : ctx->operation != MCL_NF_COPY)
        || ctx->backward != (uint32_t)copy_backward) {
        unmatched_fatal(signal_number, info, uc);
        return;
    }
    const uint64_t remaining = mc->__ss.__rcx;
    const uint64_t destination = mc->__ss.__rdi;
    const uint64_t source = fill ? 0 : mc->__ss.__rsi;
    const int writing = (mc->__es.__err & 2) != 0;
    if (!remaining || remaining > ctx->length || (fill && !writing)) {
        unmatched_fatal(signal_number, info, uc);
        return;
    }
    const uint64_t completed = ctx->length - remaining;
    const uint64_t offset = copy_backward ? remaining - 1 : completed;
    const uintptr_t fault_address = (uintptr_t)info->si_addr;
    if (destination != ctx->destination + offset || (!fill && source != ctx->source + offset)
        || fault_address != (writing ? destination : source)
        || mc->__es.__faultvaddr != fault_address
        || ((mc->__ss.__rflags >> 10) & 1) != (uint64_t)copy_backward) {
        unmatched_fatal(signal_number, info, uc);
        return;
    }
    unsigned armed = 1;
    if (!atomic_compare_exchange_strong_explicit(&ctx->armed, &armed, 2,
            memory_order_acq_rel, memory_order_acquire)) {
        unmatched_fatal(signal_number, info, uc);
        return;
    }
    ctx->result.destination_next = destination;
    ctx->result.source_next = source;
    ctx->result.remaining = remaining;
    ctx->result.completed = completed;
    ctx->result.host_address = fault_address;
    ctx->result.host_pc = pc;
    ctx->result.host_rflags = mc->__ss.__rflags;
    ctx->result.signal_number = signal_number;
    ctx->result.signal_code = info->si_code;
    ctx->result.writing = writing;
    ctx->result.backward = copy_backward;
    ctx->result.exception_error = mc->__es.__err;
    ctx->result.trap_number = mc->__es.__trapno;
    ctx->result.original_mask = ctx->original_mask;
    ctx->result.reserved = 0;
    atomic_store_explicit(&ctx->armed, 3, memory_order_release);
    mc->__ss.__rflags &= ~(UINT64_C(1) << 10); // No C/Rust return with DF set.
    mc->__ss.__rip = (uintptr_t)mcl_nf_fault_return;
}

int mcl_nf_install_with_report(struct mcl_nf_install_report *output) {
    // Startup-only: the caller controls all native sigaction changes and has
    // not started guest threads. This is not a live-process installation API.
    struct mcl_nf_install_report report = {0};
    if (atomic_load_explicit(&installed, memory_order_acquire)) goto finished;
    const int signal_numbers[2] = {SIGSEGV, SIGBUS};
    struct sigaction visible_before[2];
    for (int i = 0; i < 2; ++i) {
        if (sigaction(signal_numbers[i], NULL, &visible_before[i])) {
            report.primary_error = errno;
            goto finished;
        }
    }
    // Query both signals before the first write. Darwin sigaction queries omit
    // SA_RESETHAND on this host, so checking that bit cannot prove a custom
    // callback is persistent. Reject ALL unknown custom and ignored actions.
    for (int i = 0; i < 2; ++i) {
        if (visible_before[i].sa_handler != SIG_DFL) {
            report.primary_error = ENOTSUP;
            goto finished;
        }
    }
    for (int i = 0; i < 2; ++i) {
        struct sigaction action = {0};
        action.sa_sigaction = mcl_nf_signal_entry;
        action.sa_flags = SA_SIGINFO;
        sigemptyset(&action.sa_mask);
        sigaddset(&action.sa_mask, SIGSEGV);
        sigaddset(&action.sa_mask, SIGBUS);
        // Dispatcher and unmatched fallback use canonical masks/flags. Prior
        // DFL flags/masks are observable snapshots for FAILED-install rollback
        // only; they do not define supported custom chaining semantics.
        if (sigaction(signal_numbers[i], &action, NULL)) {
            report.primary_error = errno;
            if (i) {
                // This restores DFL behavior and the query-visible mask/flags.
                // A hidden raw SA_RESETHAND bit cannot be recovered by query;
                // raw disposition state is intentionally NOT claimed restored.
                if (sigaction(SIGSEGV, &visible_before[0], NULL))
                    report.rollback_error = errno;
                else report.restored_mask = MCL_NF_SIGNAL_SEGV;
            }
            goto finished;
        }
        report.changed_mask |= i ? MCL_NF_SIGNAL_BUS : MCL_NF_SIGNAL_SEGV;
    }
    atomic_store_explicit(&installed, 1, memory_order_release);
finished:
    if (output) *output = report;
    return report.primary_error;
}

int mcl_nf_install(void) { return mcl_nf_install_with_report(NULL); }

void mcl_nf_enter_thread(void) {
    // Reading the TLV outside a callback initializes Darwin's descriptor.
    (void)atomic_load_explicit(&local_context.armed, memory_order_relaxed);
}

int mcl_nf_call(uint32_t operation, uintptr_t destination, uintptr_t source,
                size_t length, unsigned value, struct mcl_nf_result *output) {
    const int saved_errno = errno;
    int status = MCL_NF_DECLINED;
    if (!output) goto done;
    if (operation != MCL_NF_COPY && operation != MCL_NF_FILL) goto done;
    // Do not inspect either pointer for an empty operation.
    if (!length) {
        *output = (struct mcl_nf_result){0};
        status = MCL_NF_OK;
        goto done;
    }
    if (!valid_range(destination, length) || (operation == MCL_NF_COPY && !valid_range(source, length))) goto done;
    // A declined call must not modify either guest interval by clearing an
    // aliased result first. Output remains untouched on these decline paths.
    if (overlaps(destination, length, (uintptr_t)output, sizeof(*output))
        || (operation == MCL_NF_COPY && overlaps(source, length, (uintptr_t)output, sizeof(*output)))) goto done;
    *output = (struct mcl_nf_result){0};
    if (operation == MCL_NF_COPY && destination == source) {
        output->completed = length;
        status = MCL_NF_OK;
        goto done;
    }
    if (!atomic_load_explicit(&installed, memory_order_acquire)) goto done;
    struct mcl_nf_context *ctx = &local_context;
    if (overlaps(destination, length, (uintptr_t)ctx, sizeof(*ctx))
        || overlaps(destination, length, (uintptr_t)active_contexts, sizeof(active_contexts))) goto done;
    // Own preparation before writing any descriptor field. An unrelated async
    // native callback can reenter on this host thread even outside the REP; checking
    // idle and only arming after initialization would let it overwrite ctx.
    unsigned idle = 0;
    if (!atomic_compare_exchange_strong_explicit(&ctx->armed, &idle, 4,
            memory_order_acq_rel, memory_order_acquire)) {
        status = MCL_NF_REENTRANT;
        goto done;
    }
    sigset_t mask;
    int error = pthread_sigmask(SIG_SETMASK, NULL, &mask);
    if (error) goto release_preparing;
    // Blocking hardware faults does not turn into successful copies. Capturing
    // them while blocked has no portable/Darwin return contract; decline before
    // mutation so the future guest caller can choose its explicit fatal path.
    if (sigismember(&mask, SIGSEGV) || sigismember(&mask, SIGBUS)) goto release_preparing;
    ctx->operation = operation;
    ctx->destination = destination;
    ctx->source = source;
    ctx->length = length;
    ctx->backward = operation == MCL_NF_COPY && destination > source && destination - source < length;
    ctx->original_mask = mask;
    ctx->result = (struct mcl_nf_result){0};
    int slot = reserve_context();
    if (slot < 0) goto release_preparing;
    ctx->leaf_stack_slot = (uintptr_t)&active_contexts[slot].stack;
    atomic_store_explicit(&ctx->armed, 1, memory_order_release);
    atomic_store_explicit(&active_contexts[slot].context, ctx, memory_order_release);
    status = operation == MCL_NF_COPY
        ? mcl_nf_copy_leaf(ctx, destination, source, length, ctx->backward)
        : mcl_nf_fill_leaf(ctx, destination, value & 255, length);
    if (status == MCL_NF_FAULT) {
        if (atomic_load_explicit(&ctx->armed, memory_order_acquire) != 3) status = MCL_NF_INTERNAL;
        else *output = ctx->result;
    } else if (status == MCL_NF_OK) {
        output->completed = length;
        output->backward = ctx->backward;
        output->original_mask = mask;
    } else status = MCL_NF_INTERNAL;
    atomic_store_explicit(&ctx->armed, 0, memory_order_release);
    atomic_store_explicit(&active_contexts[slot].context, NULL, memory_order_release);
    goto done;
release_preparing:
    atomic_store_explicit(&ctx->armed, 0, memory_order_release);
done:
    errno = saved_errno;
    return status;
}
