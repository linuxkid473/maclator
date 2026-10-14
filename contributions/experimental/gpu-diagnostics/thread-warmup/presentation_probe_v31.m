// SOURCE ONLY v3.1: unchanged v3 pthread path, plus a post-install main-loop warmup.
// Hypothesis only: not a bridge implementation or a threading/performance guarantee.
// Keeps all v3 sources/results unchanged; does not synthesize presentation or callbacks.
// Each invocation creates and closes only its own finite-duration test window.
// GPU command completion and IOSurface pixels do not establish onscreen presentation.
#import <AppKit/AppKit.h>
#import <Metal/Metal.h>
#import <MetalKit/MetalKit.h>
#import <QuartzCore/QuartzCore.h>
#import <IOSurface/IOSurface.h>
#import <objc/runtime.h>
#include <pthread.h>
#include <errno.h>
#include <time.h>
#include <math.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static _Atomic unsigned presentedCallbacks;
static NSUInteger completedFrames;
static const NSUInteger requestedFrames = 12;
static BOOL noMainPumpCritical;
static _Atomic unsigned explicitMainPumpViolations;
static _Atomic unsigned workerPresentReturns;
static _Atomic unsigned positivePresentedTimes;
static _Atomic unsigned zeroPresentedTimes;
static _Atomic unsigned callbackMetadataErrors;
static unsigned callbacksBeforeDrain;
static NSUInteger verifiedSurfacePixels;
static const size_t side = 256;

static void stage(const char *message) {
    printf("STAGE: %s\n", message);
    fflush(stdout);
}

static void pumpFor(NSTimeInterval seconds) {
    if (noMainPumpCritical && [NSThread isMainThread]) {
        atomic_fetch_add(&explicitMainPumpViolations, 1);
        fprintf(stderr, "FAIL: explicit main run-loop pump during critical frame loop\n");
        return;
    }
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
    NSUInteger phase = frame % 3;
    return MTLClearColorMake(phase == 0, phase == 1, phase == 2, 1.0);
}


typedef struct {
    void *retainedDrawable;
    NSUInteger frame;
    pthread_mutex_t lock;
    pthread_cond_t condition;
    int done, okay;
} PresentJob;

static void *workerPresent(void *opaque) {
    PresentJob *job = opaque;
    @autoreleasepool {
        // Main transferred +1 ownership before pthread_create. This strong local
        // consumes it and lives through the actual present and autorelease drain.
        id<CAMetalDrawable> drawable = (__bridge_transfer id)job->retainedDrawable;
        job->retainedDrawable = NULL;
        BOOL okay = ![NSThread isMainThread];
        printf("WORKER_PRESENT_BEGIN: frame=%lu main_thread=%d pthread=%p drawable=%s\n",
               (unsigned long)job->frame, [NSThread isMainThread], (void *)pthread_self(), object_getClassName(drawable));
        fflush(stdout);
        if (okay) {
            @try {
                [drawable present]; // actual original guest method, no fake callback
                atomic_fetch_add(&workerPresentReturns, 1);
                printf("WORKER_PRESENT_RETURNED: frame=%lu main_thread=0\n", (unsigned long)job->frame);
                fflush(stdout);
            } @catch (NSException *exception) {
                okay = NO;
                fprintf(stderr, "WORKER_PRESENT_EXCEPTION: frame=%lu %s: %s\n", (unsigned long)job->frame, exception.name.UTF8String, exception.reason.UTF8String);
            }
        }
        pthread_mutex_lock(&job->lock);
        job->okay = okay;
        job->done = 1;
        pthread_cond_signal(&job->condition);
        pthread_mutex_unlock(&job->lock);
    }
    return NULL;
}

