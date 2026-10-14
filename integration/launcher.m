// Native LaunchServices entry point. The translated application remains intact.
#import <AppKit/AppKit.h>
#import <Carbon/Carbon.h>
#include <CommonCrypto/CommonDigest.h>
#include <fcntl.h>
#include <sys/file.h>
#include <unistd.h>

@interface MCLLauncher : NSObject <NSApplicationDelegate>
@property NSDictionary *configuration;
@property NSTask *task;
@property NSMutableArray<NSURL *> *pendingURLs;
@property BOOL started;
@property BOOL deliveryScheduled;
@end

@implementation MCLLauncher
- (instancetype)init {
    if ((self = [super init])) _pendingURLs = [NSMutableArray array];
    return self;
}
- (void)showError:(NSString *)message {
    NSAlert *alert = [NSAlert new];
    alert.messageText = @"Maclator";
    alert.informativeText = message;
    [alert runModal];
    [NSApp terminate:nil];
}
- (void)applicationDidFinishLaunching:(NSNotification *)notification {
    self.configuration = [NSDictionary dictionaryWithContentsOfURL:
        [[NSBundle mainBundle] URLForResource:@"MaclatorLaunch" withExtension:@"plist"]];
    // Allow the initial open-document event to arrive before creating the guest.
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 200 * NSEC_PER_MSEC),
                   dispatch_get_main_queue(), ^{ [self launchGuest]; });
}
- (void)launchGuest {
    if (self.started) return;
    self.started = YES;
    NSString *runtime = self.configuration[@"Runtime"];
    NSString *target = self.configuration[@"Target"];
    if (!runtime || !target || ![[NSFileManager defaultManager] isExecutableFileAtPath:runtime]
        || ![[NSFileManager defaultManager] fileExistsAtPath:target]) {
        [self showError:@"启动文件已移动或不存在。请等待后台重新扫描，或运行 maclator-apps scan。"];
        return;
    }
    NSMutableArray *arguments = [NSMutableArray arrayWithArray:self.configuration[@"Arguments"] ?: @[]];
    [arguments addObject:target];
    [arguments addObjectsFromArray:self.configuration[@"ApplicationArguments"] ?: @[]];
    NSArray *commandArguments = [[NSProcessInfo processInfo] arguments];
    for (NSUInteger i = 1; i < commandArguments.count; i++) {
        NSString *arg = commandArguments[i];
        if (![arg hasPrefix:@"-psn_"]) [arguments addObject:arg];
    }
    self.task = [NSTask new];
    self.task.executableURL = [NSURL fileURLWithPath:runtime];
    self.task.arguments = arguments;
    NSMutableDictionary *environment = [[[NSProcessInfo processInfo] environment] mutableCopy];
    [environment addEntriesFromDictionary:self.configuration[@"Environment"] ?: @{}];
    environment[@"MACLATOR_SOURCE_APP"] = target;
    self.task.environment = environment;
    self.task.currentDirectoryURL = [NSURL fileURLWithPath:NSHomeDirectory()];
    NSString *logPath = self.configuration[@"Log"];
    if (logPath) {
        [[NSFileManager defaultManager] createDirectoryAtPath:[logPath stringByDeletingLastPathComponent]
                                withIntermediateDirectories:YES attributes:nil error:nil];
        if (![[NSFileManager defaultManager] fileExistsAtPath:logPath])
            [[NSFileManager defaultManager] createFileAtPath:logPath contents:nil attributes:nil];
        NSFileHandle *log = [NSFileHandle fileHandleForWritingAtPath:logPath];
        [log seekToEndOfFile];
        self.task.standardOutput = log;
        self.task.standardError = log;
    }
    self.task.standardInput = [NSFileHandle fileHandleWithNullDevice];
    __weak MCLLauncher *weakSelf = self;
    self.task.terminationHandler = ^(NSTask *finished) {
        dispatch_async(dispatch_get_main_queue(), ^{
            MCLLauncher *launcher = weakSelf;
            if (finished.terminationStatus != 0)
                [launcher showError:[NSString stringWithFormat:@"应用已退出（状态 %d）。日志：%@",
                                    finished.terminationStatus, logPath ?: @"未设置"]];
            else [NSApp terminate:nil];
        });
    };
    NSError *error = nil;
    if (![self.task launchAndReturnError:&error]) { [self showError:error.localizedDescription]; return; }
    [self deliverPendingAfter:2 attempt:0];
}
- (OSStatus)sendURLs:(NSArray<NSURL *> *)urls {
    pid_t pid = self.task.processIdentifier;
    NSAppleEventDescriptor *address = [NSAppleEventDescriptor descriptorWithDescriptorType:typeKernelProcessID
                                        bytes:&pid length:sizeof(pid)];
    NSMutableArray *files = [NSMutableArray array];
    OSStatus status = noErr;
    for (NSURL *url in urls) {
        if (url.isFileURL) [files addObject:url];
        else {
            NSAppleEventDescriptor *event = [NSAppleEventDescriptor appleEventWithEventClass:kInternetEventClass
                eventID:kAEGetURL targetDescriptor:address returnID:kAutoGenerateReturnID transactionID:kAnyTransactionID];
            [event setParamDescriptor:[NSAppleEventDescriptor descriptorWithString:url.absoluteString]
                           forKeyword:keyDirectObject];
            status = AESendMessage(event.aeDesc, NULL, kAENoReply | kAENeverInteract, 60);
            if (status != noErr) return status;
        }
    }
    if (files.count) {
        NSAppleEventDescriptor *list = [NSAppleEventDescriptor listDescriptor];
        for (NSURL *url in files)
            [list insertDescriptor:[NSAppleEventDescriptor descriptorWithDescriptorType:typeFileURL
                         data:[url.absoluteString dataUsingEncoding:NSUTF8StringEncoding]] atIndex:0];
        NSAppleEventDescriptor *event = [NSAppleEventDescriptor appleEventWithEventClass:kCoreEventClass
            eventID:kAEOpenDocuments targetDescriptor:address returnID:kAutoGenerateReturnID transactionID:kAnyTransactionID];
        [event setParamDescriptor:list forKeyword:keyDirectObject];
        status = AESendMessage(event.aeDesc, NULL, kAENoReply | kAENeverInteract, 60);
    }
    return status;
}
- (void)deliverPendingAfter:(NSUInteger)seconds attempt:(NSUInteger)attempt {
    if (self.deliveryScheduled) return;
    self.deliveryScheduled = YES;
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, seconds * NSEC_PER_SEC), dispatch_get_main_queue(), ^{
        self.deliveryScheduled = NO;
        if (!self.task.running || !self.pendingURLs.count) return;
        OSStatus status = noErr;
        while (self.pendingURLs.count) {
            status = [self sendURLs:@[self.pendingURLs[0]]];
            if (status != noErr) break;
            [self.pendingURLs removeObjectAtIndex:0];
        }
        if (status != noErr && attempt < 5) [self deliverPendingAfter:2 attempt:attempt + 1];
        else if (status != noErr) {
            NSLog(@"Maclator: open-document delivery failed: %d", (int)status);
            NSAlert *alert = [NSAlert new];
            alert.messageText = @"Maclator 无法传递打开文件事件";
            alert.informativeText = @"请从应用内的“打开”菜单选择文件。";
            [alert runModal];
            [self.pendingURLs removeAllObjects];
        }
    });
}
- (void)application:(NSApplication *)application openFiles:(NSArray<NSString *> *)files {
    for (NSString *file in files) [self.pendingURLs addObject:[NSURL fileURLWithPath:file]];
    if (self.task.running) [self deliverPendingAfter:0 attempt:0];
    [application replyToOpenOrPrint:NSApplicationDelegateReplySuccess];
}
- (void)application:(NSApplication *)application openURLs:(NSArray<NSURL *> *)urls {
    [self.pendingURLs addObjectsFromArray:urls];
    if (self.task.running) [self deliverPendingAfter:0 attempt:0];
}
- (BOOL)applicationShouldHandleReopen:(NSApplication *)application hasVisibleWindows:(BOOL)visible {
    if (self.task.running)
        [[NSRunningApplication runningApplicationWithProcessIdentifier:self.task.processIdentifier]
         activateWithOptions:0];
    return NO;
}
- (NSApplicationTerminateReply)applicationShouldTerminate:(NSApplication *)application {
    if (self.task.running) [self.task terminate];
    return NSTerminateNow;
}
@end

