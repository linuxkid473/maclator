// SOURCE ONLY candidate probe: original commandBuffer presentDrawable route.
// Tests the separate opt-in bridge; does not present guest drawables directly.
// Keeps the v1 source unchanged; does not synthesize presentation or callbacks.
// Each invocation creates and closes only its own finite-duration test window.
// GPU command completion and IOSurface pixels do not establish onscreen presentation.
#import <AppKit/AppKit.h>
#import <Metal/Metal.h>
#import <MetalKit/MetalKit.h>
#import <QuartzCore/QuartzCore.h>
#import <IOSurface/IOSurface.h>
#import <objc/runtime.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static _Atomic unsigned presentedCallbacks;
static NSUInteger completedFrames;
static NSUInteger verifiedSurfacePixels;
static const size_t side = 256;

static void stage(const char *message) {
    printf("STAGE: %s\n", message);
    fflush(stdout);
}

static void pumpFor(NSTimeInterval seconds) {
    NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:seconds];
    while (deadline.timeIntervalSinceNow > 0) {
        @autoreleasepool {
            NSEvent *event = [NSApp nextEventMatchingMask:NSEventMaskAny
                                               untilDate:[NSDate dateWithTimeIntervalSinceNow:0.02]
                                                  inMode:NSDefaultRunLoopMode dequeue:YES];
            if (event) [NSApp sendEvent:event];
            [NSApp updateWindows];
            [[NSRunLoop currentRunLoop] runMode:NSDefaultRunLoopMode
                                    beforeDate:[NSDate dateWithTimeIntervalSinceNow:0.002]];
        }
    }
}

static MTLClearColor colorFor(NSUInteger frame) {
    return MTLClearColorMake(frame == 0, frame == 1, frame == 2, 1.0);
}

static BOOL clearFrame(id<MTLCommandQueue> queue, id<MTLTexture> texture,
                       id<CAMetalDrawable> drawable, NSUInteger frame,
                       BOOL synchronizeForReadback, BOOL localPresent) {
    if (!texture) { stage("FAIL: nil target texture"); return NO; }
    MTLRenderPassDescriptor *pass = [MTLRenderPassDescriptor renderPassDescriptor];
    pass.colorAttachments[0].texture = texture;
    pass.colorAttachments[0].loadAction = MTLLoadActionClear;
    pass.colorAttachments[0].storeAction = MTLStoreActionStore;
    pass.colorAttachments[0].clearColor = colorFor(frame);
    id<MTLCommandBuffer> command = [queue commandBuffer];
    if (!command) { stage("FAIL: nil command buffer"); return NO; }
    id<MTLRenderCommandEncoder> encoder = [command renderCommandEncoderWithDescriptor:pass];
    if (!encoder) { stage("FAIL: nil render encoder"); return NO; }
    [encoder endEncoding];
    if (synchronizeForReadback) {
        id<MTLBlitCommandEncoder> blit = [command blitCommandEncoder];
        if (!blit) { stage("FAIL: nil synchronize encoder"); return NO; }
        [blit synchronizeResource:texture];
        [blit endEncoding];
    }
    if (drawable) {
        id drawableTexture = drawable.texture;
        id drawableLayer = drawable.layer;
        id layerDevice = [(CAMetalLayer *)drawableLayer device];
        id textureDevice = [(id<MTLTexture>)drawableTexture device];
        printf("DRAWABLE_META: frame=%lu main_thread=%d drawable=%s texture=%s layer=%s layer_device=%s texture_device=%s\n",
               (unsigned long)frame, [NSThread isMainThread], object_getClassName(drawable),
               object_getClassName(drawableTexture), object_getClassName(drawableLayer),
               object_getClassName(layerDevice), object_getClassName(textureDevice));
        fflush(stdout);
        [drawable addPresentedHandler:^(id<MTLDrawable> value) {
            unsigned count = atomic_fetch_add(&presentedCallbacks, 1) + 1;
            printf("REAL_PRESENTED_CALLBACK: count=%u drawable=%s main_thread=%d presented_time=%.9f\n",
                   count, object_getClassName(value), [NSThread isMainThread], value.presentedTime);
            fflush(stdout);
        }];
        if (!localPresent) {
            stage("calling commandBuffer presentDrawable");
            [command presentDrawable:drawable];
        }
    }
    stage("calling commandBuffer commit/waitUntilCompleted");
    [command commit];
    [command waitUntilCompleted];
    if (command.status != MTLCommandBufferStatusCompleted) {
        fprintf(stderr, "GPU command failed: %s\n", command.error.description.UTF8String);
        return NO;
    }
    completedFrames++;
    printf("GPU_COMPLETE: frame=%lu\n", (unsigned long)frame);
    fflush(stdout);
    if (drawable && localPresent) {
        if (![NSThread isMainThread]) { stage("FAIL: local present requires guest main thread"); return NO; }
        stage("calling guest drawable present on main thread after GPU completion");
        [drawable present];
        printf("LOCAL_PRESENT_RETURNED: frame=%lu main_thread=1\n", (unsigned long)frame);
        fflush(stdout);
    }
    return YES;
}

