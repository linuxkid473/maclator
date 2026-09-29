// Minimal AppKit calculator used to exercise Maclator with a real GUI app.
#import <Cocoa/Cocoa.h>

@interface Calc : NSObject <NSApplicationDelegate>
@property (strong) NSWindow *win;
@property (strong) NSTextField *display;
@property double acc;
@property (copy) NSString *op;
@property BOOL fresh;
- (void)press:(NSString *)k;
@end

@implementation Calc
- (double)apply:(double)b {
    if ([_op isEqualToString:@"+"]) return _acc + b;
    if ([_op isEqualToString:@"-"]) return _acc - b;
    if ([_op isEqualToString:@"×"]) return _acc * b;
    if ([_op isEqualToString:@"÷"]) return b == 0 ? 0 : _acc / b;
    return b;
}
- (void)press:(NSString *)k {
    NSString *digits = @"0123456789.";
    if (k.length == 1 && [digits containsString:k]) {
        NSString *cur = _fresh ? @"" : _display.stringValue;
        if ([cur isEqualToString:@"0"] && ![k isEqualToString:@"."]) cur = @"";
        _display.stringValue = [cur stringByAppendingString:k];
        _fresh = NO;
    } else if ([k isEqualToString:@"C"]) {
        _acc = 0; _op = nil; _display.stringValue = @"0"; _fresh = YES;
    } else {
        double v = _display.doubleValue;
        _acc = _op ? [self apply:v] : v;
        _display.stringValue = [NSString stringWithFormat:@"%g", _acc];
        _op = [k isEqualToString:@"="] ? nil : k;
        _fresh = YES;
    }
}
- (void)clicked:(NSButton *)b { [self press:b.title]; }
- (void)applicationDidFinishLaunching:(NSNotification *)n {
    _win = [[NSWindow alloc] initWithContentRect:NSMakeRect(200, 200, 260, 330)
        styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable backing:NSBackingStoreBuffered defer:NO];
    _win.title = @"Maclator Calc";
    NSView *v = _win.contentView;
    _display = [NSTextField labelWithString:@"0"];
    _display.frame = NSMakeRect(10, 270, 240, 44);
    _display.alignment = NSTextAlignmentRight;
    _display.font = [NSFont monospacedDigitSystemFontOfSize:32 weight:NSFontWeightLight];
    [v addSubview:_display];
    NSArray *rows = @[@[@"C", @"", @"", @"÷"], @[@"7", @"8", @"9", @"×"], @[@"4", @"5", @"6", @"-"],
                      @[@"1", @"2", @"3", @"+"], @[@"0", @".", @"", @"="]];
    for (int r = 0; r < 5; r++)
        for (int c = 0; c < 4; c++) {
            NSString *t = rows[r][c];
            if (!t.length) continue;
            NSButton *b = [NSButton buttonWithTitle:t target:self action:@selector(clicked:)];
            b.frame = NSMakeRect(10 + c * 60, 210 - r * 50, 55, 44);
            [v addSubview:b];
        }
    [_win makeKeyAndOrderFront:nil];
    [NSApp activateIgnoringOtherApps:YES];
    _fresh = YES;
    if (getenv("CALC_SNAPSHOT")) {
        // Press 7 x 6 =, let AppKit draw, then dump the window contents to a PNG.
        for (NSString *k in @[@"7", @"×", @"6", @"="]) [self press:k];
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(1.5 * NSEC_PER_SEC)), dispatch_get_main_queue(), ^{
            NSView *cv = self.win.contentView.superview ?: self.win.contentView;
            NSBitmapImageRep *rep = [cv bitmapImageRepForCachingDisplayInRect:cv.bounds];
            [cv cacheDisplayInRect:cv.bounds toBitmapImageRep:rep];
            NSData *png = [rep representationUsingType:NSBitmapImageFileTypePNG properties:@{}];
            [png writeToFile:@"/tmp/calc_snapshot.png" atomically:YES];
            printf("snapshot: display=%s png=%lu bytes\n", self.display.stringValue.UTF8String, (unsigned long)png.length);
            fflush(stdout);
            [NSApp terminate:nil];
        });
    }
    if (getenv("CALC_SELFTEST")) {
        for (NSString *k in @[@"2", @"+", @"3", @"×", @"4", @"="]) [self press:k];
        printf("selftest 2+3x4 (left-to-right) = %s\n", _display.stringValue.UTF8String);
        fflush(stdout);
        [self press:@"C"];
        for (NSString *k in @[@"1", @"2", @".", @"5", @"÷", @"5", @"="]) [self press:k];
        printf("selftest 12.5/5 = %s\n", _display.stringValue.UTF8String);
        fflush(stdout);
        [NSApp performSelector:@selector(terminate:) withObject:nil afterDelay:1.5];
    }
}
- (BOOL)applicationShouldTerminateAfterLastWindowClosed:(NSApplication *)a { return YES; }
@end

int main(void) {
    @autoreleasepool {
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyRegular];
        Calc *d = [Calc new];
        app.delegate = d;
        [app run];
    }
    return 0;
}
