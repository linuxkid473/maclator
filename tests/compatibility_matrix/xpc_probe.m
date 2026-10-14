// Self-owned in-bundle NSXPC echo probe. It requires no privileged service.
#import <Foundation/Foundation.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static void probe_log(const char *stage) {
#ifdef MCL_XPC_LOG
    FILE *log = fopen(MCL_XPC_LOG, "a");
    if (log) {
        fprintf(log, "%s pid=%d xpc_service_name=%d xpc_flags=%d cf_bundle_id=%d\n", stage,
                getpid(), getenv("XPC_SERVICE_NAME") != NULL, getenv("XPC_FLAGS") != NULL,
                getenv("__CFBundleIdentifier") != NULL);
        fclose(log);
    }
#else
    (void)stage;
#endif
}

@protocol MaclatorEcho
- (void)echo:(NSString *)value reply:(void (^)(NSString *))reply;
@end

#if defined(MCL_XPC_SERVICE)
@interface MaclatorEchoService : NSObject <MaclatorEcho, NSXPCListenerDelegate>
@end
@implementation MaclatorEchoService
- (void)echo:(NSString *)value reply:(void (^)(NSString *))reply {
    probe_log("guest-service-echo");
    reply(value);
}
- (BOOL)listener:(NSXPCListener *)listener shouldAcceptNewConnection:(NSXPCConnection *)connection {
    connection.exportedInterface = [NSXPCInterface interfaceWithProtocol:@protocol(MaclatorEcho)];
    connection.exportedObject = self;
    [connection resume];
    return YES;
}
@end
int main(void) {
    probe_log("guest-service-entry");
    @autoreleasepool {
        MaclatorEchoService *delegate = [MaclatorEchoService new];
        NSXPCListener *listener = [NSXPCListener serviceListener];
        listener.delegate = delegate;
        probe_log("guest-service-resume");
        [listener resume];
    }
    return 0;
}
#else
int main(int argc, char **argv) {
    @autoreleasepool {
        if (argc != 2) return 2;
        __block _Atomic(int) result = 0;
        NSXPCConnection *connection = [[NSXPCConnection alloc] initWithServiceName:@(argv[1])];
        connection.remoteObjectInterface = [NSXPCInterface interfaceWithProtocol:@protocol(MaclatorEcho)];
        [connection resume];
        id<MaclatorEcho> service = [connection remoteObjectProxyWithErrorHandler:^(NSError *error) {
            fprintf(stderr, "XPC_ECHO error: %s\n", error.description.UTF8String);
            atomic_store(&result, -1);
        }];
        [service echo:@"maclator-xpc-echo" reply:^(NSString *value) {
            atomic_store(&result, [value isEqualToString:@"maclator-xpc-echo"] ? 1 : -2);
        }];
        NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:5];
        while (!atomic_load(&result) && [deadline timeIntervalSinceNow] > 0)
            [[NSRunLoop currentRunLoop] runUntilDate:[NSDate dateWithTimeIntervalSinceNow:0.02]];
        int status = atomic_load(&result);
        [connection invalidate];
        printf("XPC_ECHO %s result=%d\n", status == 1 ? "PASS" : "FAIL", status);
        return status == 1 ? 0 : 1;
    }
}
#endif