static BOOL presentOnWorker(id<CAMetalDrawable> drawable, NSUInteger frame) {
    PresentJob job = {0};
    job.frame = frame;
    int mutexResult = pthread_mutex_init(&job.lock, NULL);
    if (mutexResult) { fprintf(stderr, "FAIL: worker mutex init=%d\n", mutexResult); return NO; }
    int conditionResult = pthread_cond_init(&job.condition, NULL);
    if (conditionResult) { pthread_mutex_destroy(&job.lock); fprintf(stderr, "FAIL: worker condition init=%d\n", conditionResult); return NO; }
    job.retainedDrawable = (__bridge_retained void *)drawable;
    pthread_t thread;
    int creation = pthread_create(&thread, NULL, workerPresent, &job);
    if (creation) {
        id releasedDrawable = (__bridge_transfer id)job.retainedDrawable;
        (void)releasedDrawable;
        pthread_cond_destroy(&job.condition);
        pthread_mutex_destroy(&job.lock);
        fprintf(stderr, "FAIL: worker pthread_create=%d\n", creation);
        return NO;
    }
    struct timespec deadline;
    if (clock_gettime(CLOCK_REALTIME, &deadline)) {
        fprintf(stderr, "FAIL: worker deadline clock\n");
        fflush(NULL);
        _Exit(4); // only this owned probe; do not return while job remains in use
    }
    deadline.tv_sec += 5;
    pthread_mutex_lock(&job.lock);
    while (!job.done) {
        int waited = pthread_cond_timedwait(&job.condition, &job.lock, &deadline);
        if (waited) {
            fprintf(stderr, "WORKER_WAIT_FAILED: frame=%lu code=%d timeout=%d\n", (unsigned long)frame, waited, waited == ETIMEDOUT);
            fflush(NULL);
            _Exit(4); // stack job cannot outlive this function while thread runs
        }
    }
    BOOL okay = job.okay;
    pthread_mutex_unlock(&job.lock);
    // Parent imposes a separate <=30s process-group deadline, including joining
    // an unexpected framework/autorelease hang after the worker signals done.
    int joined = pthread_join(thread, NULL);
    pthread_cond_destroy(&job.condition);
    pthread_mutex_destroy(&job.lock);
    if (joined) { fprintf(stderr, "FAIL: worker pthread_join=%d\n", joined); return NO; }
    return okay;
}

// Single behavioral change from v3: let the newly installed layer/view process
// a bounded main-loop warmup before beginning the unchanged no-explicit-pump loop.
// This does not establish that Core Animation committed or displayed a frame.
static void postInstallWarmup(void) {
    printf("POST_INSTALL_WARMUP_BEGIN: seconds=0.2 main_thread=%d\n", [NSThread isMainThread]);
    fflush(stdout);
    pumpFor(0.2);
    printf("POST_INSTALL_WARMUP_END: main_thread=%d\n", [NSThread isMainThread]);
    fflush(stdout);
}

static void beginNoMainPump(void) {
    noMainPumpCritical = YES;
    printf("NO_MAIN_PUMP_BEGIN: requested_frames=%lu pool_count=3 main_thread=%d\n", (unsigned long)requestedFrames, [NSThread isMainThread]);
    fflush(stdout);
}

static void endNoMainPump(void) {
    noMainPumpCritical = NO;
    callbacksBeforeDrain = atomic_load(&presentedCallbacks);
    printf("NO_MAIN_PUMP_END: completed_frames=%lu worker_present_returns=%u callbacks_during_phase=%u explicit_pump_calls=%u\n",
           (unsigned long)completedFrames, atomic_load(&workerPresentReturns), callbacksBeforeDrain, atomic_load(&explicitMainPumpViolations));
    fflush(stdout);
    stage("critical loop finished; now draining callbacks with main run-loop pump");
    pumpFor(2.0);
}

