#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>

static void *worker(void *argument) {
    return argument;
}

int main(void) {
    pthread_attr_t attr;
    struct sched_param requested = { .sched_priority = 0 };
    if (pthread_attr_init(&attr) ||
        pthread_attr_setinheritsched(&attr, PTHREAD_EXPLICIT_SCHED) ||
        pthread_attr_setschedpolicy(&attr, SCHED_OTHER) ||
        pthread_attr_setschedparam(&attr, &requested)) {
        return 1;
    }

    for (uintptr_t iteration = 1; iteration <= 16; ++iteration) {
        pthread_t thread;
        int status = pthread_create(&thread, &attr, worker, (void *)iteration);
        if (status) {
            fprintf(stderr, "pthread_create failed: %d\n", status);
            return 2;
        }
        // XNU must publish this port before returning to the creator.
        if (pthread_mach_thread_np(thread) == 0) {
            fprintf(stderr, "new pthread has no Mach port\n");
            return 3;
        }
        int policy = -1;
        struct sched_param actual = { .sched_priority = -1 };
        status = pthread_getschedparam(thread, &policy, &actual);
        if (status || policy != SCHED_OTHER || actual.sched_priority != 0) {
            fprintf(stderr, "scheduling changed: error=%d policy=%d priority=%d\n",
                    status, policy, actual.sched_priority);
            return 4;
        }
        void *result;
        status = pthread_join(thread, &result);
        if (status || result != (void *)iteration) {
            fprintf(stderr, "pthread_join failed: %d\n", status);
            return 5;
        }
    }
    pthread_attr_destroy(&attr);
    puts("PASS: thread Mach port, explicit scheduling, and join (16 threads)");
    return 0;
}
