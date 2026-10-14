// Source-only guest fixture. Compile for arm64, run through the parent's
// bounded OFF/ON launcher; no native x86 throughput claim from this fixture.
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <sys/ucontext.h>

#if !defined(__aarch64__) && !defined(__arm64__)
#error This fixture requires an ARM64 guest binary.
#endif

static volatile uint64_t loop_pc;
static volatile sig_atomic_t frames, modified, failures;

static void inspect_vector(int sig, siginfo_t *info, void *context) {
    (void)sig; (void)info;
    ucontext_t *uc = context;
    if (!uc || !uc->uc_mcontext || uc->uc_mcontext->__ss.__pc != loop_pc) return;
    uint64_t lanes[2];
    memcpy(lanes, &uc->uc_mcontext->__ns.__v[16], sizeof(lanes));
    double accumulator;
    memcpy(&accumulator, lanes, sizeof(accumulator));
    double expected = (double)uc->uc_mcontext->__ss.__x[19] + (double)modified * 1024.0;
    ++frames;
    if (lanes[1] != 0 || accumulator != expected) ++failures;
    // Modifying the saved SIMD context must survive sigreturn and external
    // re-entry, rather than being overwritten by the old fixed XMM mapping.
    accumulator += 1024.0;
    memcpy(lanes, &accumulator, sizeof(accumulator));
    lanes[1] = 0;
    memcpy(&uc->uc_mcontext->__ns.__v[16], lanes, sizeof(lanes));
    ++modified;
}

int main(void) {
    struct sigaction action = {0};
    action.sa_sigaction = inspect_vector;
    action.sa_flags = SA_SIGINFO;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGALRM, &action, 0) != 0) return 10;
    struct itimerval timer = {{0, 1000}, {0, 1000}};
    if (setitimer(ITIMER_REAL, &timer, 0) != 0) return 11;
    const uint64_t limit = 4000000;
    uint64_t bits, count;
    __asm__ volatile(
        "mov x19, xzr\n"
        "mov x20, %[limit]\n"
        "fmov d16, xzr\n"
        "fmov d17, #1.0\n"
        "fmov d18, #1.0\n"
        "adr x10, 1f\n"
        "str x10, [%[begin]]\n"
        "1:\n"
        "fmadd d16, d17, d18, d16\n"
        "add x19, x19, #1\n"
        "cmp x19, x20\n"
        "b.lt 1b\n"
        "fmov %[bits], d16\n"
        "mov %[count], x19\n"
        : [bits] "=&r" (bits), [count] "=&r" (count)
        : [limit] "r" (limit), [begin] "r" (&loop_pc)
        : "x10", "x19", "x20", "v16", "v17", "v18", "cc", "memory");
    struct itimerval stopped = {0};
    if (setitimer(ITIMER_REAL, &stopped, 0) != 0) return 12;
    double actual;
    memcpy(&actual, &bits, sizeof(actual));
    const double expected = (double)limit + (double)modified * 1024.0;
    printf("frames=%d modified=%d failures=%d count=%llu actual=%.0f expected=%.0f\n",
        frames, modified, failures, (unsigned long long)count, actual, expected);
    return frames > 0 && modified == frames && failures == 0 && count == limit && actual == expected ? 0 : 13;
}
