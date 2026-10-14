// Darwin x86-64 public-API host probes. Built/run only by the parent later.
#include "native_fault.h"
#include <errno.h>
#include <inttypes.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ucontext.h>
#include <unistd.h>

#if !defined(__APPLE__) || !defined(__x86_64__)
#error These probes require actual Darwin x86-64 hardware.
#endif

extern uint64_t mcl_probe_read_flags(void);
extern int mcl_probe_call_and_flags(uint32_t, uintptr_t, uintptr_t, size_t, unsigned,
                                   struct mcl_nf_result *, uint64_t *);
extern void mcl_probe_nonleaf_fault(void *);
static int failures;
static uint64_t last_return_flags;
#define CHECK(condition, message) do { \
    if (!(condition)) { fprintf(stderr, "FAIL: %s (%s:%d)\n", message, __FILE__, __LINE__); ++failures; } \
} while (0)
#define REQUIRE(condition, message) do { \
    const int require_ok = !!(condition); CHECK(require_ok, message); if (!require_ok) goto cleanup; \
} while (0)

static int probe_call(uint32_t operation, uintptr_t destination, uintptr_t source,
                      size_t length, unsigned value, struct mcl_nf_result *result) {
    uint64_t flags = UINT64_MAX;
    const int status = mcl_probe_call_and_flags(operation, destination, source, length, value, result, &flags);
    last_return_flags = flags;
    CHECK(!(flags & (UINT64_C(1) << 10)), "immediate primitive return DF clear before C/errno");
    return status;
}

static sigset_t current_mask(void) {
    sigset_t mask = 0;
    int error = pthread_sigmask(SIG_SETMASK, NULL, &mask);
    CHECK(error == 0, "query current mask");
    return mask;
}
static void set_mask(sigset_t mask) {
    CHECK(pthread_sigmask(SIG_SETMASK, &mask, NULL) == 0, "set current mask");
}
static void prepare_default_install(void) {
    sigset_t empty = 0;
    set_mask(empty);
    struct sigaction action = {0};
    action.sa_handler = SIG_DFL;
    sigemptyset(&action.sa_mask);
    CHECK(sigaction(SIGSEGV, &action, NULL) == 0, "set prior SEGV default");
    CHECK(sigaction(SIGBUS, &action, NULL) == 0, "set prior BUS default");
    CHECK(mcl_nf_install() == 0, "install fault capture");
    mcl_nf_enter_thread();
}
static void seed(unsigned char *bytes, size_t length) {
    for (size_t i = 0; i < length; ++i)
        bytes[i] = (unsigned char)((i * UINT64_C(73) + (i >> 3) * UINT64_C(19) + 11) & 255);
}

