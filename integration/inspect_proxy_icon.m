#import <AppKit/AppKit.h>
#include <stdio.h>
int main(int argc, const char **argv) {
    @autoreleasepool {
        if (argc != 3) return 2;
        NSImage *image = [[NSWorkspace sharedWorkspace] iconForFile:[NSString stringWithUTF8String:argv[1]]];
        image.size = NSMakeSize(256, 256);
        NSBitmapImageRep *bitmap = [[NSBitmapImageRep alloc] initWithBitmapDataPlanes:NULL pixelsWide:256 pixelsHigh:256
            bitsPerSample:8 samplesPerPixel:4 hasAlpha:YES isPlanar:NO colorSpaceName:NSCalibratedRGBColorSpace
            bitmapFormat:0 bytesPerRow:1024 bitsPerPixel:32];
        memset(bitmap.bitmapData, 0, 256 * 256 * 4);
        [NSGraphicsContext saveGraphicsState];
        NSGraphicsContext.currentContext = [NSGraphicsContext graphicsContextWithBitmapImageRep:bitmap];
        [image drawInRect:NSMakeRect(0, 0, 256, 256) fromRect:NSZeroRect operation:NSCompositingOperationCopy fraction:1];
        [NSGraphicsContext restoreGraphicsState];
        NSData *png = [bitmap representationUsingType:NSBitmapImageFileTypePNG properties:@{}];
        if (![png writeToFile:[NSString stringWithUTF8String:argv[2]] atomically:YES]) return 3;
        printf("LaunchServices icon valid=%d size=256x256\n", image.isValid);
    }
    return 0;
}
