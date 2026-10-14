// Opt-in guest-side application icon repair. Build for arm64, and inject only
// into a translated GUI application's initial process. The original .app is
// never edited. Foundation is the only linked framework: this library does not
// load AppKit, instantiate NSApplication, or run an event loop of its own.
#import <Foundation/Foundation.h>
#import <AppKit/AppKit.h>
#import <dispatch/dispatch.h>
#import <objc/runtime.h>
#include <stdlib.h>

static NSString *mcl_icon_path;
static id mcl_launch_observer;
static BOOL mcl_icon_applied;

static void mcl_apply_application_icon(id application) {
    // AppKit's icon setter must run on the application's main thread. Use the
    // existing application supplied by its launch notification, never create it.
    if (![NSThread isMainThread] || !application || !mcl_icon_path) return;
    Class imageClass = objc_getClass("NSImage");
    Class applicationClass = objc_getClass("NSApplication");
    if (!imageClass || !applicationClass ||
        ![application isKindOfClass:applicationClass] ||
        ![application respondsToSelector:@selector(setApplicationIconImage:)]) return;

    @autoreleasepool {
        // Read the original resource directly. NSWorkspace iconForFile: may
        // supply LaunchServices' architecture-prohibited overlay instead.
        NSImage *icon = [[imageClass alloc] initWithContentsOfFile:mcl_icon_path];
        if (!icon || ![icon isValid]) return;
        [(NSApplication *)application setApplicationIconImage:icon];
        mcl_icon_applied = YES;
    }
}

__attribute__((constructor)) static void mcl_install_application_icon(void) {
    @autoreleasepool {
        const char *path = getenv("MACLATOR_APP_ICON");
        if (!path || !*path) return;
        NSString *candidate = [[NSString alloc] initWithUTF8String:path];
        BOOL directory = NO;
        if (!candidate || ![candidate isAbsolutePath] ||
            ![[NSFileManager defaultManager] fileExistsAtPath:candidate isDirectory:&directory] ||
            directory) return;
        mcl_icon_path = candidate;

        // A string avoids an AppKit data-symbol dependency. Main executables
        // that do not load AppKit simply never emit this notification.
        mcl_launch_observer = [[NSNotificationCenter defaultCenter]
            addObserverForName:@"NSApplicationDidFinishLaunchingNotification"
            object:nil queue:nil usingBlock:^(NSNotification *notification) {
                id application = notification.object;
                void (^apply)(void) = ^{
                    mcl_apply_application_icon(application);
                    // AppKit's launch sequence or the app's delegate may set
                    // its default icon later in this same run-loop turn.
                    dispatch_async(dispatch_get_main_queue(), ^{
                        mcl_apply_application_icon(application);
                    });
                };
                if ([NSThread isMainThread]) apply();
                else dispatch_async(dispatch_get_main_queue(), apply);
            }];
    }
}

// Read-only diagnostic hook for the focused GUI regression program. It does
// not expose or change AppKit state, and is not needed by the launcher.
__attribute__((visibility("default"))) int mcl_application_icon_applied(void) {
    return mcl_icon_applied ? 1 : 0;
}