static BOOL verifySurface(IOSurfaceRef surface, NSUInteger frame) {
    if (IOSurfaceLock(surface, kIOSurfaceLockReadOnly, NULL) != kIOReturnSuccess) return NO;
    const uint8_t *base = IOSurfaceGetBaseAddress(surface);
    const size_t stride = IOSurfaceGetBytesPerRow(surface);
    BOOL valid = base != NULL && stride >= side * 4;
    size_t checked = 0;
    for (size_t y = 0; valid && y < side; y++) {
        for (size_t x = 0; valid && x < side; x++) {
            const uint8_t *p = base + y * stride + 4 * x;
            // BGRA byte order; color is generated only by the GPU render-pass clear.
            valid = p[0] == (frame == 2 ? 255 : 0) &&
                    p[1] == (frame == 1 ? 255 : 0) &&
                    p[2] == (frame == 0 ? 255 : 0) && p[3] == 255;
            checked += valid;
        }
    }
    IOSurfaceUnlock(surface, kIOSurfaceLockReadOnly, NULL);
    if (valid) verifiedSurfacePixels += checked;
    printf("SURFACE_PIXELS: frame=%lu checked=%zu valid=%d\n",
           (unsigned long)frame, checked, valid);
    fflush(stdout);
    return valid;
}

