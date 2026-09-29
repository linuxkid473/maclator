// Host side of the Maclator Metal bridge (x86-64, runs inside the maclator process).
//
// The guest (arm64) shim forwards Objective-C messages sent to Metal proxy objects
// as binary-plist RPCs. This side owns the real Metal objects (handle table), performs
// the calls with NSInvocation, and turns results back into handles/values.
// Guest and host share one address space, so raw pointers (bytes, MTLBuffer.contents,
// out-parameters) pass through untouched.
#import <Foundation/Foundation.h>
#import <objc/runtime.h>
#import <objc/message.h>
#import <Metal/Metal.h>
#include <dlfcn.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>

#ifndef MCL_LOOPBACK
#define MCL_EXPORT __attribute__((visibility("default")))
#else
#define MCL_EXPORT __attribute__((visibility("hidden")))
#endif

static BOOL gTrace, gStats;
#define TRACE(...) do { if (gTrace) fprintf(stderr, "[mclbridge] " __VA_ARGS__), fputc('\n', stderr); } while (0)

// ---- handle table -------------------------------------------------------------------
@interface MCLEntry : NSObject { @public id obj; int n; }
@end
@implementation MCLEntry
@end

static NSMutableDictionary<NSNumber *, MCLEntry *> *gObjs;
static NSMapTable *gRev; // object pointer -> handle
static uint64_t gNext = 1;
static NSLock *gLock;
static NSMutableDictionary<NSValue *, NSDictionary *> *gClassInfo;

static void ensureInit(void) {
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        gObjs = [NSMutableDictionary new];
        gRev = [NSMapTable mapTableWithKeyOptions:NSPointerFunctionsOpaquePersonality | NSPointerFunctionsOpaqueMemory valueOptions:NSPointerFunctionsOpaquePersonality | NSPointerFunctionsOpaqueMemory];
        gLock = [NSLock new];
        gClassInfo = [NSMutableDictionary new];
        gTrace = getenv("MCL_TRACE") != NULL;
        gStats = getenv("MCL_STATS") != NULL;
    });
}

static id NILV(void) { return @{@"z": @YES}; }

static uint64_t handleFor(id o, BOOL adopt) {
    [gLock lock];
    void *existing = NSMapGet(gRev, (__bridge void *)o);
    uint64_t h;
    if (existing) {
        h = (uint64_t)existing;
        gObjs[@(h)]->n++;
        if (adopt) CFRelease((__bridge CFTypeRef)o); // table already holds a reference
    } else {
        h = gNext++;
        MCLEntry *e = [MCLEntry new];
        if (adopt) e->obj = (__bridge_transfer id)(__bridge void *)o; else e->obj = o;
        e->n = 1;
        gObjs[@(h)] = e;
        NSMapInsert(gRev, (__bridge void *)o, (void *)h);
    }
    [gLock unlock];
    return h;
}

static id objFor(NSNumber *h) {
    [gLock lock];
    MCLEntry *e = gObjs[h];
    id o = e ? e->obj : nil;
    [gLock unlock];
    return o;
}

static void dropHandle(uint64_t h, int cnt) {
    [gLock lock];
    MCLEntry *e = gObjs[@(h)];
    if (e) {
        e->n -= cnt;
        if (e->n <= 0) {
            NSMapRemove(gRev, (__bridge void *)e->obj);
            [gObjs removeObjectForKey:@(h)];
        }
    }
    [gLock unlock];
}

static NSDictionary *classInfo(id o) {
    Class c = object_getClass(o);
    NSValue *k = [NSValue valueWithPointer:(__bridge void *)c];
    [gLock lock];
    NSDictionary *d = gClassInfo[k];
    [gLock unlock];
    if (d) return d;
    NSMutableArray *chain = [NSMutableArray new];
    NSMutableSet *protos = [NSMutableSet new];
    for (Class x = c; x; x = class_getSuperclass(x)) {
        [chain addObject:@(class_getName(x))];
        unsigned n = 0;
        Protocol *__unsafe_unretained *pl = class_copyProtocolList(x, &n);
        NSMutableArray *stack = [NSMutableArray new];
        for (unsigned i = 0; i < n; i++) [stack addObject:(__bridge id)(__bridge void *)pl[i]];
        free(pl);
        while (stack.count) {
            Protocol *p = (__bridge Protocol *)(__bridge void *)stack.lastObject;
            [stack removeLastObject];
            NSString *nm = @(protocol_getName(p));
            if ([protos containsObject:nm]) continue;
            [protos addObject:nm];
            unsigned m = 0;
            Protocol *__unsafe_unretained *sub = protocol_copyProtocolList(p, &m);
            for (unsigned i = 0; i < m; i++) [stack addObject:(__bridge id)(__bridge void *)sub[i]];
            free(sub);
        }
    }
    d = @{@"c": chain, @"p": protos.allObjects};
    [gLock lock];
    gClassInfo[k] = d;
    [gLock unlock];
    return d;
}