static BOOL clearFrame(id<MTLCommandQueue> queue, id<MTLTexture> texture,
                       id<CAMetalDrawable> drawable, NSUInteger frame) {
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
    if (drawable) {
        id drawableTexture = drawable.texture;
        id drawableLayer = drawable.layer;
        id layerDevice = [(CAMetalLayer *)drawableLayer device];
        id textureDevice = [(id<MTLTexture>)drawableTexture device];
        printf("DRAWABLE_META: frame=%lu main_thread=%d drawable=%s texture=%s layer=%s layer_device=%s texture_device=%s texture_storage=%lu framebuffer_only=%d\n",
               (unsigned long)frame, [NSThread isMainThread], object_getClassName(drawable),
               object_getClassName(drawableTexture), object_getClassName(drawableLayer),
               object_getClassName(layerDevice), object_getClassName(textureDevice),
               (unsigned long)[(id<MTLTexture>)drawableTexture storageMode], [(id<MTLTexture>)drawableTexture isFramebufferOnly]);
        fflush(stdout);
        [drawable addPresentedHandler:^(id<MTLDrawable> value) {
            unsigned count = atomic_fetch_add(&presentedCallbacks, 1) + 1;
            @try {
                CFTimeInterval actualTime = value.presentedTime;
                if (isfinite(actualTime) && actualTime > 0) atomic_fetch_add(&positivePresentedTimes, 1);
                else if (actualTime == 0) atomic_fetch_add(&zeroPresentedTimes, 1);
                else atomic_fetch_add(&callbackMetadataErrors, 1);
                printf("REAL_PRESENTED_CALLBACK: count=%u frame=%lu drawable=%s main_thread=%d presented_time=%.9f\n",
                       count, (unsigned long)frame, object_getClassName(value), [NSThread isMainThread], actualTime);
                fflush(stdout);
            } @catch (NSException *exception) {
                atomic_fetch_add(&callbackMetadataErrors, 1);
                fprintf(stderr, "CALLBACK_METADATA_EXCEPTION: %s: %s\n", exception.name.UTF8String, exception.reason.UTF8String);
            }
        }];
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
    if (drawable) {
        if (![NSThread isMainThread]) { stage("FAIL: diagnostic submission loop requires guest main thread"); return NO; }
        stage("handoff actual drawable to independent guest pthread after GPU completion");
        if (!presentOnWorker(drawable, frame)) return NO;
    }
    return YES;
}

static BOOL runCAMetal(NSView *view, id<MTLDevice> device, id<MTLCommandQueue> queue) {
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
    BOOL passed = YES;
    postInstallWarmup();
    beginNoMainPump();
    for (NSUInteger frame = 0; passed && frame < requestedFrames; frame++) {
        @autoreleasepool {
            stage("calling CAMetalLayer nextDrawable (critical loop does not pump main runloop)");
            id<CAMetalDrawable> drawable = [layer nextDrawable];
            if (!drawable) { stage("FAIL: nil CAMetalDrawable"); passed = NO; break; }
            if (drawable.layer != layer) { stage("FAIL: wrong drawable layer"); passed = NO; break; }
            passed = clearFrame(queue, drawable.texture, drawable, frame);
        } // release every drawable before acquiring the next frame
    }
    endNoMainPump();
    return passed;
}

@interface ProbeDelegate : NSObject<MTKViewDelegate>
@property(strong) id<MTLCommandQueue> queue;
@property NSUInteger frame;
@property BOOL passed;
@end
@implementation ProbeDelegate
- (void)mtkView:(MTKView *)view drawableSizeWillChange:(CGSize)size { (void)view; (void)size; }
- (void)drawInMTKView:(MTKView *)view {
    stage("MTKView delegate requesting currentDrawable");
    id<CAMetalDrawable> drawable = view.currentDrawable;
    self.passed = drawable != nil && clearFrame(self.queue, drawable.texture, drawable, self.frame);
    self.frame++;
}
@end

static BOOL runMTKView(NSWindow *window, id<MTLDevice> device, id<MTLCommandQueue> queue) {
    stage("creating guest MTKView with bridged device");
    MTKView *view = [[MTKView alloc] initWithFrame:window.contentView.bounds device:device];
    view.paused = YES;
    view.enableSetNeedsDisplay = NO;
    view.colorPixelFormat = MTLPixelFormatBGRA8Unorm;
    view.drawableSize = CGSizeMake(side, side);
    view.autoResizeDrawable = NO;
    view.framebufferOnly = YES;
    CAMetalLayer *backingLayer = (CAMetalLayer *)view.layer;
    backingLayer.maximumDrawableCount = 3;
    backingLayer.allowsNextDrawableTimeout = YES;
    ProbeDelegate *delegate = [ProbeDelegate new];
    delegate.queue = queue;
    view.delegate = delegate;
    window.contentView = view;
    BOOL passed = YES;
    postInstallWarmup();
    beginNoMainPump();
    for (NSUInteger frame = 0; passed && frame < requestedFrames; frame++) {
        @autoreleasepool {
            stage("calling MTKView draw (critical loop does not pump main runloop)");
            delegate.passed = NO;
            [view draw];
            passed = delegate.passed;
            if (!passed) stage("FAIL: MTKView delegate frame");
        }
    }
    endNoMainPump();
    return passed;
}

int main(int argc, const char **argv) {
    @autoreleasepool {
        if (argc != 2 || (strcmp(argv[1], "cametal-thread") && strcmp(argv[1], "mtkview-thread"))) {
            fprintf(stderr, "Usage: %s cametal-thread|mtkview-thread\n", argv[0]);
            return 2;
        }
        const char *mode = argv[1];
        printf("PROBE: mode=%s frames=12 pool_count=3; texture storage is read from actual drawable; screen output needs separate visual confirmation\n", mode);
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
            else if (!strcmp(mode, "cametal-thread")) passed = runCAMetal(window.contentView, device, queue);
            else passed = runMTKView(window, device, queue);
        } @catch (NSException *exception) {
            fprintf(stderr, "EXCEPTION: %s: %s\n", exception.name.UTF8String, exception.reason.UTF8String);
        }
        [window orderOut:nil];
        [window close];
        printf("RESULT: api_gpu_checks=%s frames=%lu surface_pixels=%lu presented_callbacks=%u worker_returns=%u callbacks_before_drain=%u positive_times=%u zero_times=%u callback_metadata_errors=%u explicit_pump_calls=%u screen_verified=false\n",
               passed ? "PASS" : "FAIL", (unsigned long)completedFrames,
               (unsigned long)verifiedSurfacePixels, atomic_load(&presentedCallbacks), atomic_load(&workerPresentReturns), callbacksBeforeDrain,
               atomic_load(&positivePresentedTimes), atomic_load(&zeroPresentedTimes), atomic_load(&callbackMetadataErrors), atomic_load(&explicitMainPumpViolations));
        fflush(stdout);
        return passed ? 0 : 1;
    }
}