static BOOL runIOSurface(NSView *view, id<MTLDevice> device,
                         id<MTLCommandQueue> queue, BOOL managed) {
    CALayer *layer = [CALayer layer];
    layer.frame = view.bounds;
    layer.contentsScale = view.window.backingScaleFactor;
    layer.contentsGravity = kCAGravityResize;
    view.wantsLayer = YES;
    view.layer = layer;
    // No frame-pool claim: fresh surfaces are all retained through window teardown.
    // A production pool needs CA/display consumption and drawable-lifetime tracking.
    NSMutableArray *surfaces = [NSMutableArray array];
    NSMutableArray *textures = [NSMutableArray array];
    BOOL passed = YES;
    for (NSUInteger frame = 0; passed && frame < 3; frame++) {
        @autoreleasepool {
            size_t row = IOSurfaceAlignProperty(kIOSurfaceBytesPerRow, side * 4);
            size_t allocation = IOSurfaceAlignProperty(kIOSurfaceAllocSize, row * side);
            NSDictionary *properties = @{
                (__bridge NSString *)kIOSurfaceWidth: @(side),
                (__bridge NSString *)kIOSurfaceHeight: @(side),
                (__bridge NSString *)kIOSurfaceBytesPerElement: @4,
                (__bridge NSString *)kIOSurfaceBytesPerRow: @(row),
                (__bridge NSString *)kIOSurfaceAllocSize: @(allocation),
                (__bridge NSString *)kIOSurfacePixelFormat: @((uint32_t)0x42475241)
            };
            stage("creating guest IOSurface");
            IOSurfaceRef surface = IOSurfaceCreate((__bridge CFDictionaryRef)properties);
            if (!surface) { passed = NO; break; }
            [surfaces addObject:(__bridge id)surface];
            CFRelease(surface); // the array holds a reference until after window teardown
            MTLTextureDescriptor *desc = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:
                                         MTLPixelFormatBGRA8Unorm width:side height:side mipmapped:NO];
            desc.usage = MTLTextureUsageRenderTarget;
            desc.storageMode = managed ? MTLStorageModeManaged : MTLStorageModeShared;
            stage("creating bridged host texture from guest IOSurface");
            id<MTLTexture> texture = [device newTextureWithDescriptor:desc iosurface:surface plane:0];
            if (!texture) { stage("FAIL: nil IOSurface texture"); passed = NO; break; }
            [textures addObject:texture];
            passed = clearFrame(queue, texture, nil, frame, managed, NO) && verifySurface(surface, frame);
            if (!passed) break;
            stage("attaching IOSurface to guest CALayer contents");
            [CATransaction begin];
            [CATransaction setDisableActions:YES];
            layer.contents = (__bridge id)surface;
            [CATransaction commit];
            [CATransaction flush];
            printf("CONTENTS_ATTACHED: frame=%lu surface_id=%u in_use=%d\n",
                   (unsigned long)frame, IOSurfaceGetID(surface), IOSurfaceIsInUse(surface));
            fflush(stdout);
            pumpFor(0.8);
        }
    }
    stage("hold final test frame (visual confirmation is separate)");
    pumpFor(2.0);
    [view.window orderOut:nil];
    layer.contents = nil;
    [CATransaction flush];
    pumpFor(0.1);
    // Strong locals keep arrays through the last flush; there is no re-use before exit.
    printf("RETAINED_RESOURCES: surfaces=%lu textures=%lu\n",
           (unsigned long)surfaces.count, (unsigned long)textures.count);
    return passed;
}

static BOOL runCAMetal(NSView *view, id<MTLDevice> device, id<MTLCommandQueue> queue, BOOL localPresent) {
    CAMetalLayer *layer = [CAMetalLayer layer];
    layer.frame = view.bounds;
    layer.drawableSize = CGSizeMake(side, side);
    layer.pixelFormat = MTLPixelFormatBGRA8Unorm;
    layer.framebufferOnly = YES;
    layer.maximumDrawableCount = 3;
    layer.allowsNextDrawableTimeout = YES;
    stage("assigning device to guest CAMetalLayer");
    layer.device = device;
    view.wantsLayer = YES;
    view.layer = layer;
    for (NSUInteger frame = 0; frame < 3; frame++) {
        @autoreleasepool {
            stage("calling CAMetalLayer nextDrawable");
            id<CAMetalDrawable> drawable = [layer nextDrawable];
            if (!drawable) { stage("FAIL: nil CAMetalDrawable"); return NO; }
            if (drawable.layer != layer) { stage("FAIL: wrong drawable layer"); return NO; }
            if (!clearFrame(queue, drawable.texture, drawable, frame, NO, localPresent)) return NO;
            pumpFor(0.8);
        }
    }
    pumpFor(2.0);
    return YES;
}

@interface ProbeDelegate : NSObject<MTKViewDelegate>
@property(strong) id<MTLCommandQueue> queue;
@property NSUInteger frame;
@property BOOL passed;
@property BOOL localPresent;
@end
@implementation ProbeDelegate
- (void)mtkView:(MTKView *)view drawableSizeWillChange:(CGSize)size { (void)view; (void)size; }
- (void)drawInMTKView:(MTKView *)view {
    stage("MTKView delegate requesting currentDrawable");
    id<CAMetalDrawable> drawable = view.currentDrawable;
    self.passed = drawable != nil && clearFrame(self.queue, drawable.texture, drawable, self.frame, NO, self.localPresent);
    self.frame++;
}
@end

