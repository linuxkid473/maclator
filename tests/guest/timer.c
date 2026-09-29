#include <dispatch/dispatch.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
static void fire(void *ctx) { printf("main-queue dispatch_after fired (%s)\n", (const char *)ctx); fflush(stdout); }
static void fire_exit(void *ctx) { printf("exiting\n"); fflush(stdout); exit(0); }
int main(void) {
    dispatch_after_f(dispatch_time(DISPATCH_TIME_NOW, 300 * NSEC_PER_MSEC), dispatch_get_global_queue(0, 0), "global", fire);
    dispatch_after_f(dispatch_time(DISPATCH_TIME_NOW, 600 * NSEC_PER_MSEC), dispatch_get_main_queue(), "main", fire);
    dispatch_after_f(dispatch_time(DISPATCH_TIME_NOW, 1500 * NSEC_PER_MSEC), dispatch_get_main_queue(), "main", fire_exit);
    dispatch_main();
}
