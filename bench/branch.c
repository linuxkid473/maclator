// Deterministic branch-heavy microbenchmark for CBZ/CBNZ and TBZ/TBNZ code generation.
// 37 is coprime to 512; each 512 iterations visits every pattern of the tested bits once.
#include <stdint.h>
#include <stdio.h>
#include <time.h>

extern uint64_t maclator_branch_loop(uint64_t count, uint64_t seed);

int main(void) {
    const uint64_t count = 1u << 26;
    struct timespec start, end, cpu_start, cpu_end;
    clock_gettime(CLOCK_MONOTONIC, &start);
    int cpu_ok = clock_gettime(CLOCK_THREAD_CPUTIME_ID, &cpu_start) == 0;
    uint64_t sum = maclator_branch_loop(count, 17);
    cpu_ok = cpu_ok && clock_gettime(CLOCK_THREAD_CPUTIME_ID, &cpu_end) == 0;
    clock_gettime(CLOCK_MONOTONIC, &end);
    double ms = (end.tv_sec - start.tv_sec) * 1e3 + (end.tv_nsec - start.tv_nsec) / 1e6;
    printf("branches %9.3f ms checksum=%llu expected=%llu\n", ms,
           (unsigned long long)sum, (unsigned long long)(count * 8));
    if (cpu_ok) {
        double cpu_ms = (cpu_end.tv_sec - cpu_start.tv_sec) * 1e3
                        + (cpu_end.tv_nsec - cpu_start.tv_nsec) / 1e6;
        printf("branch_cpu %9.3f ms\n", cpu_ms);
    }
    return sum != count * 8;
}