static BOOL runMTKView(NSWindow *window, id<MTLDevice> device, id<MTLCommandQueue> queue, BOOL localPresent) {
    stage("creating guest MTKView with bridged device");
    MTKView *view = [[MTKView alloc] initWithFrame:window.contentView.bounds device:device];
    view.paused = YES;
    view.enableSetNeedsDisplay = NO;
    view.colorPixelFormat = MTLPixelFormatBGRA8Unorm;
    view.drawableSize = CGSizeMake(side, side);
    view.autoResizeDrawable = NO;
    view.framebufferOnly = YES;
    ProbeDelegate *delegate = [ProbeDelegate new];
    delegate.queue = queue;
    delegate.localPresent = localPresent;
    view.delegate = delegate;
    window.contentView = view;
    for (NSUInteger frame = 0; frame < 3; frame++) {
        @autoreleasepool {
            stage("calling MTKView draw");
            delegate.passed = NO;
            [view draw];
            if (!delegate.passed) { stage("FAIL: MTKView delegate frame"); return NO; }
            pumpFor(0.8);
        }
    }
    pumpFor(2.0);
    return YES;
}

int main(int argc, const char **argv) {
    @autoreleasepool {
        if (argc < 2 || (strcmp(argv[1], "iosurface") && strcmp(argv[1], "cametal") && strcmp(argv[1], "mtkview") && strcmp(argv[1], "cametal-local") && strcmp(argv[1], "mtkview-local"))) {
            fprintf(stderr, "Usage: %s iosurface|cametal|mtkview|cametal-local|mtkview-local [managed|shared]\n", argv[0]);
            return 2;
        }
        const char *mode = argv[1];
        BOOL localPresent = strstr(mode, "-local") != NULL;
        BOOL managed = argc < 3 || strcmp(argv[2], "shared") != 0;
        printf("PROBE: mode=%s storage=%s; screen output needs separate visual confirmation\n",
               mode, managed ? "managed" : "shared");
        fflush(stdout);
        NSWindow *window = nil;
        BOOL passed = NO;
        @try {
            stage("initializing guest AppKit");
            [NSApplication sharedApplication];
            [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
            [NSApp finishLaunching];
            window = [[NSWindow alloc] initWithContentRect:NSMakeRect(0, 0, 400, 300)
                                                styleMask:NSWindowStyleMaskTitled
                                                  backing:NSBackingStoreBuffered defer:NO];
            window.releasedWhenClosed = NO;
            window.title = [NSString stringWithFormat:@"Maclator GPU probe: %s (red → green → blue)", mode];
            [window center];
            [window makeKeyAndOrderFront:nil];
            [NSApp activateIgnoringOtherApps:YES];
            pumpFor(0.2);
            stage("creating Metal device and queue");
            id<MTLDevice> device = MTLCreateSystemDefaultDevice();
            id<MTLCommandQueue> queue = [device newCommandQueue];
            if (!device || !queue) { stage("FAIL: Metal device/queue"); }
            else if (!strcmp(mode, "iosurface")) passed = runIOSurface(window.contentView, device, queue, managed);
            else if (!strcmp(mode, "cametal") || !strcmp(mode, "cametal-local")) passed = runCAMetal(window.contentView, device, queue, localPresent);
            else passed = runMTKView(window, device, queue, localPresent);
        } @catch (NSException *exception) {
            fprintf(stderr, "EXCEPTION: %s: %s\n", exception.name.UTF8String, exception.reason.UTF8String);
        }
        [window orderOut:nil];
        [window close];
        printf("RESULT: api_gpu_checks=%s frames=%lu surface_pixels=%lu presented_callbacks=%u screen_verified=false\n",
               passed ? "PASS" : "FAIL", (unsigned long)completedFrames,
               (unsigned long)verifiedSurfacePixels, atomic_load(&presentedCallbacks));
        fflush(stdout);
        return passed ? 0 : 1;
    }
}
