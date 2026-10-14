// A short-lived ARM-only .app that records native-entry and document events.
#import <AppKit/AppKit.h>
#include <dlfcn.h>

static NSString *logPath;
static void record(NSString *line) {
    NSFileHandle *file = [NSFileHandle fileHandleForWritingAtPath:logPath];
    [file seekToEndOfFile];
    [file writeData:[[line stringByAppendingString:@"\n"] dataUsingEncoding:NSUTF8StringEncoding]];
    [file closeFile];
}

@interface TestLauncherGuest : NSObject <NSApplicationDelegate>
@end
@implementation TestLauncherGuest
- (void)applicationDidFinishLaunching:(NSNotification *)notification {
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 300 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{
        int (*applied)(void) = dlsym(RTLD_DEFAULT, "mcl_application_icon_applied");
        record([NSString stringWithFormat:@"ready icon=%d valid=%d", applied ? applied() : -1,
                                         NSApp.applicationIconImage.isValid]);
    });
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 25 * NSEC_PER_SEC), dispatch_get_main_queue(), ^{
        [NSApp terminate:nil];
    });
}
- (void)application:(NSApplication *)application openFiles:(NSArray<NSString *> *)files {
    for (NSString *file in files) record([@"file " stringByAppendingString:file]);
    [application replyToOpenOrPrint:NSApplicationDelegateReplySuccess];
}
- (void)application:(NSApplication *)application openURLs:(NSArray<NSURL *> *)urls {
    for (NSURL *url in urls)
        record([url.isFileURL ? @"file " : @"url " stringByAppendingString:url.isFileURL ? url.path : url.absoluteString]);
}
@end

int main(int argc, const char **argv) {
    @autoreleasepool {
        logPath = [[[NSProcessInfo processInfo] environment] objectForKey:@"MCL_TEST_LOG"];
        if (!logPath) return 2;
        [[NSFileManager defaultManager] createFileAtPath:logPath contents:nil attributes:nil];
        record(@"started arm64");
        TestLauncherGuest *delegate = [TestLauncherGuest new];
        NSApplication *application = [NSApplication sharedApplication];
        application.delegate = delegate;
        [application setActivationPolicy:NSApplicationActivationPolicyAccessory];
        [application run];
    }
    return 0;
}