// ---- encode / decode ----------------------------------------------------------------
static id encObj(id o, BOOL adopt);

static NSDictionary *encHandle(id o, BOOL adopt) {
    uint64_t h = handleFor(o, adopt);
    NSDictionary *ci = classInfo(o);
    return @{@"h": @(h), @"c": ci[@"c"], @"p": ci[@"p"]};
}

static id encObj(id o, BOOL adopt) {
    if (!o) return NILV();
    if ([o isKindOfClass:[NSString class]] || [o isKindOfClass:[NSNumber class]] || [o isKindOfClass:[NSData class]]) return o;
    if ([o isKindOfClass:[NSArray class]]) {
        NSMutableArray *a = [NSMutableArray new];
        for (id e in (NSArray *)o) [a addObject:encObj(e, NO)];
        return a;
    }
    if ([o isKindOfClass:[NSError class]]) {
        NSError *e = o;
        return @{@"e": @{@"d": e.domain ?: @"", @"k": @(e.code), @"m": e.localizedDescription ?: @"", @"f": e.localizedFailureReason ?: @""}};
    }
    return encHandle(o, adopt);
}

static id decObj(id v);
static void applyDesc(id target, NSDictionary *d);

static id buildDesc(NSDictionary *d) {
    Class c = NSClassFromString(d[@"$d"]);
    if (!c) { TRACE("unknown descriptor class %s", [d[@"$d"] UTF8String]); return nil; }
    id o = [[c alloc] init];
    applyDesc(o, d);
    return o;
}

static id decObj(id v) {
    if ([v isKindOfClass:[NSDictionary class]]) {
        NSDictionary *d = v;
        if (d[@"z"]) return nil;
        if (d[@"h"]) return objFor(d[@"h"]);
        if (d[@"$d"]) return buildDesc(d);
        if (d[@"dd"]) {
            NSData *nd = d[@"dd"];
            return (id)dispatch_data_create(nd.bytes, nd.length, NULL, DISPATCH_DATA_DESTRUCTOR_DEFAULT);
        }
        if (d[@"url"]) return [NSURL URLWithString:d[@"url"]];
        if (d[@"u"]) { TRACE("unsupported argument class %s", [d[@"u"] UTF8String]); return nil; }
        NSMutableDictionary *m = [NSMutableDictionary new];
        for (id k in d) { id x = decObj(d[k]); if (x) m[k] = x; }
        return m;
    }
    if ([v isKindOfClass:[NSArray class]]) {
        NSMutableArray *a = [NSMutableArray new];
        for (id e in (NSArray *)v) { id x = decObj(e); [a addObject:x ?: [NSNull null]]; }
        return a;
    }
    return v;
}

static const char *skipQual(const char *t) {
    while (*t == 'r' || *t == 'n' || *t == 'N' || *t == 'o' || *t == 'O' || *t == 'R' || *t == 'V') t++;
    return t;
}

static void setBytesArg(NSInvocation *inv, NSUInteger idx, const char *type, NSData *d) {
    NSUInteger sz = 0;
    NSGetSizeAndAlignment(type, &sz, NULL);
    void *buf = calloc(1, sz ? sz : 8);
    memcpy(buf, d.bytes, MIN(sz, d.length));
    [inv setArgument:buf atIndex:idx];
    free(buf);
}