int main(int argc, const char **argv) {
    @autoreleasepool {
        // A stable per-entry lock prevents the scanner from replacing our
        // bundle while it is starting or supervising a translated app.
        NSString *bundle = [[[NSBundle mainBundle] bundlePath] stringByResolvingSymlinksInPath];
        NSData *pathData = [bundle dataUsingEncoding:NSUTF8StringEncoding];
        unsigned char digest[CC_SHA256_DIGEST_LENGTH];
        CC_SHA256(pathData.bytes, (CC_LONG)pathData.length, digest);
        NSMutableString *key = [NSMutableString string];
        for (NSUInteger i = 0; i < sizeof(digest); i++) [key appendFormat:@"%02x", digest[i]];
        NSString *locks = [NSHomeDirectory() stringByAppendingPathComponent:@"Library/Application Support/Maclator/Locks"];
        [[NSFileManager defaultManager] createDirectoryAtPath:locks withIntermediateDirectories:YES attributes:nil error:nil];
        NSString *lockPath = [locks stringByAppendingPathComponent:[key stringByAppendingString:@".lock"]];
        int lock = open(lockPath.fileSystemRepresentation, O_CREAT | O_RDWR, 0600);
        if (lock < 0 || flock(lock, LOCK_SH) != 0) return 1;
        NSApplication *application = [NSApplication sharedApplication];
        MCLLauncher *delegate = [MCLLauncher new];
        application.delegate = delegate;
        [application setActivationPolicy:NSApplicationActivationPolicyAccessory];
        [application run];
        close(lock);
    }
    return 0;
}
