// White-box installer fault injection. ONLY the sigaction call site is wrapped;
// all non-injected queries/writes still use real Darwin libc/kernel actions.
// This is not evidence that the kernel naturally failed a valid installation.
#include "native_fault.h"
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static int probe_sigaction(int, const struct sigaction *, struct sigaction *);
#define sigaction(signo, action, old) probe_sigaction((signo), (action), (old))
#include "../native-fault/native_fault.c"
#undef sigaction

static int failures, query_calls, write_calls, fail_query_at, fail_write_at, fail_rollback;
static int events[8], event_count;
#define CHECK(condition, message) do { \
    if (!(condition)) { fprintf(stderr, "FAIL: %s (%s:%d)\n", message, __FILE__, __LINE__); ++failures; } \
} while (0)

static int probe_sigaction(int signo, const struct sigaction *action, struct sigaction *old) {
    const int ordinal = action ? ++write_calls : ++query_calls;
    CHECK(event_count < 8, "bounded installer call log");
    if (event_count < 8) events[event_count++] = action ? signo : -signo;
    if ((!action && ordinal == fail_query_at) || (action && ordinal == fail_write_at)) {
        errno = EIO;
        return -1;
    }
    if (action && ordinal == 3 && fail_rollback) { errno = EBUSY; return -1; }
    return sigaction(signo, action, old);
}
static void callback(int signo) { (void)signo; }
static int same_action(const struct sigaction *a, const struct sigaction *b) {
    return a->sa_handler == b->sa_handler && a->sa_flags == b->sa_flags && a->sa_mask == b->sa_mask;
}
static void init_default_actions(int hidden_reset, struct sigaction before[2]) {
    struct sigaction action = {0};
    action.sa_handler = SIG_DFL;
    action.sa_flags = SA_ONSTACK | SA_RESTART | SA_NODEFER | (hidden_reset ? SA_RESETHAND : 0);
    sigaddset(&action.sa_mask, SIGUSR1);
    CHECK(sigaction(SIGSEGV, &action, NULL) == 0, "set visible noncanonical SEGV default");
    sigemptyset(&action.sa_mask);
    sigaddset(&action.sa_mask, SIGUSR2);
    CHECK(sigaction(SIGBUS, &action, NULL) == 0, "set visible noncanonical BUS default");
    CHECK(sigaction(SIGSEGV, NULL, &before[0]) == 0 && sigaction(SIGBUS, NULL, &before[1]) == 0,
          "snapshot query-visible defaults");
    CHECK(before[0].sa_handler == SIG_DFL && before[1].sa_handler == SIG_DFL, "default snapshots");
}
static void check_no_guest_write(void) {
    unsigned char bytes[32], before[32];
    memset(bytes, 0x41, sizeof(bytes));
    memcpy(before, bytes, sizeof(bytes));
    struct mcl_nf_result result;
    errno = EDOM;
    CHECK(mcl_nf_call(MCL_NF_FILL, (uintptr_t)bytes, 0, sizeof(bytes), 7, &result) == MCL_NF_DECLINED,
          "failed install cannot enable primitive dispatch");
    CHECK(errno == EDOM && memcmp(bytes, before, sizeof(bytes)) == 0, "failed-install decline preserves guest bytes/errno");
}
static void test_install(const char *name) {
    struct sigaction before[2], after[2];
    const int hidden = strcmp(name, "rollback-hidden-default") == 0;
    init_default_actions(hidden, before);
    const int reject = strncmp(name, "reject-", 7) == 0;
    if (reject) {
        struct sigaction unsupported = {0};
        const int target = strstr(name, "-bus") ? SIGBUS : SIGSEGV;
        unsupported.sa_handler = strstr(name, "ignore") ? SIG_IGN : callback;
        unsupported.sa_flags = strstr(name, "oneshot") ? SA_RESETHAND : 0;
        sigaddset(&unsupported.sa_mask, SIGALRM);
        CHECK(sigaction(target, &unsupported, NULL) == 0, "set unsupported opposite-side fixture");
        CHECK(sigaction(SIGSEGV, NULL, &before[0]) == 0 && sigaction(SIGBUS, NULL, &before[1]) == 0,
              "snapshot unsupported actions");
    }
    if (strcmp(name, "query-first-fails") == 0) fail_query_at = 1;
    if (strcmp(name, "query-second-fails") == 0) fail_query_at = 2;
    if (strcmp(name, "write-first-fails") == 0) fail_write_at = 1;
    if (strcmp(name, "write-second-fails") == 0 || hidden || strcmp(name, "rollback-fails") == 0) fail_write_at = 2;
    if (strcmp(name, "rollback-fails") == 0) fail_rollback = 1;
    const int success = strcmp(name, "default-success") == 0;
    struct mcl_nf_install_report report = {99,99,99,99};
    const int status = mcl_nf_install_with_report(&report);
    CHECK(status == (success ? 0 : reject ? ENOTSUP : EIO), "installer returns original error");
    CHECK(report.primary_error == status, "report matches primary error");
    CHECK(sigaction(SIGSEGV, NULL, &after[0]) == 0 && sigaction(SIGBUS, NULL, &after[1]) == 0,
          "real kernel query after injected installer path");
    if (fail_query_at) {
        CHECK(query_calls == fail_query_at && write_calls == 0, "query failure performs no writes");
    } else {
        CHECK(query_calls == 2 && events[0] == -SIGSEGV && events[1] == -SIGBUS,
              "both preflight queries precede first write/rejection");
    }
    if (success) {
        sigset_t expected = 0;
        sigaddset(&expected, SIGSEGV); sigaddset(&expected, SIGBUS);
        CHECK(write_calls == 2 && atomic_load(&installed) == 1, "two successful dispatcher writes enable calls");
        CHECK(report.changed_mask == 3 && report.restored_mask == 0 && report.rollback_error == 0,
              "success report");
        CHECK(after[0].sa_sigaction == mcl_nf_signal_entry && after[1].sa_sigaction == mcl_nf_signal_entry
            && after[0].sa_flags == SA_SIGINFO && after[1].sa_flags == SA_SIGINFO
            && after[0].sa_mask == expected && after[1].sa_mask == expected, "canonical installed flags/masks");
        const struct mcl_nf_install_report first_report = report;
        const int first_queries = query_calls, first_writes = write_calls;
        query_calls = write_calls = event_count = 0;
        CHECK(mcl_nf_install_with_report(&report) == 0 && query_calls == 0 && write_calls == 0,
              "idempotent install uses no sigaction calls");
        CHECK(report.primary_error == 0 && report.rollback_error == 0
            && report.changed_mask == 0 && report.restored_mask == 0, "idempotent report initialized");
        printf("{\"case\":\"%s\",\"phase\":\"idempotent\",\"queries\":%d,\"writes_attempted\":%d,"
               "\"primary_error\":%d,\"rollback_error\":%d,\"changed_mask\":%u,\"restored_mask\":%u,"
               "\"pass\":%s,\"failures\":%d}\n",
               name, query_calls, write_calls, report.primary_error, report.rollback_error,
               report.changed_mask, report.restored_mask, failures ? "false" : "true", failures);
        // The final case record retains the first attempt, while the separate
        // record above observes the idempotent call. Do not lose its 2 queries,
        // 2 writes and changed_mask=3 by reporting only the second call's zeros.
        report = first_report;
        query_calls = first_queries;
        write_calls = first_writes;
    } else {
        CHECK(atomic_load(&installed) == 0, "failed/rejected install never publishes readiness");
        check_no_guest_write();
        if (reject) CHECK(write_calls == 0, "unknown action rejection attempts zero writes");
        if (fail_write_at == 1) CHECK(write_calls == 1 && events[2] == SIGSEGV, "failed first write needs no rollback");
        if (fail_write_at == 2) CHECK(write_calls == 3 && events[2] == SIGSEGV && events[3] == SIGBUS
            && events[4] == SIGSEGV, "second write failure attempts SEGV rollback exactly once");
        CHECK(report.changed_mask == (fail_write_at == 2 ? MCL_NF_SIGNAL_SEGV : 0), "successful-write report mask");
        CHECK(report.restored_mask == (fail_write_at == 2 && !fail_rollback ? MCL_NF_SIGNAL_SEGV : 0),
              "successful-rollback report mask");
        CHECK(report.rollback_error == (fail_rollback ? EBUSY : 0), "rollback error separately exposed");
        if (fail_rollback) {
            CHECK(after[0].sa_sigaction == mcl_nf_signal_entry && same_action(&before[1], &after[1]),
                  "rollback failure exposes partial SEGV installation without changing BUS");
        } else CHECK(same_action(&before[0], &after[0]) && same_action(&before[1], &after[1]),
                      "failure/rejection restores both query-visible defaults or leaves unsupported actions unchanged");
    }
    printf("{\"case\":\"%s\",\"kind\":\"whitebox-installer-real-actions\",\"queries\":%d,\"writes_attempted\":%d,"
           "\"primary_error\":%d,\"rollback_error\":%d,\"changed_mask\":%u,\"restored_mask\":%u,"
           "\"hidden_raw_RESET_restoration_claimed\":false,\"pass\":%s,\"failures\":%d}\n",
           name, query_calls, write_calls, report.primary_error, report.rollback_error,
           report.changed_mask, report.restored_mask, failures ? "false" : "true", failures);
}
int main(int argc, char **argv) {
    if (argc != 2) return 2;
    const char *names[] = {"reject-custom-segv","reject-custom-bus","reject-ignore-segv","reject-ignore-bus",
        "reject-oneshot-segv","reject-oneshot-bus","query-first-fails","query-second-fails","write-first-fails",
        "write-second-fails","rollback-hidden-default","rollback-fails","default-success"};
    int found = 0;
    for (unsigned i=0; i < sizeof(names)/sizeof(names[0]); ++i) if (strcmp(names[i], argv[1]) == 0) found = 1;
    if (!found) return 2;
    test_install(argv[1]);
    return failures ? 1 : 0;
}