static void applyDesc(id target, NSDictionary *d) {
    NSArray *arr = d[@"a"];
    if (arr) {
        for (NSUInteger i = 0; i < arr.count; i++) {
            NSDictionary *e = arr[i];
            if ([e isKindOfClass:[NSDictionary class]] && e[@"z"]) continue;
            id elem = ((id(*)(id, SEL, NSUInteger))objc_msgSend)(target, sel_registerName("objectAtIndexedSubscript:"), i);
            if (elem) applyDesc(elem, e);
        }
    }
    for (NSDictionary *p in d[@"p"]) {
        @try {
            TRACE("  prop %s (%s) setter=%s", [p[@"n"] UTF8String], [p[@"t"] UTF8String], [p[@"s"] UTF8String]);
            NSString *setter = p[@"s"];
            NSString *type = p[@"t"];
            id v = p[@"v"];
            BOOL isObj = [type hasPrefix:@"@"];
            if (isObj && [v isKindOfClass:[NSDictionary class]] && ((NSDictionary *)v)[@"$d"]) {
                if (setter.length) {
                    id sub = buildDesc(v);
                    if (sub) ((void(*)(id, SEL, id))objc_msgSend)(target, NSSelectorFromString(setter), sub);
                } else {
                    id cur = ((id(*)(id, SEL))objc_msgSend)(target, NSSelectorFromString(p[@"g"]));
                    if (cur) applyDesc(cur, v);
                }
                continue;
            }
            if (!setter.length) continue;
            SEL ss = NSSelectorFromString(setter);
            NSMethodSignature *sig = [target methodSignatureForSelector:ss];
            if (!sig) continue;
            NSInvocation *inv = [NSInvocation invocationWithMethodSignature:sig];
            inv.target = target;
            inv.selector = ss;
            [inv retainArguments];
            if (isObj) {
                id o = decObj(v);
                [inv setArgument:&o atIndex:2];
            } else {
                setBytesArg(inv, 2, [sig getArgumentTypeAtIndex:2], v);
            }
            [inv invoke];
        } @catch (NSException *e) {
            TRACE("descriptor property %s failed: %s", [p[@"n"] UTF8String], [[e reason] UTF8String]);
        }
    }
}

// ---- events (callbacks into guest blocks) -------------------------------------------
static NSMutableArray *gEvents;
static NSCondition *gEvCond;

static void pushEvent(NSDictionary *ev) {
    [gEvCond lock];
    [gEvents addObject:ev];
    [gEvCond signal];
    [gEvCond unlock];
}

static id makeBlock(NSDictionary *bd) {
    uint64_t bid = [bd[@"blk"] unsignedLongLongValue];
    NSArray *types = bd[@"sig"];
    NSUInteger n = types.count;
    void (^blk)(void *, void *, void *, void *) = ^(void *a, void *b, void *c, void *d) {
        void *raw[4] = {a, b, c, d};
        NSMutableArray *args = [NSMutableArray new];
        for (NSUInteger i = 0; i < n && i < 4; i++) {
            if ([types[i] hasPrefix:@"@"]) [args addObject:encObj((__bridge id)raw[i], NO)];
            else { uint64_t x = (uint64_t)raw[i]; [args addObject:[NSData dataWithBytes:&x length:8]]; }
        }
        pushEvent(@{@"blk": @(bid), @"args": args});
    };
    return [blk copy];
}

// ---- message dispatch ---------------------------------------------------------------
static BOOL isNewFamily(NSString *sel) {
    const char *s = sel.UTF8String;
    while (*s == '_') s++;
    static const char *fam[] = {"new", "alloc", "copy", "mutableCopy"};
    for (int i = 0; i < 4; i++) {
        size_t l = strlen(fam[i]);
        if (strncmp(s, fam[i], l) == 0 && !(s[l] >= 'a' && s[l] <= 'z')) return YES;
    }
    return NO;
}

