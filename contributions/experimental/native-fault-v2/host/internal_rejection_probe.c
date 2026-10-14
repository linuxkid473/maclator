// Deliberate white-box SOURCE inclusion. This executable is not a public-ABI
// probe and never modifies the independent v2 file. Artificial states are NOT
// asynchronous timing evidence. Keep the compile path/hash pinned in build-host.sh.
#include "../native-fault/native_fault.c"
#include <stdio.h>
#include <string.h>

extern uint64_t mcl_probe_read_flags(void);
extern int mcl_probe_call_and_flags(uint32_t, uintptr_t, uintptr_t, size_t, unsigned,
                                   struct mcl_nf_result *, uint64_t *);
static int failures;
#define CHECK(condition, message) do { \
    if (!(condition)) { fprintf(stderr, "FAIL: %s (%s:%d)\n", message, __FILE__, __LINE__); ++failures; } \
} while (0)

static sigset_t prepare(void) {
    sigset_t original = 0;
    sigaddset(&original, SIGUSR1);
    CHECK(pthread_sigmask(SIG_SETMASK, &original, NULL) == 0, "set baseline mask");
    struct sigaction action = {0};
    action.sa_handler = SIG_DFL;
    action.sa_flags = 0;
    sigemptyset(&action.sa_mask);
    CHECK(sigaction(SIGSEGV, &action, NULL) == 0, "set prior SEGV default");
    CHECK(sigaction(SIGBUS, &action, NULL) == 0, "set prior BUS default");
    CHECK(mcl_nf_install() == 0, "install v2 default-only prototype");
    mcl_nf_enter_thread();
    return original;
}

static void test_internal_target(const char *name) {
    const sigset_t original = prepare();
    unsigned char buffer[128], buffer_before[128];
    unsigned char context_before[sizeof(local_context)];
    unsigned char registry_before[sizeof(active_contexts)];
    struct mcl_nf_result result = {0};
    memset(buffer, 0x7b, sizeof(buffer));
    memcpy(buffer_before, buffer, sizeof(buffer));
    if (strcmp(name, "reentrant-preparing") == 0 || strcmp(name, "reentrant-armed") == 0)
        atomic_store_explicit(&local_context.armed, strcmp(name, "reentrant-preparing") == 0 ? 4 : 1, memory_order_release);
    memcpy(context_before, &local_context, sizeof(local_context));
    memcpy(registry_before, active_contexts, sizeof(active_contexts));
    uintptr_t destination = (uintptr_t)buffer;
    if (strcmp(name, "target-context") == 0) destination = (uintptr_t)&local_context;
    if (strcmp(name, "target-registry") == 0) destination = (uintptr_t)active_contexts;
    errno = EDOM;
    uint64_t flags = UINT64_MAX;
    const int status = mcl_probe_call_and_flags(MCL_NF_FILL, destination, 0, 64, 0x19, &result, &flags);
    CHECK(status == (strncmp(name, "reentrant-", 10) == 0 ? MCL_NF_REENTRANT : MCL_NF_DECLINED),
          "internal overlap/reentry status");
    CHECK(errno == EDOM, "internal rejection errno");
    CHECK(memcmp(buffer, buffer_before, sizeof(buffer)) == 0, "internal rejection guest bytes untouched");
    CHECK(memcmp(context_before, &local_context, sizeof(local_context)) == 0, "internal descriptor untouched");
    CHECK(memcmp(registry_before, active_contexts, sizeof(active_contexts)) == 0, "internal registry untouched");
    CHECK(!(flags & (UINT64_C(1) << 10)), "immediate internal rejection DF clear");
    atomic_store_explicit(&local_context.armed, 0, memory_order_release);
    CHECK(pthread_sigmask(SIG_SETMASK, &original, NULL) == 0, "restore baseline mask");
}

int main(int argc, char **argv) {
    if (argc != 2) { fprintf(stderr, "one case name required\n"); return 2; }
    if (strcmp(argv[1], "target-context") == 0 || strcmp(argv[1], "target-registry") == 0
        || strcmp(argv[1], "reentrant-preparing") == 0 || strcmp(argv[1], "reentrant-armed") == 0)
        test_internal_target(argv[1]);
    else { fprintf(stderr, "unknown internal case\n"); return 2; }
    printf("{\"case\":\"%s\",\"kind\":\"whitebox-state\",\"pass\":%s,\"failures\":%d}\n",
           argv[1], failures ? "false" : "true", failures);
    return failures ? 1 : 0;
}
