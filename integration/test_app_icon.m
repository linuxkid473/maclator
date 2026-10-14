// Bounded by the external test runner. Pass: shim.dylib icon.icns [invalid].
#import <AppKit/AppKit.h>
#import <dispatch/dispatch.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void *shim;
static NSString *expected_path;
static BOOL expect_valid;
static int test_result = 1;

static NSData *icon_pixels(NSImage *image) {
    if (!image || ![image isValid]) return nil;
    NSBitmapImageRep *bitmap = [[NSBitmapImageRep alloc]
        initWithBitmapDataPlanes:NULL pixelsWide:128 pixelsHigh:128
        bitsPerSample:8 samplesPerPixel:4 hasAlpha:YES isPlanar:NO
        colorSpaceName:NSCalibratedRGBColorSpace bitmapFormat:0
        bytesPerRow:128 * 4 bitsPerPixel:32];
    memset(bitmap.bitmapData, 0, 128 * 128 * 4);
    [NSGraphicsContext saveGraphicsState];
    NSGraphicsContext.currentContext = [NSGraphicsContext graphicsContextWithBitmapImageRep:bitmap];
    [image drawInRect:NSMakeRect(0, 0, 128, 128) fromRect:NSZeroRect
           operation:NSCompositingOperationCopy fraction:1.0];
    [NSGraphicsContext restoreGraphicsState];
    return [NSData dataWithBytes:bitmap.bitmapData length:128 * 128 * 4];
}

@interface MaclatorIconTestDelegate : NSObject <NSApplicationDelegate>
@end
@implementation MaclatorIconTestDelegate
- (void)applicationDidFinishLaunching:(NSNotification *)notification {
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 100 * NSEC_PER_MSEC),
                   dispatch_get_main_queue(), ^{
        int (*applied)(void) = dlsym(shim, "mcl_application_icon_applied");
        int did_apply = applied ? applied() : -1;
        NSImage *actual = NSApp.applicationIconImage;
        NSImage *expected = [[NSImage alloc] initWithContentsOfFile:expected_path];
        BOOL same = actual && expected && [actual isValid] && [expected isValid] &&
            NSEqualSizes(actual.size, expected.size) &&
            [icon_pixels(actual) isEqualToData:icon_pixels(expected)];
        test_result = expect_valid ? !(did_apply == 1 && same) : !(did_apply == 0);
        printf("ICON_TEST applied=%d valid=%d size=%.0fx%.0f expected=%.0fx%.0f pixels_match=%d result=%s\n",
               did_apply, actual.isValid, actual.size.width, actual.size.height,
               expected.size.width, expected.size.height, same,
               test_result == 0 ? "PASS" : "FAIL");
        fflush(stdout);
        [NSApp stop:nil];
        // Wake the run loop so -run notices -stop and returns immediately.
        [NSApp postEvent:[NSEvent otherEventWithType:NSEventTypeApplicationDefined
            location:NSZeroPoint modifierFlags:0 timestamp:0 windowNumber:0
            context:nil subtype:0 data1:0 data2:0] atStart:YES];
    });
}
@end

int main(int argc, const char **argv) {
    @autoreleasepool {
        if (argc < 3) return 2;
        expected_path = [NSString stringWithUTF8String:argv[2]];
        expect_valid = argc < 4;
        setenv("MACLATOR_APP_ICON", argv[2], 1);
        shim = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
        if (!shim) { fprintf(stderr, "dlopen: %s\n", dlerror()); return 3; }
        printf("ICON_TEST constructor_created_application=%d\n", NSApp != nil);
        if (NSApp) return 4;
        MaclatorIconTestDelegate *delegate = [MaclatorIconTestDelegate new];
        NSApplication *app = [NSApplication sharedApplication];
        app.delegate = delegate;
        [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
        [app run];
        return test_result;
    }
}