// Every case has a fresh process. Both reference images are computed before
// protecting pages, so a retry of already-mutated overlapping data cannot
// accidentally become the reference.
static void test_memory(const char *name) {
    const int backward = strncmp(name, "backward-", 9) == 0;
    const int fill = strncmp(name, "fill-", 5) == 0;
    const int write_fault = fill || strstr(name, "-write-") != NULL;
    const int at_start = strstr(name, "-start") != NULL;
    const size_t page = (size_t)sysconf(_SC_PAGESIZE);
    const size_t arena_size = page * 6;
    unsigned char *arena = MAP_FAILED;
    unsigned char *before = NULL, *partial = NULL, *expected = NULL;
    unsigned char *protected_page = NULL, *source = NULL, *destination = NULL;
    const size_t length = page + 512;
    const unsigned value = 0x6d;
    size_t expected_completed = 0;
    prepare_default_install();
    REQUIRE(page >= 1024, "valid host page size");
    arena = mmap(NULL, arena_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    REQUIRE(arena != MAP_FAILED, "map six-page arena");
    before = malloc(arena_size);
    partial = malloc(arena_size);
    expected = malloc(arena_size);
    REQUIRE(before && partial && expected, "allocate independent reference images");
    seed(arena, arena_size);
    memcpy(before, arena, arena_size);
    memcpy(partial, before, arena_size);
    memcpy(expected, before, arena_size);
    if (fill) {
        destination = arena + (at_start ? page * 2 : page + 128);
        protected_page = arena + page * 2;
        expected_completed = at_start ? 0 : page - 128;
    } else if (backward) {
        source = arena + page + 256;
        destination = source + 128;
        protected_page = arena + (at_start ? page * 2 : page);
        expected_completed = at_start ? 0 : (write_fault ? 896 : 768);
    } else {
        if (at_start) {
            if (write_fault) {
                destination = arena + page * 2;
                source = destination + 128;
            } else {
                source = arena + page * 2;
                destination = source - 128;
            }
        } else {
            source = arena + page + 256;
            destination = source - 128;
        }
        protected_page = arena + page * 2;
        expected_completed = at_start ? 0 : (write_fault ? page - 128 : page - 256);
    }
    const size_t source_offset = source ? (size_t)(source - arena) : 0;
    const size_t destination_offset = (size_t)(destination - arena);
    const size_t remaining = length - expected_completed;
    if (fill) {
        memset(expected + destination_offset, value, length);
        memset(partial + destination_offset, value, expected_completed);
    } else {
        memmove(expected + destination_offset, expected + source_offset, length);
        if (backward)
            memmove(partial + destination_offset + remaining, partial + source_offset + remaining, expected_completed);
        else
            memmove(partial + destination_offset, partial + source_offset, expected_completed);
    }
    REQUIRE(mprotect(protected_page, page, write_fault && !fill ? PROT_READ : PROT_NONE) == 0,
            "protect source/destination page");
    const sigset_t mask_before = current_mask();
    struct mcl_nf_result first = {0};
    errno = EDOM;
    const int first_status = probe_call(fill ? MCL_NF_FILL : MCL_NF_COPY,
        (uintptr_t)destination, (uintptr_t)source, length, value, &first);
    const int first_errno = errno;
    const uint64_t flags_after_first = last_return_flags;
    CHECK(first_errno == EDOM, "fault return preserves errno");
    CHECK(!(flags_after_first & (UINT64_C(1) << 10)), "fault return clears DF");
    CHECK(current_mask() == mask_before, "fault sigreturn preserves original mask");
    REQUIRE(first_status == MCL_NF_FAULT, "actual protected-page access returns FAULT");
    CHECK(first.completed == expected_completed, "exact completed count");
    CHECK(first.remaining == remaining && first.completed + first.remaining == length, "exact remaining count");
    CHECK(first.backward == backward, "original overlap direction");
    CHECK(first.writing == write_fault, "read/write classification");
    const uintptr_t next_destination = (uintptr_t)destination + (backward ? remaining - 1 : expected_completed);
    const uintptr_t next_source = fill ? 0 : (uintptr_t)source + (backward ? remaining - 1 : expected_completed);
    CHECK(first.destination_next == next_destination, "exact destination cursor");
    CHECK(first.source_next == next_source, "exact source cursor");
    CHECK(first.host_address == (write_fault ? next_destination : next_source), "fault address is current accessed byte");
    CHECK(first.host_pc != 0, "host fault PC present");
    CHECK(first.signal_number == SIGSEGV || first.signal_number == SIGBUS, "hardware fault signal");
    CHECK(first.signal_number == SIGSEGV
        ? first.signal_code == SEGV_MAPERR || first.signal_code == SEGV_ACCERR
        : first.signal_number == SIGBUS && (first.signal_code == BUS_ADRERR || first.signal_code == BUS_OBJERR),
        "known data-fault code for actual signal");
    CHECK(first.trap_number == 14, "page-fault trap number");
    CHECK(((first.exception_error >> 1) & 1) == (unsigned)write_fault, "page-fault write bit");
    CHECK(!(first.exception_error & (1u << 4)), "not an instruction-fetch fault");
    CHECK(((first.host_rflags >> 10) & 1) == (unsigned)backward, "saved DF records original direction");
    CHECK(first.original_mask == (uint32_t)mask_before && first.reserved == 0, "result mask/reserved");
    REQUIRE(first.completed == expected_completed && first.remaining == remaining
        && first.destination_next == next_destination && first.source_next == next_source,
        "exact in-arena cursor required before continuing");
    REQUIRE(mprotect(protected_page, page, PROT_READ | PROT_WRITE) == 0, "restore protected page");
    CHECK(memcmp(arena, partial, arena_size) == 0, "entire arena matches precise partial operation");

    // This is manual host continuation, not a guest RESUME implementation.
    // Reverse cursors point at the last byte of the remaining prefix.
    const uintptr_t resume_destination = backward
        ? first.destination_next - (first.remaining - 1) : first.destination_next;
    const uintptr_t resume_source = fill ? 0 : backward
        ? first.source_next - (first.remaining - 1) : first.source_next;
    struct mcl_nf_result second = {0};
    errno = ERANGE;
    const int second_status = probe_call(fill ? MCL_NF_FILL : MCL_NF_COPY,
        resume_destination, resume_source, first.remaining, value, &second);
    const int second_errno = errno;
    const uint64_t flags_after_second = last_return_flags;
    CHECK(second_status == MCL_NF_OK, "only remaining interval completes");
    CHECK(second.completed == first.remaining, "continuation completed count");
    CHECK(second_errno == ERANGE, "success return preserves errno");
    CHECK(!(flags_after_second & (UINT64_C(1) << 10)), "continuation returns with DF clear");
    CHECK(current_mask() == mask_before, "continuation preserves mask");
    CHECK(memcmp(arena, expected, arena_size) == 0, "full-buffer/canaries equal original one-shot reference");
    printf("{\"observation\":\"host-partial-and-resume\",\"completed\":%" PRIu64
           ",\"remaining\":%" PRIu64 ",\"backward\":%d,\"writing\":%d}\n",
           first.completed, first.remaining, first.backward, first.writing);
cleanup:
    if (arena != MAP_FAILED) {
        if (protected_page) (void)mprotect(protected_page, page, PROT_READ | PROT_WRITE);
        (void)munmap(arena, arena_size);
    }
    free(expected);
    free(partial);
    free(before);
}

static void test_success_and_decline(void) {
    prepare_default_install();
    unsigned char bytes[512], reference[512], source[64];
    struct mcl_nf_result result;
    seed(bytes, sizeof(bytes));
    memcpy(reference, bytes, sizeof(bytes));
    seed(source, sizeof(source));
    errno = EDOM;
    CHECK(probe_call(MCL_NF_COPY, (uintptr_t)bytes + 17, (uintptr_t)bytes, 200, 0, &result) == MCL_NF_OK,
          "successful backward overlap");
    CHECK(errno == EDOM, "successful overlap errno");
    CHECK(!(last_return_flags & (UINT64_C(1) << 10)), "successful backward overlap DF");
    memmove(reference + 17, reference, 200);
    CHECK(memcmp(bytes, reference, sizeof(bytes)) == 0, "successful overlap full buffer");
    CHECK(probe_call(MCL_NF_COPY, UINTPTR_MAX, UINTPTR_MAX, 0, 0, &result) == MCL_NF_OK,
          "zero size does not inspect invalid inputs");
    CHECK(result.completed == 0, "zero result count");
    CHECK(probe_call(MCL_NF_COPY, (uintptr_t)bytes, (uintptr_t)bytes, sizeof(bytes), 0, &result) == MCL_NF_OK,
          "same normalized pointer no-op");
    CHECK(result.completed == sizeof(bytes), "same pointer count");
    CHECK(probe_call(MCL_NF_COPY, (UINT64_C(1) << 47), (uintptr_t)source, 1, 0, &result) == MCL_NF_DECLINED,
          "noncanonical interval decline");
    CHECK(probe_call(MCL_NF_FILL, (UINT64_C(1) << 47) - 2, 0, 4, 0, &result) == MCL_NF_DECLINED,
          "end crossing canonical boundary decline");
    CHECK(memcmp(bytes, reference, sizeof(bytes)) == 0, "declines leave guest buffer unchanged");
    const sigset_t original = current_mask();
    sigset_t blocked = original;
    sigaddset(&blocked, SIGSEGV);
    set_mask(blocked);
    errno = ERANGE;
    CHECK(probe_call(MCL_NF_FILL, (uintptr_t)bytes, 0, sizeof(bytes), 0, &result) == MCL_NF_DECLINED,
          "blocked hardware signal decline");
    CHECK(errno == ERANGE && current_mask() == blocked, "blocked decline preserves errno/mask");
    CHECK(memcmp(bytes, reference, sizeof(bytes)) == 0, "blocked decline no guest mutation");
    set_mask(original);
}

static void test_output_alias(void) {
    prepare_default_install();
    union { struct mcl_nf_result result; unsigned char bytes[256]; } record;
    unsigned char before[sizeof(record)], other[256], other_before[256];
    seed(record.bytes, sizeof(record));
    memcpy(before, &record, sizeof(record));
    seed(other, sizeof(other));
    memcpy(other_before, other, sizeof(other));
    errno = EDOM;
    CHECK(probe_call(MCL_NF_COPY, (uintptr_t)&record.result, (uintptr_t)other, 64, 0,
          &record.result) == MCL_NF_DECLINED, "destination/result alias rejected");
    CHECK(errno == EDOM && memcmp(&record, before, sizeof(record)) == 0, "destination alias untouched");
    CHECK(probe_call(MCL_NF_COPY, (uintptr_t)other, (uintptr_t)&record.result, 64, 0,
          &record.result) == MCL_NF_DECLINED, "source/result alias rejected");
    CHECK(memcmp(&record, before, sizeof(record)) == 0, "source alias untouched");
    CHECK(memcmp(other, other_before, sizeof(other)) == 0, "source alias rejection target untouched");
    CHECK(probe_call(MCL_NF_FILL, (uintptr_t)&record.result + 8, 0, 16, 9,
          &record.result) == MCL_NF_DECLINED, "partial destination/result alias rejected");
    CHECK(memcmp(&record, before, sizeof(record)) == 0, "partial result alias untouched");
}

static volatile sig_atomic_t handler_calls;
static void prior_simple(int signal_number) {
    (void)signal_number;
    ++handler_calls;
}
static void prior_info(int signal_number, siginfo_t *info, void *opaque) {
    (void)signal_number; (void)info; (void)opaque;
    ++handler_calls;
}
static int same_action(const struct sigaction *left, const struct sigaction *right) {
    return left->sa_handler == right->sa_handler
        && left->sa_flags == right->sa_flags && left->sa_mask == right->sa_mask;
}
static void fatal_ready(void) {
    const char ready[] = "{\"fatal_trigger_ready\":true}\n";
    CHECK(write(STDOUT_FILENO, ready, sizeof(ready) - 1) == (ssize_t)(sizeof(ready) - 1),
          "write fatal trigger ready marker");
}
static void test_preflight(const char *name) {
    set_mask(0);
    const int target = strstr(name, "-bus") ? SIGBUS : SIGSEGV;
    const int ignored = strncmp(name, "ignore-", 7) == 0;
    const int oneshot = strncmp(name, "oneshot-", 8) == 0;
    const int simple = strncmp(name, "simple-", 7) == 0;
    struct sigaction action = {0}, before[2], after[2];
    action.sa_handler = SIG_DFL;
    CHECK(sigaction(SIGSEGV, &action, NULL) == 0 && sigaction(SIGBUS, &action, NULL) == 0,
          "initialize both default dispositions");
    action.sa_sigaction = prior_info;
    action.sa_flags = SA_SIGINFO | (oneshot ? SA_RESETHAND : 0);
    sigaddset(&action.sa_mask, SIGUSR2);
    if (ignored || simple) {
        action.sa_handler = ignored ? SIG_IGN : prior_simple;
        action.sa_flags = 0;
    }
    REQUIRE(sigaction(target, &action, NULL) == 0, "set unsupported prior action");
    REQUIRE(sigaction(SIGSEGV, NULL, &before[0]) == 0 && sigaction(SIGBUS, NULL, &before[1]) == 0,
            "query original actions");
    struct mcl_nf_install_report report = {0};
    CHECK(mcl_nf_install_with_report(&report) == ENOTSUP, "any unknown prior action rejected");
    CHECK(report.primary_error == ENOTSUP && report.rollback_error == 0
        && report.changed_mask == 0 && report.restored_mask == 0, "rejection report zero changed/restored masks");
    REQUIRE(sigaction(SIGSEGV, NULL, &after[0]) == 0 && sigaction(SIGBUS, NULL, &after[1]) == 0,
            "query post-rejection actions");
    CHECK(same_action(&before[0], &after[0]) && same_action(&before[1], &after[1]),
          "both observable dispositions unchanged");
    if (!ignored) {
        CHECK(raise(target) == 0 && handler_calls == 1, "first trigger invokes original callback once");
        if (oneshot) {
            REQUIRE(sigaction(target, NULL, &after[0]) == 0, "query after first one-shot callback");
            CHECK(after[0].sa_handler == SIG_DFL, "actual one-shot reset survives installation rejection");
            if (failures) goto cleanup;
            const char first[] = "{\"oneshot_first_callback\":true,\"pass\":true}\n";
            CHECK(write(STDOUT_FILENO, first, sizeof(first) - 1) == (ssize_t)(sizeof(first) - 1),
                  "write first callback confirmation");
            if (failures) goto cleanup;
            fatal_ready();
            if (failures) goto cleanup;
            raise(target);
            CHECK(0, "second one-shot trigger must default-terminate");
        } else CHECK(raise(target) == 0 && handler_calls == 2, "persistent original callback remains persistent");
    } else CHECK(raise(target) == 0 && handler_calls == 0, "original ignored action remains ignored");
cleanup:
    return;
}
static void test_default_flags(void) {
    struct sigaction action = {0}, observed[2];
    action.sa_handler = SIG_DFL;
    action.sa_flags = SA_ONSTACK | SA_RESTART | SA_NODEFER | SA_RESETHAND;
    sigaddset(&action.sa_mask, SIGUSR1);
    sigaddset(&action.sa_mask, SIGUSR2);
    REQUIRE(sigaction(SIGSEGV, &action, NULL) == 0 && sigaction(SIGBUS, &action, NULL) == 0,
            "set DFL with noncanonical flags/masks");
    struct mcl_nf_install_report report;
    REQUIRE(mcl_nf_install_with_report(&report) == 0, "install on DFL with noncanonical flags/masks");
    CHECK(report.primary_error == 0 && report.rollback_error == 0
        && report.changed_mask == (MCL_NF_SIGNAL_SEGV | MCL_NF_SIGNAL_BUS) && report.restored_mask == 0,
        "successful install report");
    REQUIRE(sigaction(SIGSEGV, NULL, &observed[0]) == 0 && sigaction(SIGBUS, NULL, &observed[1]) == 0,
            "query canonical dispatcher actions");
    sigset_t expected_mask = 0;
    sigaddset(&expected_mask, SIGSEGV);
    sigaddset(&expected_mask, SIGBUS);
    CHECK(observed[0].sa_handler != SIG_DFL && observed[0].sa_handler != SIG_IGN
        && observed[0].sa_handler == observed[1].sa_handler, "same capture dispatcher installed");
    CHECK(observed[0].sa_flags == SA_SIGINFO && observed[1].sa_flags == SA_SIGINFO,
          "dispatcher uses only canonical SIGINFO flags");
    CHECK(observed[0].sa_mask == expected_mask && observed[1].sa_mask == expected_mask,
          "dispatcher uses only hardware-signal capture mask");
    CHECK(mcl_nf_install_with_report(&report) == 0 && report.changed_mask == 0 && report.restored_mask == 0,
          "successful installation is idempotent under no-mutation contract");
cleanup:
    return;
}

static void test_nonleaf(const char *name) {
    const size_t page = (size_t)sysconf(_SC_PAGESIZE);
    void *guard = mmap(NULL, page, PROT_NONE, MAP_PRIVATE | MAP_ANON, -1, 0);
    if (guard == MAP_FAILED) { CHECK(0, "map nonleaf guard"); return; }
    (void)name;
    prepare_default_install();
    if (failures) { (void)munmap(guard, page); return; }
    fatal_ready();
    if (failures) { (void)munmap(guard, page); return; }
    mcl_probe_nonleaf_fault(guard);
    CHECK(0, "nonleaf protected write must signal, not return");
    (void)munmap(guard, page);
}

int main(int argc, char **argv) {
    if (argc != 2) { fprintf(stderr, "one case name required\n"); return 2; }
    const char *name = argv[1];
    if (strncmp(name, "forward-", 8) == 0 || strncmp(name, "backward-", 9) == 0 || strncmp(name, "fill-", 5) == 0)
        test_memory(name);
    else if (strcmp(name, "success-decline") == 0) test_success_and_decline();
    else if (strcmp(name, "output-alias") == 0) test_output_alias();
    else if (strncmp(name, "ignore-", 7) == 0 || strncmp(name, "oneshot-", 8) == 0
        || strncmp(name, "custom-", 7) == 0 || strncmp(name, "simple-", 7) == 0) test_preflight(name);
    else if (strcmp(name, "default-flags") == 0) test_default_flags();
    else if (strncmp(name, "nonleaf-", 8) == 0) test_nonleaf(name);
    else if (strcmp(name, "default-segv") == 0 || strcmp(name, "default-bus") == 0) {
        prepare_default_install();
        if (failures) goto final_report;
        fatal_ready();
        if (failures) goto final_report;
        raise(strcmp(name, "default-segv") == 0 ? SIGSEGV : SIGBUS);
        CHECK(0, "default fatal signal must not return");
    } else { fprintf(stderr, "unknown case: %s\n", name); return 2; }
final_report:
    printf("{\"case\":\"%s\",\"pass\":%s,\"failures\":%d}\n", name, failures ? "false" : "true", failures);
    return failures ? 1 : 0;
}