static NSDictionary *doMsg(NSDictionary *req) {
    id target = objFor(req[@"h"]);
    NSString *selName = req[@"sel"];
    if (!target) return @{@"x": [NSString stringWithFormat:@"invalid handle %@ for %@", req[@"h"], selName]};
    SEL sel = NSSelectorFromString(selName);
    NSMethodSignature *sig = [target methodSignatureForSelector:sel];
    if (!sig) return @{@"x": [NSString stringWithFormat:@"-[%s %@]: unrecognized selector", object_getClassName(target), selName]};
    NSInvocation *inv = [NSInvocation invocationWithMethodSignature:sig];
    inv.target = target;
    inv.selector = sel;
    [inv retainArguments];
    NSArray *args = req[@"args"];
    NSMutableArray *keep = [NSMutableArray new];
    NSError *__autoreleasing *errSlot = NULL;
    NSError *__autoreleasing errStore = nil;
    BOOL wantErr = NO;
    void **idArrays[8]; int nIdArr = 0;
    for (NSUInteger i = 2; i < sig.numberOfArguments; i++) {
        const char *t = [sig getArgumentTypeAtIndex:i];
        const char *b = skipQual(t);
        id v = i - 2 < args.count ? args[i - 2] : nil;
        if (b[0] == '@' && b[1] == '?') {
            id blk = ([v isKindOfClass:[NSDictionary class]] && v[@"blk"]) ? makeBlock(v) : nil;
            if (blk) [keep addObject:blk];
            [inv setArgument:&blk atIndex:i];
        } else if (b[0] == '@') {
            id o = decObj(v);
            if (o) [keep addObject:o];
            [inv setArgument:&o atIndex:i];
        } else if (b[0] == '^' && b[1] == '@') {
            if (t[0] == 'r') {
                NSArray *ids = ([v isKindOfClass:[NSDictionary class]]) ? v[@"ids"] : nil;
                void **arr = calloc(ids.count ? ids.count : 1, sizeof(void *));
                for (NSUInteger k = 0; k < ids.count; k++) {
                    id o = decObj(ids[k]);
                    if (o) [keep addObject:o];
                    arr[k] = (__bridge void *)o;
                }
                idArrays[nIdArr++] = arr;
                [inv setArgument:&arr atIndex:i];
            } else {
                wantErr = [v isKindOfClass:[NSDictionary class]] && v[@"outerr"];
                errSlot = wantErr ? &errStore : NULL;
                [inv setArgument:&errSlot atIndex:i];
            }
        } else {
            setBytesArg(inv, i, t, v);
        }
    }
    @try {
        [inv invoke];
    } @catch (NSException *e) {
        for (int k = 0; k < nIdArr; k++) free(idArrays[k]);
        return @{@"x": [NSString stringWithFormat:@"%@: %@", e.name, e.reason]};
    }
    for (int k = 0; k < nIdArr; k++) free(idArrays[k]);
    NSMutableDictionary *reply = [NSMutableDictionary new];
    const char *rt = skipQual([sig methodReturnType]);
    if (rt[0] == 'v') {
    } else if (rt[0] == '@') {
        __unsafe_unretained id r = nil;
        [inv getReturnValue:&r];
        // NSInvocation does not retain the result; new-family results are +1, others +0.
        // The handle table takes its own reference, so drop ours for the +1 case.
        BOOL nw = r && isNewFamily(selName);
        reply[@"r"] = encObj(r, NO);
        if (nw) CFRelease((__bridge CFTypeRef)r);
    } else {
        NSUInteger sz = 0;
        NSGetSizeAndAlignment(rt, &sz, NULL);
        NSMutableData *d = [NSMutableData dataWithLength:sz];
        [inv getReturnValue:d.mutableBytes];
        reply[@"r"] = d;
    }
    if (wantErr && errStore) reply[@"err"] = encObj(errStore, NO);
    return reply;
}

static NSDictionary *handleReq(NSDictionary *req) {
    NSString *cmd = req[@"c"];
    for (NSNumber *h in req[@"drop"]) dropHandle(h.unsignedLongLongValue, 1);
    if ([cmd isEqualToString:@"msg"]) return doMsg(req);
    if ([cmd isEqualToString:@"sig"]) {
        id target = objFor(req[@"h"]);
        SEL sel = NSSelectorFromString(req[@"sel"]);
        Method m = target ? class_getInstanceMethod(object_getClass(target), sel) : NULL;
        if (!m) return @{};
        return @{@"t": @(method_getTypeEncoding(m))};
    }
    if ([cmd isEqualToString:@"root"]) {
        NSString *what = req[@"what"];
        if ([what isEqualToString:@"device"]) return @{@"r": encObj(MTLCreateSystemDefaultDevice(), NO)};
        if ([what isEqualToString:@"all"]) return @{@"r": encObj(MTLCopyAllDevices(), NO)};
    }
    return @{@"x": @"bad request"};
}

// op 1: RPC (in = binary plist) -> malloc'd binary plist. op 2: free(in). op 3: wait for an event.
#ifdef MCL_LOOPBACK
extern __thread int mcl_in_host;
#endif
#include <mach/mach_time.h>
static _Atomic uint64_t gStatCalls, gStatNs, gStatIn, gStatOut;
static NSMutableDictionary *gStatSel; // selector -> @[count, ns]

static void statNote(NSString *sel, uint64_t ns) {
    @synchronized(gLock) {
        if (!gStatSel) gStatSel = [NSMutableDictionary new];
        NSArray *e = gStatSel[sel];
        gStatSel[sel] = @[@([e[0] unsignedLongLongValue] + 1), @([e[1] unsignedLongLongValue] + ns)];
    }
}

static void statDump(void) {
    static uint64_t last;
    uint64_t now = mach_absolute_time();
    mach_timebase_info_data_t tb;
    mach_timebase_info(&tb);
    if ((now - last) * tb.numer / tb.denom < 5000000000ull) return;
    last = now;
    NSMutableString *s = [NSMutableString stringWithFormat:@"[mclstats] pid %d calls=%llu host_ms=%llu in=%lluKB out=%lluKB\n", getpid(), gStatCalls, gStatNs / 1000000, gStatIn / 1024, gStatOut / 1024];
    NSArray *keys = [gStatSel keysSortedByValueUsingComparator:^NSComparisonResult(NSArray *a, NSArray *b) { return [b[1] compare:a[1]]; }];
    for (NSUInteger i = 0; i < keys.count && i < 8; i++) {
        NSArray *e = gStatSel[keys[i]];
        [s appendFormat:@"   %-52s n=%-7llu ms=%llu\n", [keys[i] UTF8String], [e[0] unsignedLongLongValue], [e[1] unsignedLongLongValue] / 1000000];
    }
    fputs(s.UTF8String, stderr);
    [gStatSel removeAllObjects];
    gStatCalls = gStatNs = gStatIn = gStatOut = 0;
}

