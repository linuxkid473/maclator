// ARM-only saved-frame and sigreturn oracle, never a throughput benchmark.
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/time.h>
#include <sys/ucontext.h>

#if !defined(__aarch64__) && !defined(__arm64__)
#error Requires ARM64 guest code
#endif

static volatile uint64_t loop_pc;
static volatile sig_atomic_t frames, modified, failures;

static void inspect_gpr(int sig, siginfo_t *info, void *context) {
    (void)sig; (void)info;
    ucontext_t *uc = context;
    if (!uc || !uc->uc_mcontext || uc->uc_mcontext->__ss.__pc != loop_pc) return;
    uint64_t count = uc->uc_mcontext->__ss.__x[19];
    uint64_t accumulator = uc->uc_mcontext->__ss.__x[21];
    uint64_t expected = count * 3 + (uint64_t)modified * 1024;
    ++frames;
    if (count > 4000000 || uc->uc_mcontext->__ss.__x[20] != 4000000 || accumulator != expected) ++failures;
    // This changed mapped GPR must be reloaded at the public entry after
    // sigreturn, rather than overwritten by a stale callee-saved host register.
    uc->uc_mcontext->__ss.__x[21] = accumulator + 1024;
    ++modified;
}

int main(void) {
    struct sigaction action = {0};
    action.sa_sigaction = inspect_gpr;
    action.sa_flags = SA_SIGINFO;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGALRM, &action, 0)) return 10;
    struct itimerval timer = {{0, 1000}, {0, 1000}};
    if (setitimer(ITIMER_REAL, &timer, 0)) return 11;
    const uint64_t limit = 4000000;
    uint64_t accumulator, count;
    __asm__ volatile(
        "mov x19, xzr\n"
        "mov x20, %[limit]\n"
        "mov x21, xzr\n"
        "adr x10, 1f\n"
        "str x10, [%[begin]]\n"
        "1:\n"
        "add x19, x19, #1\n"
        "add x21, x21, #3\n"
        "cmp x19, x20\n"
        "b.lt 1b\n"
        "mov %[accumulator], x21\n"
        "mov %[count], x19\n"
        : [accumulator] "=&r" (accumulator), [count] "=&r" (count)
        : [limit] "r" (limit), [begin] "r" (&loop_pc)
        : "x10", "x19", "x20", "x21", "cc", "memory");
    struct itimerval stopped = {0};
    if (setitimer(ITIMER_REAL, &stopped, 0)) return 12;
    uint64_t expected = limit * 3 + (uint64_t)modified * 1024;
    printf("frames=%d modified=%d failures=%d count=%llu actual=%llu expected=%llu\n",
        frames, modified, failures, (unsigned long long)count,
        (unsigned long long)accumulator, (unsigned long long)expected);
    return frames > 0 && modified == frames && !failures && count == limit && accumulator == expected ? 0 : 13;
}
