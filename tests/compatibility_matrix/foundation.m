#import <Foundation/Foundation.h>
#include <stdatomic.h>
#include <stdio.h>

int main(void) {
    @autoreleasepool {
        __block int caught = 0;
        for (int i = 0; i < 100; ++i) {
            @try { [NSException raise:@"MaclatorProbe" format:@"%d", i]; }
            @catch (NSException *e) { if ([e.name isEqualToString:@"MaclatorProbe"]) ++caught; }
        }
        dispatch_group_t group = dispatch_group_create();
        dispatch_queue_t queue = dispatch_get_global_queue(QOS_CLASS_DEFAULT, 0);
        __block _Atomic(int) count = 0;
        for (int i = 0; i < 32; ++i) {
            dispatch_group_async(group, queue, ^{
                @autoreleasepool {
                    NSData *data = [NSJSONSerialization dataWithJSONObject:@{@"probe": @1}
                                                                   options:0 error:NULL];
                    if (data.length) atomic_fetch_add(&count, 1);
                }
            });
        }
        long deadline = dispatch_group_wait(group, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC));
        int result = atomic_load(&count);
        int ok = caught == 100 && result == 32 && !deadline;
        printf("FOUNDATION_EH_DISPATCH %s caught=%d jobs=%d deadline=%ld\n", ok ? "PASS" : "FAIL",
               caught, result, deadline);
        return ok ? 0 : 1;
    }
}