MCL_EXPORT void *mcl_call(uint64_t op, const void *in, uint64_t inlen, uint64_t *outlen) {
    ensureInit();
    *outlen = 0;
#ifdef MCL_LOOPBACK
    int savedInHost = mcl_in_host;
    if (op == 1) mcl_in_host = 1;
#endif
    if (op == 2) { free((void *)in); return NULL; }
    @autoreleasepool {
        NSDictionary *reply = nil;
        if (op == 1) {
            NSData *d = [NSData dataWithBytesNoCopy:(void *)in length:inlen freeWhenDone:NO];
            NSError *e = nil;
            NSDictionary *req = [NSPropertyListSerialization propertyListWithData:d options:NSPropertyListImmutable format:NULL error:&e];
            if (!req) reply = @{@"x": @"bad plist"};
            else {
                uint64_t t0 = gStats ? mach_absolute_time() : 0;
                TRACE("%s %s h=%s", [req[@"c"] UTF8String], [(req[@"sel"] ?: @"") UTF8String], [[req[@"h"] description] UTF8String]);
                reply = handleReq(req);
                if (gStats) {
                    mach_timebase_info_data_t tb;
                    mach_timebase_info(&tb);
                    uint64_t ns = (mach_absolute_time() - t0) * tb.numer / tb.denom;
                    gStatCalls++; gStatNs += ns; gStatIn += inlen;
                    statNote(req[@"sel"] ?: req[@"c"], ns);
                }
            }
        } else if (op == 3) {
            static dispatch_once_t once;
            dispatch_once(&once, ^{ });
            [gEvCond lock];
            while (gEvents.count == 0) [gEvCond wait];
            reply = gEvents.firstObject;
            [gEvents removeObjectAtIndex:0];
            [gEvCond unlock];
        } else {
            reply = @{@"x": @"bad op"};
        }
        NSData *out = [NSPropertyListSerialization dataWithPropertyList:reply format:NSPropertyListBinaryFormat_v1_0 options:0 error:NULL];
        void *p = malloc(out.length);
        memcpy(p, out.bytes, out.length);
        *outlen = out.length;
        if (gStats) { gStatOut += out.length; statDump(); }
#ifdef MCL_LOOPBACK
        mcl_in_host = savedInHost;
#endif
        return p;
    }
}

// Initialise Metal (device, compiler service, XPC/IOKit connections) before the guest starts:
// doing it mid-run disturbs the guest's own XPC/LaunchServices state in the shared process.
MCL_EXPORT void mcl_warmup(void) {
    ensureInit();
    @autoreleasepool {
        id<MTLDevice> d = MTLCreateSystemDefaultDevice();
        if (!d) return;
        static id keep;
        keep = d;
        id<MTLCommandQueue> q = [d newCommandQueue];
        NSError *e = nil;
        id<MTLLibrary> lib = [d newLibraryWithSource:@"kernel void mcl_warm(device float *o [[buffer(0)]], uint i [[thread_position_in_grid]]) { o[i] = 1.0; }" options:nil error:&e];
        id<MTLFunction> f = [lib newFunctionWithName:@"mcl_warm"];
        id<MTLComputePipelineState> ps = f ? [d newComputePipelineStateWithFunction:f error:&e] : nil;
        id<MTLBuffer> b = [d newBufferWithLength:64 options:MTLResourceStorageModeShared];
        id<MTLCommandBuffer> cb = [q commandBuffer];
        id<MTLComputeCommandEncoder> enc = [cb computeCommandEncoder];
        if (ps) {
            [enc setComputePipelineState:ps];
            [enc setBuffer:b offset:0 atIndex:0];
            [enc dispatchThreads:MTLSizeMake(4, 1, 1) threadsPerThreadgroup:MTLSizeMake(4, 1, 1)];
        }
        [enc endEncoding];
        [cb commit];
        [cb waitUntilCompleted];
        TRACE("warmup done on %s", d.name.UTF8String);
    }
}

__attribute__((constructor)) static void mclInitEvents(void) {
    gEvents = [NSMutableArray new];
    gEvCond = [NSCondition new];
}
