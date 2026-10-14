// Controlled ARM code generation. No other process, user file, or application is touched.
#include <libkern/OSCacheControl.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>

static uint32_t *code;
static int (*volatile target)(void);
static atomic_int phase, parked, failure;
static int check(int want) {
    for (int i = 0; i < 1000; ++i) {
        int got = target();
        if (got != want) {
            atomic_store(&failure, 1);
            printf("JIT_REWRITE FAIL expected=%d actual=%d\n", want, got);
            return 0;
        }
    }
    return 1;
}
static void *worker(void *unused) {
    (void)unused;
    if (!check(1)) return NULL;
    for (int p = 1; p <= 4; ++p) {
        atomic_store_explicit(&parked, p, memory_order_release);
        // Intentionally no syscall or dispatcher escape while another guest thread invalidates.
        while (atomic_load_explicit(&phase, memory_order_acquire) < p) {}
        if (!check(p + 1)) return NULL;
    }
    atomic_store(&parked, 5);
    return NULL;
}
static void write_code(int value) {
    code[0] = 0x52800000u | ((uint32_t)value << 5); // mov w0, #value
    code[1] = 0xd65f03c0u; // ret
    sys_icache_invalidate(code, 8);
}
int main(int argc, char **argv) {
    code = mmap(NULL, 16384, PROT_READ | PROT_WRITE | PROT_EXEC,
                MAP_PRIVATE | MAP_ANON | MAP_JIT, -1, 0);
    if (code == MAP_FAILED) { perror("mmap"); return 2; }
    target = (int (*)(void))code;
    write_code(1);
    if (argc == 1 || strcmp(argv[1], "thread") != 0) {
        for (int i = 1; i <= 12; ++i) {
            write_code(i);
            if (!check(i)) return 1;
        }
        puts("JIT_REWRITE sequential PASS");
        return 0;
    }
    pthread_t thread;
    if (pthread_create(&thread, NULL, worker, NULL)) return 3;
    for (int p = 1; p <= 4; ++p) {
        while (atomic_load_explicit(&parked, memory_order_acquire) < p) {
            if (atomic_load(&failure)) { pthread_join(thread, NULL); return 1; }
            sched_yield();
        }
        write_code(p + 1);
        atomic_store_explicit(&phase, p, memory_order_release);
    }
    pthread_join(thread, NULL);
    int ok = !atomic_load(&failure) && atomic_load(&parked) == 5;
    printf("JIT_REWRITE cross-thread %s\n", ok ? "PASS" : "FAIL");
    return ok ? 0 : 1;
}
