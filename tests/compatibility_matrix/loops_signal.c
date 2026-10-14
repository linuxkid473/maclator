// Identical source for x86/ARM. Unsigned overflow is intentional and defined.
#include <inttypes.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>

static volatile sig_atomic_t alarms;
static void on_alarm(int signal) { (void)signal; ++alarms; }

__attribute__((noinline)) static uint64_t mixed_loop(uint64_t n) {
    uint64_t x = 0x123456789abcdef0ULL, sum = 0;
    for (uint64_t i = 0; i < n; ++i) {
        x = (x ^ (x >> 17)) * 0x9e3779b97f4a7c15ULL + i;
        sum ^= x + (sum << 3);
    }
    return sum ^ x;
}
__attribute__((noinline)) static uint64_t finite_loops(uint64_t n) {
    uint64_t sum = 0;
    for (uint64_t i = 0; i < n; ++i) sum += i * 7 + (i ^ 0x55aa);
    for (uint64_t i = n; i != 0; --i) sum ^= (i << (i & 7)) + (i >> 3);
    uint64_t i = 0, x = 3;
    while (i++ < n) x = x * 31 + i;
    return sum ^ x;
}
int main(int argc, char **argv) {
    uint64_t iterations = argc > 2 ? strtoull(argv[2], NULL, 0) : 20000000;
    int signals = argc > 1 && !strcmp(argv[1], "signal");
    if (signals) {
        struct sigaction action = {0};
        action.sa_handler = on_alarm;
        action.sa_flags = SA_RESTART;
        sigemptyset(&action.sa_mask);
        if (sigaction(SIGALRM, &action, NULL)) return 2;
        struct itimerval timer = {.it_interval = {0, 10000}, .it_value = {0, 10000}};
        if (setitimer(ITIMER_REAL, &timer, NULL)) return 3;
    }
    uint64_t checksum = mixed_loop(iterations) ^ finite_loops(iterations);
    if (signals) {
        struct itimerval timer = {0};
        if (setitimer(ITIMER_REAL, &timer, NULL)) return 4;
    }
    printf("FINITE_LOOPS %s iterations=%" PRIu64 " checksum=%016" PRIx64 " alarms=%d\n",
           !signals || alarms > 0 ? "PASS" : "FAIL", iterations, checksum, (int)alarms);
    return signals && !alarms ? 1 : 0;
}
