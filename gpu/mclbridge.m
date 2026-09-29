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
#include <pthread.h>
#include <unistd.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <mach/mach_time.h>
#include "mclproto.h"

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
static uint32_t gNextClassId = 1;
static NSMutableDictionary *gSigCache;   // (class, selector id) -> NSMethodSignature
static NSMutableDictionary *gMemo;       // hash -> @[key NSData, object]
static NSSet *gMemoLegacy;
static id memoGet(NSData *key, NSNumber **hashOut);
static void memoPut(NSNumber *hk, NSData *key, id obj);
static NSMutableSet *gClassSent;   // class ids whose description the guest has received

static void ensureInit(void) {
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        gObjs = [NSMutableDictionary new];
        gRev = [NSMapTable mapTableWithKeyOptions:NSPointerFunctionsOpaquePersonality | NSPointerFunctionsOpaqueMemory valueOptions:NSPointerFunctionsOpaquePersonality | NSPointerFunctionsOpaqueMemory];
        gLock = [NSLock new];
        gClassInfo = [NSMutableDictionary new];
        gClassSent = [NSMutableSet new];
        gSigCache = [NSMutableDictionary new];
        gMemoLegacy = [NSSet setWithObjects:@"newRenderPipelineStateWithDescriptor:error:", @"newComputePipelineStateWithFunction:error:", @"newLibraryWithSource:options:error:", @"newLibraryWithData:error:", nil];
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
    if (gTrace) fprintf(stderr, "[mclbridge] mention h=%llu n=%d %s adopt=%d\n", h, gObjs[@(h)]->n, object_getClassName(o), adopt);
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

// The guest names objects it asked us to create asynchronously (command buffers, encoders).
static void adoptWithHandle(id o, uint64_t h) {
    [gLock lock];
    void *existing = NSMapGet(gRev, (__bridge void *)o);
    if (existing) {
        MCLEntry *e = gObjs[@((uint64_t)existing)];
        e->n++;
        gObjs[@(h)] = e;
    } else {
        MCLEntry *e = [MCLEntry new];
        e->obj = o;
        e->n = 1;
        gObjs[@(h)] = e;
        NSMapInsert(gRev, (__bridge void *)o, (void *)h);
    }
    [gLock unlock];
}

// A handle chosen by another guest thread may not have been flushed yet: wait for it briefly.
static id objForWait(uint64_t h) {
    id o = objFor(@(h));
    for (int i = 0; !o && h >= MCL_GUEST_HANDLE_BASE && i < 500; i++) {
        usleep(200);
        o = objFor(@(h));
    }
    if (!o) fprintf(stderr, "[mclbridge] handle %llu not found (thread %p)\n", h, (void *)pthread_self());
    return o;
}

static void dropHandle(uint64_t h, int cnt) {
    [gLock lock];
    MCLEntry *e = gObjs[@(h)];
    if (gTrace) fprintf(stderr, "[mclbridge] drop h=%llu n=%d->%d %s\n", h, e ? e->n : -1, e ? e->n - cnt : -1, e ? object_getClassName(e->obj) : "?");
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
    [gLock lock];
    d = gClassInfo[k];
    if (!d) {
        d = @{@"c": chain, @"p": protos.allObjects, @"i": @(gNextClassId++)};
        gClassInfo[k] = d;
    }
    [gLock unlock];
    return d;
}

// ---- encode / decode ----------------------------------------------------------------
static id encObj(id o, BOOL adopt);

static NSDictionary *encHandle(id o, BOOL adopt) {
    uint64_t h = handleFor(o, adopt);
    NSDictionary *ci = classInfo(o);
    return @{@"h": @(h), @"c": ci[@"c"], @"p": ci[@"p"], @"i": ci[@"i"]};
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
static id parseDesc(const uint8_t **pp, id target);
static NSDictionary *defineSchema(NSDictionary *req);
static void setBytesRaw(NSInvocation *inv, NSUInteger idx, const char *type, const void *p, uint32_t n);
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
        if (d[@"dbin"]) { NSData *bd = d[@"dbin"]; const uint8_t *dp = bd.bytes; return bd.length ? parseDesc(&dp, nil) : nil; }
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

static _Atomic uint64_t gEvPushed;
static void pushEvent(NSDictionary *ev) {
    [gEvCond lock];
    gEvPushed++;
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
    CFTypeRef surfs[4]; int nSurf = 0;
    for (NSUInteger i = 2; i < sig.numberOfArguments; i++) {
        const char *t = [sig getArgumentTypeAtIndex:i];
        const char *b = skipQual(t);
        id v = i - 2 < args.count ? args[i - 2] : nil;
        if ([v isKindOfClass:[NSDictionary class]] && ((NSDictionary *)v)[@"iosurf"]) {
            static CFTypeRef (*lookup)(uint32_t);
            static dispatch_once_t once;
            dispatch_once(&once, ^{ lookup = dlsym(RTLD_DEFAULT, "IOSurfaceLookup"); });
            uint32_t sid = [((NSDictionary *)v)[@"iosurf"] unsignedIntValue];
            CFTypeRef surf = (sid && lookup) ? lookup(sid) : NULL;
            if (!surf) TRACE("IOSurface id %u not found", sid);
            if (surf && nSurf < 4) surfs[nSurf++] = surf;
            [inv setArgument:&surf atIndex:i];
        } else if (b[0] == '@' && b[1] == '?') {
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
    NSNumber *memoKey = nil;
    NSData *memoData = nil;
    if ([gMemoLegacy containsObject:selName]) {
        NSData *ab = [NSPropertyListSerialization dataWithPropertyList:@{@"h": req[@"h"], @"s": selName, @"a": args ?: @[]} format:NSPropertyListBinaryFormat_v1_0 options:0 error:NULL];
        if (ab) {
            memoData = ab;
            id hit = memoGet(ab, &memoKey);
            if (hit) {
                for (int k = 0; k < nIdArr; k++) free(idArrays[k]);
                return @{@"r": encObj(hit, NO)};
            }
        }
    }
    @try {
        [inv invoke];
    } @catch (NSException *e) {
        for (int k = 0; k < nIdArr; k++) free(idArrays[k]);
        for (int k = 0; k < nSurf; k++) CFRelease(surfs[k]);
        return @{@"x": [NSString stringWithFormat:@"%@: %@", e.name, e.reason]};
    }
    for (int k = 0; k < nIdArr; k++) free(idArrays[k]);
    for (int k = 0; k < nSurf; k++) CFRelease(surfs[k]);
    NSMutableDictionary *reply = [NSMutableDictionary new];
    const char *rt = skipQual([sig methodReturnType]);
    if (rt[0] == 'v') {
    } else if (rt[0] == '@') {
        __unsafe_unretained id r = nil;
        [inv getReturnValue:&r];
        // NSInvocation does not retain the result; new-family results are +1, others +0.
        // The handle table takes its own reference, so drop ours for the +1 case.
        BOOL nw = r && isNewFamily(selName);
        if (r && memoKey && !(wantErr && errStore)) memoPut(memoKey, memoData, r);
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
    if ([cmd isEqualToString:@"schema"]) return defineSchema(req);
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

// ---- fast path: binary command batches (see mclproto.h) ---------------------------------------------------
#define MAXSEL 8192
static SEL gSelTab[MAXSEL];
static char *gSelNames[MAXSEL];
static uint8_t gSelNew[MAXSEL], gSelMemo[MAXSEL];
static _Atomic uint64_t gSelCnt[MAXSEL], gSelNs[MAXSEL];

static void defSel(uint32_t id, const char *name) {
    if (id == 0 || id >= MAXSEL) return;
    gSelNames[id] = strdup(name);
    gSelNew[id] = isNewFamily(@(name));
    static const char *const memo[] = {"newSamplerStateWithDescriptor:", "newDepthStencilStateWithDescriptor:", "newFunctionWithName:", NULL};
    for (int i = 0; memo[i]; i++) if (strcmp(memo[i], name) == 0) gSelMemo[id] = 1;
    __atomic_store_n(&gSelTab[id], sel_registerName(name), __ATOMIC_RELEASE);
}

static uint64_t fnv(const void *p, size_t n, uint64_t h) {
    const uint8_t *b = p;
    for (size_t i = 0; i < n; i++) { h ^= b[i]; h *= 0x100000001b3ull; }
    return h;
}
// Pipeline / library / sampler creation is deterministic: identical requests return the same object.
static id memoGet(NSData *key, NSNumber **hashOut) {
    uint64_t h = fnv(key.bytes, key.length, 0xcbf29ce484222325ull);
    NSNumber *hk = @(h);
    *hashOut = hk;
    [gLock lock];
    NSArray *e = gMemo[hk];
    [gLock unlock];
    if (e && [e[0] isEqualToData:key]) return e[1];
    return nil;
}
static void memoPut(NSNumber *hk, NSData *key, id obj) {
    if (!obj) return;
    [gLock lock];
    if (!gMemo) gMemo = [NSMutableDictionary new];
    gMemo[hk] = @[key, obj];
    [gLock unlock];
}

static __thread uint8_t *tRep;
static __thread size_t tRepCap;
static void *putReply(uint32_t tag, uint32_t cls, uint64_t value, const void *blob, uint32_t blen, uint64_t *outlen) {
    size_t n = sizeof(MclReply) + blen;
    if (n > tRepCap) { tRepCap = n + 4096; tRep = realloc(tRep, tRepCap); }
    MclReply r = {tag, cls, value, blen, 0};
    memcpy(tRep, &r, sizeof r);
    if (blen) memcpy(tRep + sizeof r, blob, blen);
    *outlen = n;
    return tRep;
}

// ---- binary descriptors (see desc_emit in mclmetal.m) ---------------------------------------------------------
@interface HProp : NSObject { @public SEL getSel, setSel; char kind; uint8_t size; BOOL ro; NSString *typeStr; NSMethodSignature *setSig; }
@end
@implementation HProp
@end
@interface HSchema : NSObject { @public NSString *cls; BOOL indexed; NSMutableArray<HProp *> *props; }
@end
@implementation HSchema
@end
#define MAXSCHEMA 4096
static HSchema *gSchemas[MAXSCHEMA];

static NSDictionary *defineSchema(NSDictionary *req) {
    uint32_t sid = [req[@"id"] unsignedIntValue];
    if (sid == 0 || sid >= MAXSCHEMA) return @{@"x": @"bad schema id"};
    HSchema *sc = [HSchema new];
    sc->cls = req[@"cls"];
    sc->indexed = [req[@"idx"] boolValue];
    sc->props = [NSMutableArray new];
    NSMutableArray *force = [NSMutableArray new];
    Class c = NSClassFromString(sc->cls);
    id pristine = (c && !sc->indexed) ? [[c alloc] init] : nil;
    NSData *def = req[@"def"];
    unsigned off = 0;
    unsigned idx = 0;
    for (NSDictionary *p in req[@"props"]) {
        HProp *hp = [HProp new];
        hp->getSel = NSSelectorFromString(p[@"g"]);
        NSString *setter = p[@"s"];
        hp->setSel = setter.length ? NSSelectorFromString(setter) : NULL;
        hp->kind = [p[@"k"] intValue];
        hp->size = [p[@"z"] intValue];
        hp->ro = hp->setSel == NULL;
        hp->typeStr = p[@"t"];
        [sc->props addObject:hp];
        if (hp->kind != 5 /* DK_OBJ */ && pristine) {
            // does the host's default agree with the guest's?
            BOOL same = NO;
            @try {
                NSMethodSignature *sig = [pristine methodSignatureForSelector:hp->getSel];
                if (sig) {
                    NSInvocation *inv = [NSInvocation invocationWithMethodSignature:sig];
                    inv.target = pristine;
                    inv.selector = hp->getSel;
                    [inv invoke];
                    uint8_t cur[64] = {0};
                    NSUInteger sz = 0;
                    NSGetSizeAndAlignment(sig.methodReturnType, &sz, NULL);
                    [inv getReturnValue:cur];
                    same = off + hp->size <= def.length && sz == hp->size && memcmp(cur, (const uint8_t *)def.bytes + off, hp->size) == 0;
                }
            } @catch (NSException *e) {}
            if (!same) [force addObject:@(idx)];
        }
        if (hp->kind != 5) off += hp->size;
        idx++;
    }
    gSchemas[sid] = sc;
    return @{@"force": force};
}

static id parseDesc(const uint8_t **pp, id target);
static void invokeSetter(id obj, HProp *p, const void *bytes, id objArg, BOOL isObj) {
    if (!p->setSel) return;
    if (!p->setSig) p->setSig = [obj methodSignatureForSelector:p->setSel];
    if (!p->setSig) return;
    NSInvocation *inv = [NSInvocation invocationWithMethodSignature:p->setSig];
    inv.target = obj;
    inv.selector = p->setSel;
    if (isObj) {
        id o = objArg;
        [inv setArgument:&o atIndex:2];
    } else {
        setBytesRaw(inv, 2, [p->setSig getArgumentTypeAtIndex:2], bytes, p->size);
    }
    [inv invoke];
}

// Parses one descriptor. With `target` set the properties are applied to that object, otherwise a fresh
// instance of the schema's class is built and returned.
static id parseDesc(const uint8_t **pp, id target) {
    const uint8_t *p = *pp;
    uint16_t sid;
    memcpy(&sid, p, 2); p += 2;
    HSchema *sc = sid < MAXSCHEMA ? gSchemas[sid] : nil;
    if (!sc) { TRACE("descriptor with unknown schema %u", sid); *pp = p; return nil; }
    id obj = target;
    if (!obj) {
        Class c = NSClassFromString(sc->cls);
        obj = c ? [[c alloc] init] : nil;
    }
    if (sc->indexed) {
        uint8_t n = *p++;
        for (int i = 0; i < n; i++) {
            uint8_t idx = *p++;
            id elem = obj ? ((id(*)(id, SEL, NSUInteger))objc_msgSend)(obj, sel_registerName("objectAtIndexedSubscript:"), idx) : nil;
            parseDesc(&p, elem);
        }
    } else {
        uint16_t n;
        memcpy(&n, p, 2); p += 2;
        for (int i = 0; i < n; i++) {
            uint16_t pi;
            memcpy(&pi, p, 2); p += 2;
            HProp *hp = pi < sc->props.count ? sc->props[pi] : nil;
            if (!hp) { TRACE("bad property index"); break; }
            @try {
                if (hp->kind == 5) {
                    uint8_t vt = *p++;
                    if (vt == 1) { uint64_t h; memcpy(&h, p, 8); p += 8; invokeSetter(obj, hp, NULL, objFor(@(h)), YES); }
                    else if (vt == 2) {
                        if (hp->setSel) { id sub = parseDesc(&p, nil); invokeSetter(obj, hp, NULL, sub, YES); }
                        else { id cur = obj ? ((id(*)(id, SEL))objc_msgSend)(obj, hp->getSel) : nil; parseDesc(&p, cur); }
                    } else if (vt == 3) {
                        uint32_t len; memcpy(&len, p, 4); p += 4;
                        NSData *d = [NSData dataWithBytes:p length:len]; p += len;
                        id v = [NSPropertyListSerialization propertyListWithData:d options:NSPropertyListImmutable format:NULL error:NULL];
                        invokeSetter(obj, hp, NULL, decObj(v), YES);
                    } else if (vt == 4) {
                        uint32_t len; memcpy(&len, p, 4); p += 4;
                        NSString *str = [[NSString alloc] initWithBytes:p length:len encoding:NSUTF8StringEncoding]; p += len;
                        invokeSetter(obj, hp, NULL, str, YES);
                    } else { TRACE("bad object tag %u", vt); break; }
                } else {
                    invokeSetter(obj, hp, p, nil, NO);
                    p += hp->size;
                }
            } @catch (NSException *e) {
                TRACE("descriptor property %d failed: %s", pi, [[e reason] UTF8String]);
            }
        }
    }
    *pp = p;
    return obj;
}

static NSData *classBlobIfNew(NSDictionary *ci) {
    BOOL isNew = NO;
    [gLock lock];
    if (![gClassSent containsObject:ci[@"i"]]) { [gClassSent addObject:ci[@"i"]]; isNew = YES; }
    [gLock unlock];
    if (!isNew) return nil;
    return [NSPropertyListSerialization dataWithPropertyList:@{@"c": ci[@"c"], @"p": ci[@"p"]} format:NSPropertyListBinaryFormat_v1_0 options:0 error:NULL];
}

static void setBytesRaw(NSInvocation *inv, NSUInteger idx, const char *type, const void *p, uint32_t n) {
    NSUInteger sz = 0;
    NSGetSizeAndAlignment(type, &sz, NULL);
    void *buf = calloc(1, sz ? sz : 8);
    memcpy(buf, p, MIN(sz, n));
    [inv setArgument:buf atIndex:idx];
    free(buf);
}

static NSMethodSignature *sigFor(id target, uint32_t selid) {
    NSNumber *k = @(((uint64_t)(uintptr_t)(__bridge void *)object_getClass(target)) | ((uint64_t)selid << 48));
    [gLock lock];
    NSMethodSignature *sg = gSigCache[k];
    [gLock unlock];
    if (!sg) {
        sg = [target methodSignatureForSelector:gSelTab[selid]];
        if (sg) { [gLock lock]; gSigCache[k] = sg; [gLock unlock]; }
    }
    return sg;
}

// Executes one call record. For MCL_R_SYNC, fills the reply and returns it.
static void *execCall(const uint8_t *rec, uint32_t reclen, uint8_t kind, uint32_t nargs, uint32_t selid, uint64_t th, uint64_t rh, uint64_t *outlen) {
    SEL sel = selid < MAXSEL ? __atomic_load_n(&gSelTab[selid], __ATOMIC_ACQUIRE) : NULL;
    id target = sel ? objForWait(th) : nil;
    BOOL sync = kind == MCL_R_SYNC;
    if (!target) {
        NSString *m = [NSString stringWithFormat:@"invalid handle %llu for %s", th, sel ? gSelNames[selid] : "?"];
        TRACE("%s", m.UTF8String);
        if (sync) return putReply(MCL_T_EXC, 0, 0, m.UTF8String, (uint32_t)m.length, outlen);
        return NULL;
    }
    if (gTrace) fprintf(stderr, "[mclbridge] rec kind=%d %s h=%llu (%s) nargs=%u\n", kind, gSelNames[selid], th, object_getClassName(target), nargs);
    NSMethodSignature *sig = sigFor(target, selid);
    if (!sig || sig.numberOfArguments < 2 + nargs) {
        NSString *m = [NSString stringWithFormat:@"-[%s %s]: unrecognized selector / arity", object_getClassName(target), gSelNames[selid]];
        if (sync) return putReply(MCL_T_EXC, 0, 0, m.UTF8String, (uint32_t)m.length, outlen);
        return NULL;
    }
    NSNumber *memoKey = nil;
    NSData *memoData = nil;
    if (sync && gSelMemo[selid]) {
        NSMutableData *kd = [NSMutableData dataWithBytes:&th length:8];
        [kd appendBytes:&selid length:4];
        [kd appendBytes:rec + 32 length:reclen - 32];
        memoData = kd;
        id hit = memoGet(kd, &memoKey);
        if (hit) {
            uint64_t hh = handleFor(hit, NO);
            NSDictionary *ci = classInfo(hit);
            NSData *blob = classBlobIfNew(ci);
            return putReply(MCL_T_HANDLE, [ci[@"i"] unsignedIntValue], hh, blob.bytes, (uint32_t)blob.length, outlen);
        }
    }
    NSInvocation *inv = [NSInvocation invocationWithMethodSignature:sig];
    inv.target = target;
    inv.selector = sel;
    NSMutableArray *keep = [NSMutableArray new];
    void **idArrays[8]; int nIdArr = 0;
    const uint8_t *ap = rec + 32;
    for (uint32_t k = 0; k < nargs; k++) {
        uint64_t ah;
        memcpy(&ah, ap, 8);
        uint8_t tag = ah & 0xff;
        uint32_t alen = (uint32_t)(ah >> 32);
        const uint8_t *pl = ap + 8;
        ap += 8 + ((alen + 7) & ~(uint32_t)7);
        NSUInteger idx = k + 2;
        const char *t = [sig getArgumentTypeAtIndex:idx];
        switch (tag) {
        case MCL_A_NIL: { void *z = NULL; [inv setArgument:&z atIndex:idx]; break; }
        case MCL_A_HANDLE: {
            uint64_t h; memcpy(&h, pl, 8);
            id o = objForWait(h);
            if (o) [keep addObject:o];
            [inv setArgument:&o atIndex:idx];
            break;
        }
        case MCL_A_STRING: {
            NSString *str = [[NSString alloc] initWithBytes:pl length:alen encoding:NSUTF8StringEncoding];
            if (str) [keep addObject:str];
            [inv setArgument:&str atIndex:idx];
            break;
        }
        case MCL_A_PLIST: {
            NSData *d = [NSData dataWithBytes:pl length:alen];
            id v = [NSPropertyListSerialization propertyListWithData:d options:NSPropertyListImmutable format:NULL error:NULL];
            id o = decObj(v);
            if (o) [keep addObject:o];
            [inv setArgument:&o atIndex:idx];
            break;
        }
        case MCL_A_DESC: {
            const uint8_t *dp = pl;
            id o = parseDesc(&dp, nil);
            if (o) [keep addObject:o];
            [inv setArgument:&o atIndex:idx];
            break;
        }
        case MCL_A_SCALAR: setBytesRaw(inv, idx, t, pl, alen); break;
        case MCL_A_IDARRAY: {
            uint32_t n = alen / 8;
            void **arr = calloc(n ? n : 1, sizeof(void *));
            for (uint32_t i = 0; i < n; i++) {
                uint64_t h; memcpy(&h, pl + i * 8, 8);
                id o = h ? objForWait(h) : nil;
                if (o) [keep addObject:o];
                arr[i] = (__bridge void *)o;
            }
            if (nIdArr < 8) idArrays[nIdArr++] = arr;
            [inv setArgument:&arr atIndex:idx];
            break;
        }
        case MCL_A_DATA: { const void *p = pl; [inv setArgument:&p atIndex:idx]; break; }
        }
    }
    uint64_t t0 = gStats ? mach_absolute_time() : 0;
    NSString *excMsg = nil;
    @try {
        [inv invoke];
    } @catch (NSException *e) {
        excMsg = [NSString stringWithFormat:@"%@: %@", e.name, e.reason];
        fprintf(stderr, "[mclbridge] %s: %s\n", gSelNames[selid], excMsg.UTF8String);
    }
    for (int i = 0; i < nIdArr; i++) free(idArrays[i]);
    if (gStats) {
        struct mach_timebase_info tb; mach_timebase_info(&tb);
        gSelNs[selid] += (mach_absolute_time() - t0) * tb.numer / tb.denom;
        gSelCnt[selid]++;
    }
    const char *rt = skipQual([sig methodReturnType]);
    if (kind == MCL_R_CREATE) {
        if (!excMsg && rt[0] == '@') {
            __unsafe_unretained id res = nil;
            [inv getReturnValue:&res];
            if (res) adoptWithHandle(res, rh);
            else fprintf(stderr, "[mclbridge] %s returned nil for guest handle %llu\n", gSelNames[selid], rh);
        } else if (excMsg) fprintf(stderr, "[mclbridge] %s failed for guest handle %llu\n", gSelNames[selid], rh);
        return NULL;
    }
    if (!sync) return NULL;
    if (excMsg) return putReply(MCL_T_EXC, 0, 0, excMsg.UTF8String, (uint32_t)excMsg.length, outlen);
    if (rt[0] == 'v') return putReply(MCL_T_VOID, 0, gEvPushed, NULL, 0, outlen);
    if (rt[0] != '@') {
        uint64_t v = 0;
        NSUInteger sz = 0;
        NSGetSizeAndAlignment(rt, &sz, NULL);
        if (sz > 8) return putReply(MCL_T_VOID, 0, 0, NULL, 0, outlen);
        [inv getReturnValue:&v];
        return putReply(MCL_T_SCALAR, 0, v, NULL, 0, outlen);
    }
    __unsafe_unretained id r = nil;
    [inv getReturnValue:&r];
    if (!r) return putReply(MCL_T_NIL, 0, 0, NULL, 0, outlen);
    BOOL nw = gSelNew[selid];
    id enc = encObj(r, NO);
    if (!([enc isKindOfClass:[NSDictionary class]] && ((NSDictionary *)enc)[@"h"])) {
        if (nw) CFRelease((__bridge CFTypeRef)r);
        NSData *b = [NSPropertyListSerialization dataWithPropertyList:enc format:NSPropertyListBinaryFormat_v1_0 options:0 error:NULL];
        return putReply(MCL_T_PLIST, 0, 0, b.bytes, (uint32_t)b.length, outlen);
    }
    if (memoKey) memoPut(memoKey, memoData, r);
    if (nw) CFRelease((__bridge CFTypeRef)r);
    NSDictionary *ci = classInfo(r);
    NSData *blob = classBlobIfNew(ci);
    return putReply(MCL_T_HANDLE, [ci[@"i"] unsignedIntValue], [((NSDictionary *)enc)[@"h"] unsignedLongLongValue], blob.bytes, (uint32_t)blob.length, outlen);
}

static void *doBatch(const uint8_t *in, uint64_t len, uint64_t *outlen) {
    void *reply = NULL;
    if (len < sizeof(MclMsgHdr)) return NULL;
    MclMsgHdr h;
    memcpy(&h, in, sizeof h);
    if (h.magic != MCL_MAGIC) return NULL;
    const uint8_t *p = in + sizeof h, *end = in + len;
    while (p + 32 <= end) {
        uint64_t w0, w1, th, rh;
        memcpy(&w0, p, 8); memcpy(&w1, p + 8, 8); memcpy(&th, p + 16, 8); memcpy(&rh, p + 24, 8);
        uint32_t rlen = (uint32_t)w0;
        uint8_t kind = (w0 >> 32) & 0xff, nargs = (w0 >> 40) & 0xff;
        if (rlen < 32 || p + rlen > end) break;
        @autoreleasepool {
            if (kind == MCL_R_DROP) dropHandle(th, 1);
            else reply = execCall(p, rlen, kind, nargs, (uint32_t)w1, th, rh, outlen) ?: reply;
        }
        p += rlen;
    }
    return reply;
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
    @synchronized(gLock) {
    static uint64_t last;
    uint64_t now = mach_absolute_time();
    mach_timebase_info_data_t tb;
    mach_timebase_info(&tb);
    if ((now - last) * tb.numer / tb.denom < 5000000000ull) return;
    last = now;
    for (uint32_t i = 1; i < MAXSEL; i++) {
        if (!gSelCnt[i]) continue;
        if (!gStatSel) gStatSel = [NSMutableDictionary new];
        NSString *nm = [@"* " stringByAppendingString:@(gSelNames[i] ?: "?")];
        NSArray *e = gStatSel[nm];
        gStatSel[nm] = @[@([e[0] unsignedLongLongValue] + gSelCnt[i]), @([e[1] unsignedLongLongValue] + gSelNs[i])];
        gSelCnt[i] = gSelNs[i] = 0;
    }
    NSMutableString *s = [NSMutableString stringWithFormat:@"[mclstats] (* = batched) pid %d msgs=%llu host_ms=%llu in=%lluKB out=%lluKB\n", getpid(), gStatCalls, gStatNs / 1000000, gStatIn / 1024, gStatOut / 1024];
    NSArray *keys = [gStatSel keysSortedByValueUsingComparator:^NSComparisonResult(NSArray *a, NSArray *b) { return [b[1] compare:a[1]]; }];
    for (NSUInteger i = 0; i < keys.count && i < 8; i++) {
        NSArray *e = gStatSel[keys[i]];
        [s appendFormat:@"   %-52s n=%-7llu ms=%llu\n", [keys[i] UTF8String], [e[0] unsignedLongLongValue], [e[1] unsignedLongLongValue] / 1000000];
    }
    fputs(s.UTF8String, stderr);
    [gStatSel removeAllObjects];
    gStatCalls = gStatNs = gStatIn = gStatOut = 0;
    }
}

MCL_EXPORT void *mcl_call(uint64_t op, const void *in, uint64_t inlen, uint64_t *outlen) {
    ensureInit();
    *outlen = 0;
#ifdef MCL_LOOPBACK
    int savedInHost = mcl_in_host;
    if (op == 1) mcl_in_host = 1;
#endif
    if (op == 2) { free((void *)in); return NULL; }
    if (op == MCL_OP_DEFSEL) {
        if (inlen > 4) { uint32_t id_; memcpy(&id_, in, 4); defSel(id_, (const char *)in + 4); }
        return NULL;
    }
    if (op == MCL_OP_BATCH) {
#ifdef MCL_LOOPBACK
        mcl_in_host = 1;
#endif
        void *r = doBatch((const uint8_t *)in, inlen, outlen);
        if (gStats) { gStatCalls++; gStatIn += inlen; statDump(); }
#ifdef MCL_LOOPBACK
        mcl_in_host = savedInHost;
#endif
        return r;
    }
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
