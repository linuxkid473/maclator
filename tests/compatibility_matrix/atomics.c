#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

static _Atomic(uint64_t) counter;
static _Atomic(unsigned __int128) pair __attribute__((aligned(16)));
static atomic_int phase, pair_failures, publication_failures;
static uint64_t payload;
static unsigned iterations = 2000;
static atomic_int store_writers;
static void *store_worker(void *arg) {
    uint64_t id = (uint64_t)arg;
    for (unsigned i = 1; i <= iterations; ++i) {
        uint64_t value = (id << 48) | i;
        unsigned __int128 next = ((unsigned __int128)value << 64) | value;
        atomic_store_explicit(&pair, next, memory_order_release);
        unsigned __int128 read = atomic_load_explicit(&pair, memory_order_acquire);
        if ((uint64_t)read != (uint64_t)(read >> 64)) atomic_fetch_add(&pair_failures, 1);
    }
    atomic_fetch_add(&store_writers, 1);
    return NULL;
}
static void *count_worker(void *unused) {
    (void)unused;
    for (unsigned i = 0; i < iterations; ++i) {
        atomic_fetch_add_explicit(&counter, 1, memory_order_seq_cst);
        unsigned __int128 current = atomic_load_explicit(&pair, memory_order_relaxed);
        for (;;) {
            uint64_t lo = (uint64_t)current, hi = (uint64_t)(current >> 64);
            if (lo != hi) atomic_fetch_add(&pair_failures, 1);
            unsigned __int128 next = ((unsigned __int128)(hi + 1) << 64) | (lo + 1);
            if (atomic_compare_exchange_weak_explicit(&pair, &current, next,
                    memory_order_acq_rel, memory_order_relaxed)) break;
        }
    }
    return NULL;
}
static void *producer(void *unused) {
    (void)unused;
    for (int i = 1; i <= 2000; ++i) {
        while (atomic_load_explicit(&phase, memory_order_acquire) != 0) {}
        payload = (uint64_t)i * 0x12345;
        atomic_store_explicit(&phase, i, memory_order_release);
    }
    return NULL;
}
int main(int argc, char **argv) {
    if (argc > 1) iterations = (unsigned)strtoul(argv[1], NULL, 10);
    if (!iterations || iterations > 1000000) return 4;
    pthread_t threads[4];
    if (argc > 2) {
        for (unsigned i = 0; i < 4; ++i)
            if (pthread_create(&threads[i], NULL, store_worker, (void *)(uint64_t)(i + 1))) return 2;
        while (atomic_load(&store_writers) != 4) {
            unsigned __int128 read = atomic_load_explicit(&pair, memory_order_relaxed);
            if ((uint64_t)read != (uint64_t)(read >> 64)) atomic_fetch_add(&pair_failures, 1);
        }
        for (int i = 0; i < 4; ++i) pthread_join(threads[i], NULL);
        int errors = atomic_load(&pair_failures);
        printf("ATOMICS_STORE128_LOAD128 %s pair_failures=%d\n", errors ? "FAIL" : "PASS", errors);
        return errors ? 1 : 0;
    }
    for (int i = 0; i < 4; ++i) if (pthread_create(&threads[i], NULL, count_worker, NULL)) return 2;
    for (int i = 0; i < 4; ++i) pthread_join(threads[i], NULL);
    pthread_t pub;
    if (pthread_create(&pub, NULL, producer, NULL)) return 3;
    for (int i = 1; i <= 2000; ++i) {
        while (atomic_load_explicit(&phase, memory_order_acquire) != i) {}
        if (payload != (uint64_t)i * 0x12345) atomic_fetch_add(&publication_failures, 1);
        atomic_store_explicit(&phase, 0, memory_order_release);
    }
    pthread_join(pub, NULL);
    unsigned __int128 final = atomic_load(&pair);
    uint64_t expected = (uint64_t)iterations * 4;
    int ok = atomic_load(&counter) == expected && (uint64_t)final == expected &&
             (uint64_t)(final >> 64) == expected && !atomic_load(&pair_failures) && !atomic_load(&publication_failures);
    printf("ATOMICS_COUNTER_CAS128_PUBLICATION %s counter=%llu pair=%llu/%llu pair_failures=%d publication_failures=%d\n",
           ok ? "PASS" : "FAIL", (unsigned long long)atomic_load(&counter),
           (unsigned long long)(final >> 64), (unsigned long long)final,
           atomic_load(&pair_failures), atomic_load(&publication_failures));
    return ok ? 0 : 1;
}
