// Guest side of the Maclator Metal bridge (arm64, injected with DYLD_INSERT_LIBRARIES).
//
// Replaces MTLCreateSystemDefaultDevice/MTLCopyAllDevices with proxies of the host's real
// Metal objects. Every message sent to a proxy is marshalled (binary plist) and executed on
// the host GPU by libmclbridge (x86-64) through a Maclator trap; MTL* descriptor objects that
// live in the guest are serialized by property introspection. Compiled without ARC.
#import <Foundation/Foundation.h>
#import <objc/runtime.h>
#import <objc/message.h>
#include <pthread.h>
#include <dispatch/dispatch.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <dlfcn.h>
#include <unistd.h>
#include <stdatomic.h>
#include <mach/mach_time.h>
#include "mclproto.h"

#ifdef MCL_LOOPBACK
// Native test mode: the "host" is in the same process.
extern void *mcl_call(uint64_t op, const void *in, uint64_t inlen, uint64_t *outlen);
#endif

static Class gProxyClass;
static BOOL gStats, gNoFast, gNoAsync, gNoCache, gNoBatch, gOldDesc;
static void stats_atexit(void);
static NSMapTable *gProxies; // handle -> proxy (weak)
static NSLock *gProxyLock;
static BOOL gTrace;

static void mcl_init(void) {
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        gProxies = [[NSMapTable mapTableWithKeyOptions:NSPointerFunctionsOpaquePersonality | NSPointerFunctionsOpaqueMemory valueOptions:NSPointerFunctionsOpaquePersonality | NSPointerFunctionsOpaqueMemory] retain];
        gProxyLock = [NSLock new];
        gTrace = getenv("MCL_TRACE") != NULL;
        gStats = getenv("MCL_STATS") != NULL;
        gNoFast = getenv("MCL_NOFAST") != NULL;
        gNoAsync = getenv("MCL_NOASYNC") != NULL;
        gNoCache = getenv("MCL_NOCACHE") != NULL;
        gNoBatch = getenv("MCL_NOBATCH") != NULL;
        gOldDesc = getenv("MCL_OLDDESC") != NULL;
        gProxyClass = objc_getClass("MCLProxy");
        if (gStats) atexit(stats_atexit);
    });
}

static const char *skipQualifiers(const char *t) {
    while (*t == 'r' || *t == 'n' || *t == 'N' || *t == 'o' || *t == 'O' || *t == 'R' || *t == 'V') t++;
    return t;
}
static id NILV(void) { return @{@"z": @YES}; }

// ---- transport ----------------------------------------------------------------------
static void *mcl_raw(uint64_t op, const void *in, uint64_t len, uint64_t *outlen) {
#ifdef MCL_LOOPBACK
    return mcl_call(op, in, len, outlen);
#else
    register uint64_t x0 __asm__("x0") = op;
    register uint64_t x1 __asm__("x1") = (uint64_t)in;
    register uint64_t x2 __asm__("x2") = len;
    register uint64_t x16 __asm__("x16") = 0x4D43;
    __asm__ volatile("svc #0x80" : "+r"(x0), "+r"(x1) : "r"(x2), "r"(x16) : "memory", "cc");
    *outlen = x1;
    return (void *)x0;
#endif
}

static void tq_flush_current(void);

static NSDictionary *mcl_op(uint64_t op, NSDictionary *req) {
    mcl_init();
    NSData *d = nil;
    if (req) {
        tq_flush_current();   // queued (batched) commands must run before this synchronous request
        NSError *e = nil;
        d = [NSPropertyListSerialization dataWithPropertyList:req format:NSPropertyListBinaryFormat_v1_0 options:0 error:&e];
        if (!d) { fprintf(stderr, "[mclmetal] cannot serialize request: %s\n", e.description.UTF8String); return @{@"x": @"serialize"}; }
    }
    uint64_t outlen = 0;
    void *p = mcl_raw(op, d.bytes, d.length, &outlen);
    if (!p) return @{@"x": @"transport failure"};
    // Parsed binary plists may keep referencing the source bytes: parse from a private copy.
    NSData *o = [NSData dataWithBytes:p length:outlen];
    uint64_t dummy;
    mcl_raw(2, p, 0, &dummy);
    NSDictionary *reply = [NSPropertyListSerialization propertyListWithData:o options:NSPropertyListImmutable format:NULL error:NULL];
    return reply ?: @{@"x": @"bad reply"};
}

static NSDictionary *mcl_rpc(NSDictionary *req) { return mcl_op(1, req); }

// ---- proxy --------------------------------------------------------------------------
@interface MCLProxy : NSProxy {
@public
    uint64_t _h;
    NSArray *_chain;
    NSArray *_protos;
}
- (instancetype)initWithInfo:(NSDictionary *)info;
- (instancetype)initWithHandle:(uint64_t)h info:(NSDictionary *)info;
@end
static void tq_drop(uint64_t h);
static void classreg_note(NSDictionary *d);
static NSDictionary *classreg_get(uint32_t id);
static void *sigEncodingFor(SEL sel, const char *enc, NSString *cls);

static id decode(id v, BOOL retainedNew);
static id encodeObject(id o);
static NSData *descBinary(id o);
static id encodeBlock(id blk, NSString *selName);
static NSUInteger idArrayCount(NSInvocation *inv, NSMethodSignature *sig, NSArray *parts);
static void fastNoteResultClass(SEL sel, uint32_t cls);
static uint64_t gLegacyCalls;
static void legacy_note(SEL sel);
static NSMutableDictionary *gSigCache;
static NSLock *gSigLock;

static BOOL isNewFamily(const char *s) {
    while (*s == '_') s++;
    static const char *fam[] = {"new", "alloc", "copy", "mutableCopy"};
    for (int i = 0; i < 4; i++) {
        size_t l = strlen(fam[i]);
        if (strncmp(s, fam[i], l) == 0 && !(s[l] >= 'a' && s[l] <= 'z')) return YES;
    }
    return NO;
}


@implementation MCLProxy
- (instancetype)initWithInfo:(NSDictionary *)info {
    _h = [info[@"h"] unsignedLongLongValue];
    _chain = [info[@"c"] retain];
    _protos = [info[@"p"] retain];
    return self;
}
- (instancetype)initWithHandle:(uint64_t)h info:(NSDictionary *)info {
    _h = h;
    _chain = [info[@"c"] retain];
    _protos = [info[@"p"] retain];
    return self;
}
- (void)dealloc {
    [gProxyLock lock];
    if (NSMapGet(gProxies, (void *)_h) == (void *)self) NSMapRemove(gProxies, (void *)_h);
    [gProxyLock unlock];
    tq_drop(_h);
    [_chain release];
    [_protos release];
    [super dealloc];
}
- (NSMethodSignature *)methodSignatureForSelector:(SEL)sel {
    mcl_init();
    static dispatch_once_t once;
    dispatch_once(&once, ^{ gSigCache = [NSMutableDictionary new]; gSigLock = [NSLock new]; });
    NSString *key = [NSString stringWithFormat:@"%@|%s", _chain[0], sel_getName(sel)];
    [gSigLock lock];
    id cached = gSigCache[key];
    [gSigLock unlock];
    if (!cached) {
        NSDictionary *r = mcl_rpc(@{@"c": @"sig", @"h": @(_h), @"sel": @(sel_getName(sel))});
        cached = r[@"t"] ?: [NSNull null];
        [gSigLock lock];
        gSigCache[key] = cached;
        [gSigLock unlock];
    }
    if (cached == [NSNull null]) return nil;
    sigEncodingFor(sel, [cached UTF8String], _chain[0]);
    return [NSMethodSignature signatureWithObjCTypes:[cached UTF8String]];
}
- (BOOL)respondsToSelector:(SEL)sel { return [self methodSignatureForSelector:sel] != nil; }
- (BOOL)isKindOfClass:(Class)c { return [_chain containsObject:NSStringFromClass(c)]; }
- (BOOL)isMemberOfClass:(Class)c { return [_chain[0] isEqualToString:NSStringFromClass(c)]; }
- (BOOL)conformsToProtocol:(Protocol *)p { return [_protos containsObject:@(protocol_getName(p))]; }
- (NSString *)description {
    NSInvocation *inv = [NSInvocation invocationWithMethodSignature:[self methodSignatureForSelector:@selector(description)] ?: [NSMethodSignature signatureWithObjCTypes:"@16@0:8"]];
    inv.selector = @selector(description);
    [self forwardInvocation:inv];
    __unsafe_unretained id r = nil;
    [inv getReturnValue:&r];
    return r ?: [NSString stringWithFormat:@"<MCLProxy %llu %@>", _h, _chain[0]];
}
- (NSUInteger)hash { return (NSUInteger)_h; }

- (void)forwardInvocation:(NSInvocation *)inv {
    gLegacyCalls++;
    SEL sel = inv.selector;
    if (gStats) legacy_note(sel);
    NSMethodSignature *sig = inv.methodSignature;
    NSMutableArray *args = [NSMutableArray array];
    NSError **errOut = NULL;
    NSString *selName = @(sel_getName(sel));
    NSArray *parts = [selName componentsSeparatedByString:@":"];
    for (NSUInteger i = 2; i < sig.numberOfArguments; i++) {
        const char *t = [sig getArgumentTypeAtIndex:i];
        const char *b = skipQualifiers(t);
        NSUInteger sz = 0;
        NSGetSizeAndAlignment(t, &sz, NULL);
        char *buf = calloc(1, sz + 8);
        [inv getArgument:buf atIndex:i];
        if (b[0] == '@' && b[1] == '?') {
            [args addObject:encodeBlock(*(id *)buf, selName)];
        } else if (b[0] == '@') {
            [args addObject:encodeObject(*(id *)buf)];
        } else if (b[0] == '^' && b[1] == '@') {
            void *ptr = *(void **)buf;
            if (t[0] == 'r') {
                NSUInteger count = idArrayCount(inv, sig, parts);
                NSMutableArray *ids = [NSMutableArray array];
                for (NSUInteger k = 0; k < count && ptr; k++) [ids addObject:encodeObject(((id *)ptr)[k])];
                [args addObject:@{@"ids": ids}];
            } else {
                errOut = (NSError **)ptr;
                [args addObject:@{@"outerr": @YES}];
            }
        } else if (strncmp(b, "^{__IOSurface", 13) == 0) {
            // IOSurfaceRef is a guest-side CF object; the host looks the surface up by its global id.
            static uint32_t (*getID)(void *);
            static dispatch_once_t once;
            dispatch_once(&once, ^{ getID = dlsym(RTLD_DEFAULT, "IOSurfaceGetID"); });
            void *surf = *(void **)buf;
            uint32_t sid = (surf && getID) ? getID(surf) : 0;
            [args addObject:@{@"iosurf": @(sid)}];
        } else {
            [args addObject:[NSData dataWithBytes:buf length:sz]];
        }
        free(buf);
    }
    if (gTrace) fprintf(stderr, "[mclmetal] -> %s %s\n", [_chain[0] UTF8String], sel_getName(sel));
    NSDictionary *reply = mcl_rpc(@{@"c": @"msg", @"h": @(_h), @"sel": selName, @"args": args});
    if (reply[@"x"]) {
        [NSException raise:@"MCLRemoteException" format:@"%@", reply[@"x"]];
    }
    if (errOut && reply[@"err"]) {
        NSDictionary *e = reply[@"err"][@"e"];
        *errOut = [NSError errorWithDomain:e[@"d"] code:[e[@"k"] integerValue] userInfo:@{NSLocalizedDescriptionKey: e[@"m"] ?: @""}];
    }
    const char *rt = skipQualifiers([sig methodReturnType]);
    if (rt[0] == 'v') return;
    id r = reply[@"r"];
    if (rt[0] == '@') {
        BOOL nw = isNewFamily(sel_getName(sel));
        if ([r isKindOfClass:[NSDictionary class]] && r[@"h"] && r[@"i"]) fastNoteResultClass(sel, [r[@"i"] unsignedIntValue]);
        id o = decode(r, nw);
        [inv setReturnValue:&o];
    } else {
        NSUInteger sz = 0;
        NSGetSizeAndAlignment(rt, &sz, NULL);
        char *buf = calloc(1, sz + 8);
        memcpy(buf, [(NSData *)r bytes], MIN(sz, [(NSData *)r length]));
        [inv setReturnValue:buf];
        free(buf);
    }
}
@end


// ---- descriptors: serialize guest-side MTL*Descriptor objects by introspection -----------
static BOOL isDescriptorClass(id o) {
    const char *n = object_getClassName(o);
    return strncmp(n, "MTL", 3) == 0 || strncmp(n, "CAMetal", 7) == 0;
}

static NSDictionary *describe(id o) {
    NSMutableDictionary *d = [NSMutableDictionary dictionary];
    const char *cn = object_getClassName(o);
    d[@"$d"] = @(cn);
    if ([o respondsToSelector:@selector(objectAtIndexedSubscript:)]) {
        NSUInteger n = strstr(cn, "SampleBuffer") ? 0 : strstr(cn, "ColorAttachment") ? 8 : 31;
        NSMutableArray *a = [NSMutableArray array];
        for (NSUInteger i = 0; i < n; i++) {
            id e = ((id(*)(id, SEL, NSUInteger))objc_msgSend)(o, @selector(objectAtIndexedSubscript:), i);
            [a addObject:e ? encodeObject(e) : NILV()];
        }
        d[@"a"] = a;
        return d;
    }
    NSMutableArray *props = [NSMutableArray array];
    NSMutableSet *seen = [NSMutableSet set];
    for (Class c = object_getClass(o); c && c != [NSObject class]; c = class_getSuperclass(c)) {
        unsigned n = 0;
        objc_property_t *pl = class_copyPropertyList(c, &n);
        for (unsigned i = 0; i < n; i++) {
            const char *pn = property_getName(pl[i]);
            NSString *name = @(pn);
            if ([seen containsObject:name]) continue;
            [seen addObject:name];
            char *G = property_copyAttributeValue(pl[i], "G");
            char *S = property_copyAttributeValue(pl[i], "S");
            char *R = property_copyAttributeValue(pl[i], "R");
            BOOL ro = R != NULL;
            NSString *getter = @(G ?: pn);
            NSString *setter = @"";
            if (!ro) {
                if (S) setter = @(S);
                else setter = [NSString stringWithFormat:@"set%c%s:", toupper(pn[0]), pn + 1];
            }
            free(G); free(S); free(R);
            SEL gs = NSSelectorFromString(getter);
            if (![o respondsToSelector:gs]) continue;
            NSMethodSignature *sig = [o methodSignatureForSelector:gs];
            if (!sig || sig.numberOfArguments != 2) continue;
            const char *rt = skipQualifiers([sig methodReturnType]);
            id enc = nil;
            @try {
                NSInvocation *inv = [NSInvocation invocationWithMethodSignature:sig];
                inv.target = o;
                inv.selector = gs;
                [inv invoke];
                if (rt[0] == '@') {
                    __unsafe_unretained id v = nil;
                    [inv getReturnValue:&v];
                    if (!v) continue;
                    enc = encodeObject(v);
                    if (ro && !(([enc isKindOfClass:[NSDictionary class]]) && enc[@"$d"])) continue;
                } else {
                    if (ro) continue;
                    NSUInteger sz = 0;
                    NSGetSizeAndAlignment(rt, &sz, NULL);
                    NSMutableData *bytes = [NSMutableData dataWithLength:sz];
                    [inv getReturnValue:bytes.mutableBytes];
                    enc = bytes;
                }
            } @catch (NSException *e) {
                continue;
            }
            [props addObject:@{@"n": name, @"g": getter, @"s": setter, @"t": @(rt), @"v": enc}];
        }
        free(pl);
    }
    d[@"p"] = props;
    return d;
}

static void dispatch_release_compat(dispatch_data_t d) { [(id)d release]; }

static id encodeObject(id o) {
    if (!o) return NILV();
    if (object_getClass(o) == [MCLProxy class]) return @{@"h": @(((MCLProxy *)o)->_h)};
    if ([o isKindOfClass:NSClassFromString(@"OS_dispatch_data")]) {
        const void *p = NULL;
        size_t n = 0;
        dispatch_data_t m = dispatch_data_create_map((dispatch_data_t)o, &p, &n);
        NSData *d = [NSData dataWithBytes:p length:n];
        dispatch_release_compat(m);
        return @{@"dd": d};
    }
    if ([o isKindOfClass:[NSString class]] || [o isKindOfClass:[NSNumber class]] || [o isKindOfClass:[NSData class]]) return o;
    if ([o isKindOfClass:[NSArray class]]) {
        NSMutableArray *a = [NSMutableArray array];
        for (id e in (NSArray *)o) [a addObject:encodeObject(e)];
        return a;
    }
    if ([o isKindOfClass:[NSDictionary class]]) {
        NSMutableDictionary *m = [NSMutableDictionary dictionary];
        for (id k in (NSDictionary *)o) m[k] = encodeObject(((NSDictionary *)o)[k]);
        return m;
    }
    if ([o isKindOfClass:[NSURL class]]) return @{@"url": [(NSURL *)o absoluteString]};
    if (isDescriptorClass(o)) return gOldDesc ? describe(o) : @{@"dbin": descBinary(o)};
    return @{@"u": @(object_getClassName(o))};
}

// ---- blocks (completion handlers): invoked later from a pump thread ----------------------
static NSMutableDictionary *gBlocks;
static NSLock *gBlockLock;
static uint64_t gNextBlock = 1;
static void *pump(void *arg);
static _Atomic uint64_t gEvHandled;

struct BlockLayout { void *isa; int flags; int reserved; void *invoke; struct { unsigned long reserved, size; void *rest[]; } *desc; };

static NSArray *blockArgTypes(id blk) {
    struct BlockLayout *b = (struct BlockLayout *)blk;
    if (b->flags & (1 << 30)) {
        const char *sig = (const char *)((b->flags & (1 << 25)) ? b->desc->rest[2] : b->desc->rest[0]);
        if (sig) {
            NSMethodSignature *ms = [NSMethodSignature signatureWithObjCTypes:sig];
            NSMutableArray *a = [NSMutableArray array];
            for (NSUInteger i = 1; i < ms.numberOfArguments; i++) [a addObject:@([ms getArgumentTypeAtIndex:i])];
            return a;
        }
    }
    return @[@"@", @"@"];
}

static id encodeBlock(id blk, NSString *selName) {
    if (!blk) return NILV();
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        gBlocks = [NSMutableDictionary new];
        gBlockLock = [NSLock new];
        pthread_t th;
        pthread_attr_t at;
        pthread_attr_init(&at);
        pthread_attr_setstacksize(&at, 1 << 20);
        pthread_create(&th, &at, pump, NULL);
        pthread_detach(th);
    });
    id copy = [blk copy];
    [gBlockLock lock];
    uint64_t bid = gNextBlock++;
    BOOL once_ = [selName rangeOfString:@"Handler"].location != NSNotFound;
    gBlocks[@(bid)] = @{@"b": copy, @"once": @(once_)};
    [copy release];
    [gBlockLock unlock];
    return @{@"blk": @(bid), @"sig": blockArgTypes(blk)};
}

static void *pump(void *arg) {
    for (;;) {
        @autoreleasepool {
            NSDictionary *ev = mcl_op(3, nil);
            if (!ev[@"blk"]) continue;
            [gBlockLock lock];
            NSDictionary *ent = [[gBlocks[ev[@"blk"]] retain] autorelease]; // survive removal below
            if ([ent[@"once"] boolValue]) [gBlocks removeObjectForKey:ev[@"blk"]];
            [gBlockLock unlock];
            if (!ent) { atomic_fetch_add(&gEvHandled, 1); continue; }
            void *a[4] = {0, 0, 0, 0};
            NSMutableArray *keep = [NSMutableArray array];
            NSArray *args = ev[@"args"];
            for (NSUInteger i = 0; i < args.count && i < 4; i++) {
                id v = args[i];
                if ([v isKindOfClass:[NSData class]]) {
                    uint64_t x = 0;
                    memcpy(&x, [v bytes], MIN(8, [v length]));
                    a[i] = (void *)x;
                } else {
                    id o = decode(v, NO);
                    if (o) [keep addObject:o];
                    a[i] = o;
                }
            }
            void (^b)(void *, void *, void *, void *) = (void (^)(void *, void *, void *, void *))ent[@"b"];
            b(a[0], a[1], a[2], a[3]);
            atomic_fetch_add(&gEvHandled, 1);
        }
    }
    return NULL;
}

// Number of elements of a `const id *` argument, derived from the selector's shape.
static NSUInteger idArrayCount(NSInvocation *inv, NSMethodSignature *sig, NSArray *parts) {
    for (NSUInteger i = 2; i < sig.numberOfArguments; i++) {
        const char *t = [sig getArgumentTypeAtIndex:i];
        if (strncmp(t, "{_NSRange", 9) == 0) {
            NSRange r;
            [inv getArgument:&r atIndex:i];
            return r.length;
        }
        NSString *part = i - 2 < parts.count ? parts[i - 2] : @"";
        if ([part hasSuffix:@"count"] && (t[0] == 'Q' || t[0] == 'q')) {
            NSUInteger n = 0;
            [inv getArgument:&n atIndex:i];
            return n;
        }
    }
    return 0;
}

static id decode(id v, BOOL nw) {
    if ([v isKindOfClass:[NSArray class]]) {
        NSMutableArray *a = [NSMutableArray array];
        for (id e in (NSArray *)v) { id x = decode(e, NO); [a addObject:x ?: [NSNull null]]; }
        return a;
    }
    if (![v isKindOfClass:[NSDictionary class]]) return v;
    NSDictionary *d = v;
    if (d[@"z"]) return nil;
    if (d[@"e"]) {
        NSDictionary *e = d[@"e"];
        return [NSError errorWithDomain:e[@"d"] code:[e[@"k"] integerValue] userInfo:@{NSLocalizedDescriptionKey: e[@"m"] ?: @"", NSLocalizedFailureReasonErrorKey: e[@"f"] ?: @""}];
    }
    if (d[@"h"]) {
        mcl_init();
        classreg_note(d);
        uint64_t h = [d[@"h"] unsignedLongLongValue];
        [gProxyLock lock];
        MCLProxy *p = NSMapGet(gProxies, (void *)h);
        if (p) {
            [p retain];
            [gProxyLock unlock];
            // The host counted this mention; give the extra reference back.
            tq_drop(h);
            return nw ? p : [p autorelease];
        }
        p = [MCLProxy alloc];
        [p initWithInfo:d];
        NSMapInsert(gProxies, (void *)h, p);
        [gProxyLock unlock];
        return nw ? p : [p autorelease];
    }
    return d;
}


// ======================= fast path ====================================================================
// Selectors whose signature is simple enough (register-passed arguments, void/scalar/id result) get a
// real method on MCLProxy: the assembly trampoline below saves the argument registers and calls
// mcl_fast_dispatch, which appends a compact binary record to this thread's command stream. Void calls
// return immediately (the stream is sent in one trap at commit / on the next synchronous call); calls
// with results are sent right away; a few object-creating calls (command buffers, encoders) get a
// guest-chosen handle and stay asynchronous too. Everything else still goes through forwardInvocation:.
typedef struct { uint8_t cls, xi, di, nmem, msz, cntArg, cntKind, pad; uint16_t size, elem, soff; } PArg;
enum { AK_INT, AK_FP, AK_HFA, AK_SREG, AK_SREF, AK_ID, AK_IDARR, AK_DATA, AK_BLOCK, AK_STK };
enum { RK_VOID, RK_INT, RK_F32, RK_F64, RK_ID };

typedef struct Plan {
    SEL sel; struct Plan *next;
    uint32_t selid;
    uint8_t eligible, nargs, retKind, retSize, isNew, forceSync, flushAfter, asyncOK, cacheable, dead, isCommit;
    uint32_t resClass;
    PArg a[10];
    uint64_t nCalls, nNs, nSync;
    char *name, *enc;
} Plan;

#define PLAN_TAB 4096
static Plan *gPlanTab[PLAN_TAB];
static pthread_mutex_t gPlanLock = PTHREAD_MUTEX_INITIALIZER;
static Plan *gPlanList[PLAN_TAB];
static unsigned gNPlans;
static uint32_t gNextSelId = 1;
static uint64_t gNBatched, gNSync, gNAsync, gNFlush, gNBytes, gNCommit, gNCacheHit;
static uint64_t gLastDump;
extern void mcl_tramp(void);
extern void _objc_msgForward(void);

static Plan *plan_find(SEL sel) {
    Plan *p = __atomic_load_n(&gPlanTab[((uintptr_t)sel >> 3) & (PLAN_TAB - 1)], __ATOMIC_ACQUIRE);
    for (; p; p = p->next) if (p->sel == sel) return p;
    return NULL;
}

static NSMutableDictionary *gClassReg;
static void classreg_note(NSDictionary *d) {
    id i = d[@"i"];
    if (!i || !d[@"c"]) return;
    @synchronized([MCLProxy class]) {
        if (!gClassReg) gClassReg = [NSMutableDictionary new];
        if (!gClassReg[i]) gClassReg[i] = @{@"c": d[@"c"], @"p": d[@"p"] ?: @[]};
    }
}
static NSDictionary *classreg_get(uint32_t id_) {
    @synchronized([MCLProxy class]) { return gClassReg[@(id_)]; }
}

// ---- command stream --------------------------------------------------------------------------------------
// Each thread encodes a record into its own scratch buffer (TQ), then appends it to the one global,
// ordered queue G under a short lock. Ordering across threads therefore stays exactly the order in
// which the calls returned. Waits that can block on other GPU work are sent outside the lock.
typedef struct { uint8_t *buf; uint32_t len, cap; uint8_t busy; uint64_t *defer; uint32_t nDefer, capDefer; } TQ;
static struct { pthread_mutex_t lock; uint8_t *buf; uint32_t len, cap, ndrop; } G = {PTHREAD_MUTEX_INITIALIZER, NULL, 0, 0, 0};
static pthread_key_t gTQKey;
static pthread_once_t gTQOnce = PTHREAD_ONCE_INIT;
static void tq_destroy(void *p) {
    TQ *q = p;
    free(q->buf);
    free(q->defer);
    free(q);
}
static void tq_once(void) { pthread_key_create(&gTQKey, tq_destroy); }
static TQ *tq(void) {
    pthread_once(&gTQOnce, tq_once);
    TQ *q = pthread_getspecific(gTQKey);
    if (!q) {
        q = calloc(1, sizeof *q);
        q->cap = 1 << 14;
        q->buf = malloc(q->cap);
        q->len = 16;
        pthread_setspecific(gTQKey, q);
    }
    return q;
}
static inline void qneed(TQ *q, size_t n) {
    if (q->len + n > q->cap) {
        while (q->len + n > q->cap) q->cap *= 2;
        q->buf = realloc(q->buf, q->cap);
    }
}
static inline void q64(TQ *q, uint64_t v) { qneed(q, 8); memcpy(q->buf + q->len, &v, 8); q->len += 8; }
static inline void qbytes(TQ *q, const void *p, size_t n) {
    size_t pn = (n + 7) & ~(size_t)7;
    qneed(q, pn);
    if (n) memcpy(q->buf + q->len, p, n);
    if (pn > n) memset(q->buf + q->len + n, 0, pn - n);
    q->len += pn;
}
static inline void qarg(TQ *q, uint8_t tag, const void *p, uint32_t n) {
    q64(q, (uint64_t)tag | ((uint64_t)n << 32));
    qbytes(q, p, n);
}
static uint32_t rec_begin(TQ *q, uint8_t kind, uint32_t selid, uint64_t target, uint64_t result) {
    uint32_t off = q->len;
    q64(q, (uint64_t)kind << 32);   // len / nargs patched in rec_end
    q64(q, selid);
    q64(q, target);
    q64(q, result);
    return off;
}
static void rec_end(TQ *q, uint32_t off, uint8_t nargs) {
    uint64_t w0;
    memcpy(&w0, q->buf + off, 8);
    w0 |= (uint64_t)(q->len - off) | ((uint64_t)nargs << 40);
    memcpy(q->buf + off, &w0, 8);
}

static void stats_maybe_dump(void);
static void stats_dump_force(void);
static void g_append_locked(const uint8_t *p, uint32_t n) {
    if (!G.buf) { G.cap = 1 << 18; G.buf = malloc(G.cap); G.len = 16; }
    while (G.len + n > G.cap) { G.cap *= 2; G.buf = realloc(G.buf, G.cap); }
    memcpy(G.buf + G.len, p, n);
    G.len += n;
}
// Sends the global queue. Caller holds G.lock. Returns the host's reply for a trailing SYNC record.
static void *g_flush_locked(uint64_t *ol) {
    uint64_t dummy = 0;
    if (ol) *ol = 0;
    if (!G.buf || G.len <= 16) return NULL;
    MclMsgHdr h = {MCL_MAGIC, G.len - 16, 0, 0};
    memcpy(G.buf, &h, sizeof h);
    gNFlush++;
    gNBytes += G.len;
    void *r = mcl_raw(MCL_OP_BATCH, G.buf, G.len, ol ?: &dummy);
    G.len = 16;
    G.ndrop = 0;
    return r;
}
static void tq_flush_current(void) {
    pthread_mutex_lock(&G.lock);
    g_flush_locked(NULL);
    pthread_mutex_unlock(&G.lock);
    if (gStats) stats_maybe_dump();
}
static void drop_locked(uint64_t h) {
    uint64_t rec[4] = {32 | ((uint64_t)MCL_R_DROP << 32), 0, h, 0};
    g_append_locked((uint8_t *)rec, 32);
    if (++G.ndrop >= 512) g_flush_locked(NULL);
}
static void tq_drop(uint64_t h) {
    if (gNoFast) return;
    TQ *q = tq();
    if (q->busy) {   // a record is being built on this thread: append the drop after it
        if (q->nDefer == q->capDefer) { q->capDefer = q->capDefer ? q->capDefer * 2 : 16; q->defer = realloc(q->defer, q->capDefer * 8); }
        q->defer[q->nDefer++] = h;
        return;
    }
    pthread_mutex_lock(&G.lock);
    drop_locked(h);
    pthread_mutex_unlock(&G.lock);
}

// ---- binary descriptors ---------------------------------------------------------------------------------------
// Guest-side MTL*Descriptor objects are described once per class (schema: property getters/setters and the
// values of a pristine instance) and then sent as "property index + raw value" for the properties that differ
// from the defaults, nested objects included. The host checks its own defaults when the schema is defined and
// asks for any property that differs to be sent unconditionally.
enum { DK_INT, DK_F32, DK_F64, DK_C4, DK_GEN, DK_OBJ };
typedef struct { SEL get; char *name, *getName, *setName, *typeStr; uint8_t kind, size, ro, force; uint16_t defOff; } DProp;
typedef struct DSchema { Class cls; struct DSchema *next; uint16_t id; uint8_t indexed; int nIdx; unsigned nprops, defLen; DProp *props; uint8_t *def; } DSchema;
#define DS_TAB 256
static DSchema *gDS[DS_TAB];
static pthread_mutex_t gDSLock = PTHREAD_MUTEX_INITIALIZER;
static uint16_t gNextSchema = 1;

static inline void dput(TQ *q, const void *p, size_t n) {
    qneed(q, n);
    memcpy(q->buf + q->len, p, n);
    q->len += n;
}
static inline void dput8(TQ *q, uint8_t v) { dput(q, &v, 1); }
static inline void dput16(TQ *q, uint16_t v) { dput(q, &v, 2); }
static inline void dput32(TQ *q, uint32_t v) { dput(q, &v, 4); }
static inline void dput64(TQ *q, uint64_t v) { dput(q, &v, 8); }

struct C4 { double a, b, c, d; };

static void readProp(id o, DProp *p, void *out) {   // out has room for p->size bytes
    switch (p->kind) {
    case DK_INT: {
        uint64_t v = ((uint64_t(*)(id, SEL))objc_msgSend)(o, p->get);
        memcpy(out, &v, p->size);
        break;
    }
    case DK_F32: { float v = ((float(*)(id, SEL))objc_msgSend)(o, p->get); memcpy(out, &v, 4); break; }
    case DK_F64: { double v = ((double(*)(id, SEL))objc_msgSend)(o, p->get); memcpy(out, &v, 8); break; }
    case DK_C4: { struct C4 v = ((struct C4(*)(id, SEL))objc_msgSend)(o, p->get); memcpy(out, &v, 32); break; }
    default: {
        NSMethodSignature *sig = [o methodSignatureForSelector:p->get];
        NSInvocation *inv = [NSInvocation invocationWithMethodSignature:sig];
        inv.target = o;
        inv.selector = p->get;
        [inv invoke];
        [inv getReturnValue:out];
    }
    }
}

static DSchema *ds_find(Class c) {
    for (DSchema *s = gDS[((uintptr_t)c >> 4) & (DS_TAB - 1)]; s; s = s->next) if (s->cls == c) return s;
    return NULL;
}

static DSchema *ds_build(id o) {
    Class c = object_getClass(o);
    pthread_mutex_lock(&gDSLock);
    DSchema *sc = ds_find(c);
    if (sc) { pthread_mutex_unlock(&gDSLock); return sc; }
    sc = calloc(1, sizeof *sc);
    sc->cls = c;
    const char *cn = class_getName(c);
    if ([o respondsToSelector:@selector(objectAtIndexedSubscript:)]) {
        sc->indexed = 1;
        sc->nIdx = strstr(cn, "SampleBuffer") ? 0 : strstr(cn, "ColorAttachment") ? 8 : 31;
    } else {
        NSMutableSet *seen = [NSMutableSet set];
        unsigned cap = 0, n = 0;
        DProp *props = NULL;
        id pristine = [[c alloc] init];
        for (Class k = c; k && k != [NSObject class]; k = class_getSuperclass(k)) {
            unsigned cnt = 0;
            objc_property_t *pl = class_copyPropertyList(k, &cnt);
            for (unsigned i = 0; i < cnt; i++) {
                const char *pn = property_getName(pl[i]);
                NSString *name = @(pn);
                if ([seen containsObject:name]) continue;
                [seen addObject:name];
                char *G = property_copyAttributeValue(pl[i], "G");
                char *S = property_copyAttributeValue(pl[i], "S");
                char *R = property_copyAttributeValue(pl[i], "R");
                BOOL ro = R != NULL;
                NSString *getter = @(G ?: pn);
                NSString *setter = @"";
                if (!ro) setter = S ? @(S) : [NSString stringWithFormat:@"set%c%s:", toupper(pn[0]), pn + 1];
                free(G); free(S); free(R);
                SEL gs = NSSelectorFromString(getter);
                if (![pristine respondsToSelector:gs]) continue;
                NSMethodSignature *sig = [pristine methodSignatureForSelector:gs];
                if (!sig || sig.numberOfArguments != 2) continue;
                const char *rt = skipQualifiers([sig methodReturnType]);
                DProp p = {0};
                p.get = gs; p.ro = ro;
                p.name = strdup(pn); p.getName = strdup(getter.UTF8String); p.setName = strdup(setter.UTF8String); p.typeStr = strdup(rt);
                NSUInteger sz = 0;
                NSGetSizeAndAlignment(rt, &sz, NULL);
                p.size = (uint8_t)sz;
                if (rt[0] == '@') p.kind = DK_OBJ;
                else if (ro) continue;
                else if (rt[0] == 'f') p.kind = DK_F32;
                else if (rt[0] == 'd') p.kind = DK_F64;
                else if (rt[0] == '{' && sz == 32 && strncmp(rt, "{?=dddd}", 8) == 0) p.kind = DK_C4;
                else if (strchr("cCsSiIlLqQB", rt[0])) p.kind = DK_INT;
                else if (sz > 64) continue;
                else p.kind = DK_GEN;
                if (n == cap) { cap = cap ? cap * 2 : 32; props = realloc(props, cap * sizeof *props); }
                props[n++] = p;
            }
            free(pl);
        }
        sc->props = props;
        sc->nprops = n;
        // defaults of a pristine instance
        unsigned total = 0;
        for (unsigned i = 0; i < n; i++) if (props[i].kind != DK_OBJ) { props[i].defOff = total; total += props[i].size; }
        sc->def = calloc(1, total ? total : 1);
        sc->defLen = total;
        for (unsigned i = 0; i < n; i++) {
            if (props[i].kind == DK_OBJ) continue;
            @try { readProp(pristine, &props[i], sc->def + props[i].defOff); } @catch (NSException *e) {}
        }
        [pristine release];
    }
    sc->id = gNextSchema++;
    // define on the host; it names the properties whose defaults differ from ours
    NSMutableArray *pa = [NSMutableArray array];
    for (unsigned i = 0; i < sc->nprops; i++) {
        DProp *p = &sc->props[i];
        [pa addObject:@{@"n": @(p->name), @"g": @(p->getName), @"s": @(p->setName), @"t": @(p->typeStr), @"k": @(p->kind), @"z": @(p->size)}];
    }
    NSDictionary *req = @{@"c": @"schema", @"id": @(sc->id), @"cls": @(class_getName(c)), @"idx": @(sc->indexed), @"n": @(sc->nIdx), @"props": pa,
                          @"def": sc->def ? [NSData dataWithBytes:sc->def length:sc->defLen] : [NSData data]};
    NSDictionary *rep = mcl_rpc(req);
    for (NSNumber *f in rep[@"force"]) if (f.unsignedIntValue < sc->nprops) sc->props[f.unsignedIntValue].force = 1;
    unsigned slot = ((uintptr_t)c >> 4) & (DS_TAB - 1);
    sc->next = gDS[slot];
    __atomic_store_n(&gDS[slot], sc, __ATOMIC_RELEASE);
    pthread_mutex_unlock(&gDSLock);
    return sc;
}

static unsigned desc_emit(TQ *q, id o);   // returns the number of entries written

// object-valued property: 0 = nothing to send
static BOOL desc_emit_obj(TQ *q, id v) {
    if (!v) return NO;
    Class c = object_getClass(v);
    if (c == gProxyClass) { dput8(q, 1); dput64(q, ((MCLProxy *)v)->_h); return YES; }
    if (isDescriptorClass(v)) {
        uint32_t mark = q->len;
        dput8(q, 2);
        if (desc_emit(q, v) == 0) { q->len = mark; return NO; }
        return YES;
    }
    @autoreleasepool {
        if ([v isKindOfClass:[NSString class]]) {
            const char *u = [(NSString *)v UTF8String];
            dput8(q, 4); dput32(q, (uint32_t)strlen(u)); dput(q, u, strlen(u));
            return YES;
        }
        NSData *d = [NSPropertyListSerialization dataWithPropertyList:encodeObject(v) format:NSPropertyListBinaryFormat_v1_0 options:0 error:NULL];
        if (!d) return NO;
        dput8(q, 3); dput32(q, (uint32_t)d.length); dput(q, d.bytes, d.length);
        return YES;
    }
}

static unsigned desc_emit(TQ *q, id o) {
    DSchema *sc = ds_find(object_getClass(o));
    if (!sc) sc = ds_build(o);
    dput16(q, sc->id);
    uint32_t cntAt = q->len;
    unsigned n = 0;
    if (sc->indexed) {
        dput8(q, 0);
        for (int i = 0; i < sc->nIdx; i++) {
            id e = ((id(*)(id, SEL, NSUInteger))objc_msgSend)(o, @selector(objectAtIndexedSubscript:), i);
            if (!e) continue;
            uint32_t mark = q->len;
            dput8(q, (uint8_t)i);
            if (desc_emit(q, e) == 0) q->len = mark; else n++;
        }
        q->buf[cntAt] = (uint8_t)n;
    } else {
        dput16(q, 0);
        for (unsigned i = 0; i < sc->nprops; i++) {
            DProp *p = &sc->props[i];
            uint32_t mark = q->len;
            dput16(q, (uint16_t)i);
            if (p->kind == DK_OBJ) {
                id v = ((id(*)(id, SEL))objc_msgSend)(o, p->get);
                if (p->ro && !(v && isDescriptorClass(v))) { q->len = mark; continue; }
                if (desc_emit_obj(q, v)) n++; else q->len = mark;
            } else {
                uint8_t cur[64];
                readProp(o, p, cur);
                if (!p->force && memcmp(cur, sc->def + p->defOff, p->size) == 0) { q->len = mark; continue; }
                dput(q, cur, p->size);
                n++;
            }
        }
        uint16_t n16 = (uint16_t)n;
        memcpy(q->buf + cntAt, &n16, 2);
    }
    return n;
}

static NSData *descBinary(id o) {
    TQ t = {0};
    t.cap = 4096;
    t.buf = malloc(t.cap);
    @try { desc_emit(&t, o); } @catch (NSException *e) { fprintf(stderr, "[mclmetal] descriptor %s: %s\n", object_getClassName(o), e.reason.UTF8String); }
    NSData *d = [NSData dataWithBytes:t.buf length:t.len];
    free(t.buf);
    return d;
}

// ---- plans ----------------------------------------------------------------------------------------------
static int hfaLeaves(const char **pp, int *n, int *msz) {
    const char *p = *pp + 1;
    while (*p && *p != '=' && *p != '}') p++;
    if (*p == '=') p++;
    while (*p && *p != '}') {
        if (*p == '"') { p++; while (*p && *p != '"') p++; if (*p) p++; continue; }
        if (*p == '{') { if (!hfaLeaves(&p, n, msz)) return 0; continue; }
        if (*p == 'f' || *p == 'd') {
            int m = *p == 'f' ? 4 : 8;
            if (*msz && *msz != m) return 0;
            *msz = m; (*n)++; p++;
        } else return 0;
    }
    if (*p == '}') p++;
    *pp = p;
    return 1;
}
static BOOL nameHas(const char *n, const char *const *list) {
    for (; *list; list++) if (strcmp(n, *list) == 0) return YES;
    return NO;
}
static BOOL labelIsCount(NSString *l) {
    return [l hasSuffix:@"count"] || [l hasSuffix:@"Count"] || [l isEqualToString:@"length"];
}

static Plan *buildPlan(SEL sel, const char *enc) {
    NSMethodSignature *sig = [NSMethodSignature signatureWithObjCTypes:enc];
    const char *name = sel_getName(sel);
    Plan *pl = calloc(1, sizeof *pl);
    pl->sel = sel;
    pl->name = strdup(name);
    pl->enc = strdup(enc);
    NSUInteger na = sig.numberOfArguments;
    if (na < 2 || na - 2 > 10) return pl;
    pl->nargs = (uint8_t)(na - 2);
    const char *rt = skipQualifiers([sig methodReturnType]);
    NSUInteger rsz = 0;
    NSGetSizeAndAlignment(rt, &rsz, NULL);
    switch (rt[0]) {
    case 'v': pl->retKind = RK_VOID; break;
    case 'f': pl->retKind = RK_F32; pl->retSize = 4; break;
    case 'd': pl->retKind = RK_F64; pl->retSize = 8; break;
    case '@': pl->retKind = RK_ID; pl->retSize = 8; break;
    case 'c': case 'C': case 's': case 'S': case 'i': case 'I': case 'l': case 'L': case 'q': case 'Q':
    case 'B': case '^': case '*': case ':': case '#':
        pl->retKind = RK_INT; pl->retSize = (uint8_t)rsz; break;
    default: return pl;
    }
    NSArray *parts = [@(name) componentsSeparatedByString:@":"];
    const char *types[10];
    int xi = 2, di = 0;
    unsigned soff = 0;   // bytes of stack arguments (Apple arm64: packed, naturally aligned)
    for (int i = 0; i < pl->nargs; i++) {
        const char *t = [sig getArgumentTypeAtIndex:i + 2];
        const char *b = skipQualifiers(t);
        BOOL isConst = NO;
        for (const char *q = t; q < b; q++) if (*q == 'r') isConst = YES;
        types[i] = b;
        NSUInteger sz = 0;
        NSGetSizeAndAlignment(b, &sz, NULL);
        PArg *a = &pl->a[i];
        a->size = (uint16_t)sz;
        a->cntArg = 0xff;
        switch (b[0]) {
        case '@':
            if (b[1] == '?') { a->cls = AK_BLOCK; a->xi = xi++; break; }
            a->cls = AK_ID; a->xi = xi++; break;
        case 'c': case 'C': case 's': case 'S': case 'i': case 'I': case 'l': case 'L': case 'q': case 'Q':
        case 'B': case ':': case '#':
            if (xi >= 8) { a->cls = AK_STK; soff = (soff + sz - 1) & ~(sz - 1); a->soff = soff; soff += sz; break; }
            a->cls = AK_INT; a->xi = xi++; break;
        case 'f': case 'd':
            if (di >= 8) { a->cls = AK_STK; soff = (soff + sz - 1) & ~(sz - 1); a->soff = soff; soff += sz; break; }
            a->cls = AK_FP; a->di = di++; break;
        case '{': {
            int n = 0, m = 0;
            const char *pp = b;
            if (hfaLeaves(&pp, &n, &m) && n >= 1 && n <= 4) { a->cls = AK_HFA; a->di = di; a->nmem = n; a->msz = m; di += n; }
            else if (sz > 16) { a->cls = AK_SREF; a->xi = xi++; }
            else { a->cls = AK_SREG; a->xi = xi; xi += (sz + 7) / 8; }
            break;
        }
        case '^': case '*': {
            if (!isConst || b[0] == '*') return pl;
            a->xi = xi++;
            if (b[1] == '@') { a->cls = AK_IDARR; a->elem = 8; }
            else if (b[1] == 'v') { a->cls = AK_DATA; a->elem = 1; }
            else {
                NSUInteger es = 0;
                NSGetSizeAndAlignment(b + 1, &es, NULL);
                if (!es) return pl;
                a->cls = AK_DATA; a->elem = (uint16_t)es;
            }
            break;
        }
        default: return pl;
        }
    }
    if (xi > 8 || di > 8 || soff > 64) return pl;
    for (int i = 0; i < pl->nargs; i++) {
        PArg *a = &pl->a[i];
        if (a->cls != AK_IDARR && a->cls != AK_DATA) continue;
        for (int j = 0; j < pl->nargs; j++)
            if (j != i && pl->a[j].cls == AK_SREG && pl->a[j].size == 16 && strncmp(types[j], "{_NSRange", 9) == 0) { a->cntArg = j; a->cntKind = 1; break; }
        if (a->cntArg == 0xff)
            for (int j = 0; j < pl->nargs; j++)
                if (j != i && pl->a[j].cls == AK_INT && j < (int)parts.count && labelIsCount(parts[j])) { a->cntArg = j; a->cntKind = 0; break; }
        if (a->cntArg == 0xff) return pl;
    }
    static const char *const kAsync[] = {"commandBuffer", "commandBufferWithUnretainedReferences", "commandBufferWithDescriptor:",
        "renderCommandEncoderWithDescriptor:", "blitCommandEncoder", "blitCommandEncoderWithDescriptor:", "computeCommandEncoder",
        "computeCommandEncoderWithDispatchType:", "computeCommandEncoderWithDescriptor:", NULL};
    static const char *const kCache[] = {"length", "width", "height", "depth", "pixelFormat", "textureType", "mipmapLevelCount",
        "sampleCount", "arrayLength", "usage", "storageMode", "cpuCacheMode", "hazardTrackingMode", "resourceOptions", "contents",
        "gpuAddress", "allocatedSize", "isFramebufferOnly", "isShareable", "hasUnifiedMemory", "isLowPower", "isRemovable",
        "isHeadless", "registryID", "supportsFamily:", "supportsTextureSampleCount:", "supportsFeatureSet:",
        "supportsVertexAmplificationCount:", "supportsRasterizationRateMapWithLayerCount:", "supportsCounterSampling:",
        "supportsBlitSampling", "supportsDynamicLibraries", "supportsRaytracing", "supports32BitFloatFiltering",
        "supportsBCTextureCompression", "supportsPullModelInterpolation", "areBarycentricCoordsSupported",
        "supportsShaderBarycentricCoordinates", "areProgrammableSamplePositionsSupported", "areRasterOrderGroupsSupported",
        "minimumLinearTextureAlignmentForPixelFormat:", "minimumTextureBufferAlignmentForPixelFormat:", "maxBufferLength",
        "recommendedMaxWorkingSetSize", "currentAllocatedSize", NULL};
    pl->isNew = isNewFamily(name);
    pl->flushAfter = strcmp(name, "commit") == 0 || strncmp(name, "present", 7) == 0;
    pl->isCommit = strcmp(name, "commit") == 0;
    pl->forceSync = strncmp(name, "waitUntil", 9) == 0 || strncmp(name, "waitFor", 7) == 0;
    pl->asyncOK = pl->retKind == RK_ID && nameHas(name, kAsync);
    pl->cacheable = pl->retKind != RK_VOID && pl->retKind != RK_ID && pl->nargs <= 1 && nameHas(name, kCache) &&
                    (pl->nargs == 0 || pl->a[0].cls == AK_INT);
    pl->eligible = 1;
    return pl;
}

static void fastNoteResultClass(SEL sel, uint32_t cls) {
    Plan *pl = plan_find(sel);
    if (pl) pl->resClass = cls;
}

static void *sigEncodingFor(SEL sel, const char *enc, NSString *cls) {
    if (gNoFast) return NULL;
    pthread_mutex_lock(&gPlanLock);
    Plan *pl = plan_find(sel);
    if (pl) {
        if (strcmp(pl->enc, enc) != 0 && !pl->dead) {
            pl->dead = 1;   // same selector, different signature on another class: back to forwarding for good
            if (pl->eligible) class_replaceMethod(gProxyClass, sel, (IMP)_objc_msgForward, enc);
        }
        pthread_mutex_unlock(&gPlanLock);
        return NULL;
    }
    pl = buildPlan(sel, enc);
    if (pl->eligible && gNPlans < PLAN_TAB) {
        pl->selid = gNextSelId++;
        size_t nl = strlen(pl->name) + 1;
        uint8_t *d = malloc(4 + nl);
        memcpy(d, &pl->selid, 4);
        memcpy(d + 4, pl->name, nl);
        uint64_t ol = 0;
        mcl_raw(MCL_OP_DEFSEL, d, 4 + nl, &ol);
        free(d);
    }
    unsigned k = ((uintptr_t)sel >> 3) & (PLAN_TAB - 1);
    pl->next = gPlanTab[k];
    __atomic_store_n(&gPlanTab[k], pl, __ATOMIC_RELEASE);
    if (pl->eligible) {
        gPlanList[gNPlans++] = pl;
        class_addMethod(gProxyClass, sel, (IMP)mcl_tramp, enc);
    }
    pthread_mutex_unlock(&gPlanLock);
    return NULL;
}

// ---- results -------------------------------------------------------------------------------------------
static id proxy_for(uint64_t h, uint32_t cls, BOOL nw) {
    mcl_init();
    [gProxyLock lock];
    MCLProxy *p = NSMapGet(gProxies, (void *)h);
    if (p) {
        [p retain];
        [gProxyLock unlock];
        tq_drop(h);   // the host counted this mention; give the extra reference back
        return nw ? p : [p autorelease];
    }
    NSDictionary *info = classreg_get(cls);
    p = [MCLProxy alloc];
    [p initWithHandle:h info:info ?: @{@"c": @[@"NSObject"], @"p": @[]}];
    NSMapInsert(gProxies, (void *)h, p);
    [gProxyLock unlock];
    return nw ? p : [p autorelease];
}

// Small direct-mapped cache of immutable getters (MTLBuffer.length, MTLTexture.width, ...).
typedef struct { uint64_t h, key, val; } CEnt;
static CEnt gCache[2048];
static pthread_mutex_t gCacheLock = PTHREAD_MUTEX_INITIALIZER;
static inline unsigned cslot(uint64_t h, uint64_t key) { return (unsigned)((h * 0x9E3779B97F4A7C15ull ^ key * 0xC2B2AE3D27D4EB4Full) >> 40) & 2047; }

static void obj_arg(TQ *q, id o) {
    if (!o) { qarg(q, MCL_A_NIL, NULL, 0); return; }
    Class c = object_getClass(o);
    if (c == gProxyClass) { uint64_t h = ((MCLProxy *)o)->_h; qarg(q, MCL_A_HANDLE, &h, 8); return; }
    if (!gOldDesc && isDescriptorClass(o)) {
        q64(q, MCL_A_DESC);       // length patched below
        uint32_t at = q->len - 8, start = q->len;
        desc_emit(q, o);
        uint32_t n = q->len - start;
        uint64_t hdr = MCL_A_DESC | ((uint64_t)n << 32);
        memcpy(q->buf + at, &hdr, 8);
        qneed(q, 8);
        while (q->len & 7) q->buf[q->len++] = 0;   // keep the record 8-aligned
        return;
    }
    @autoreleasepool {
        if ([o isKindOfClass:[NSString class]]) {
            const char *u = [(NSString *)o UTF8String];
            qarg(q, MCL_A_STRING, u, (uint32_t)strlen(u));
            return;
        }
        NSData *d = [NSPropertyListSerialization dataWithPropertyList:encodeObject(o) format:NSPropertyListBinaryFormat_v1_0 options:0 error:NULL];
        if (!d) { qarg(q, MCL_A_NIL, NULL, 0); return; }
        qarg(q, MCL_A_PLIST, d.bytes, (uint32_t)d.length);
    }
}
static inline uint64_t handle_of(id o) {
    if (!o) return 0;
    if (object_getClass(o) == gProxyClass) return ((MCLProxy *)o)->_h;
    return 0;
}

static _Atomic uint64_t gNextGuestHandle = MCL_GUEST_HANDLE_BASE;

__attribute__((used, visibility("hidden")))
uint64_t mcl_fast_dispatch(uint64_t *R, uint8_t *stk) {
    MCLProxy *self = (MCLProxy *)R[0];
    Plan *pl = plan_find((SEL)R[1]);
    if (!pl) { fprintf(stderr, "[mclmetal] no plan for %s\n", sel_getName((SEL)R[1])); return 0; }
    uint64_t t0 = gStats ? mach_absolute_time() : 0;
    uint64_t self_h = self->_h;
    uint64_t ckey = 0;
    if (pl->cacheable && !gNoCache) {
        ckey = pl->selid | (pl->nargs ? R[pl->a[0].xi] << 16 : 0);
        unsigned s = cslot(self_h, ckey);
        pthread_mutex_lock(&gCacheLock);
        CEnt e = gCache[s];
        pthread_mutex_unlock(&gCacheLock);
        if (e.h == self_h && e.key == ckey) {
            gNCacheHit++;
            if (pl->retKind == RK_F32 || pl->retKind == RK_F64) { R[8] = e.val; return 0; }
            return e.val;
        }
    }
    TQ *q = tq();
    q->busy = 1;
    q->len = 16;

    // pointer arguments: element counts, and whether the data can be copied into the stream
    uint32_t cnt[10] = {0};
    BOOL bigData = NO;
    for (int i = 0; i < pl->nargs; i++) {
        PArg *a = &pl->a[i];
        if (a->cls != AK_IDARR && a->cls != AK_DATA) continue;
        uint64_t c = a->cntKind == 1 ? R[pl->a[a->cntArg].xi + 1] : R[pl->a[a->cntArg].xi];
        if (pl->a[a->cntArg].size == 4) c &= 0xffffffffu;
        if (c > (1u << 24)) c = 0;
        cnt[i] = (uint32_t)c;
        if (a->cls == AK_DATA && R[a->xi] && c * a->elem > 4096) bigData = YES;
    }
    BOOL sync = pl->retKind != RK_VOID || pl->forceSync || bigData || gNoBatch;
    BOOL async = !sync ? NO : (!gNoAsync && pl->asyncOK && pl->resClass && !bigData && !pl->forceSync);
    uint64_t newh = 0;
    uint8_t kind = MCL_R_MSG;
    if (async) { newh = atomic_fetch_add(&gNextGuestHandle, 1); kind = MCL_R_CREATE; }
    else if (sync) kind = MCL_R_SYNC;
    uint32_t off = rec_begin(q, kind, pl->selid, self_h, newh);
    for (int i = 0; i < pl->nargs; i++) {
        PArg *a = &pl->a[i];
        switch (a->cls) {
        case AK_INT: qarg(q, MCL_A_SCALAR, &R[a->xi], a->size); break;
        case AK_FP: qarg(q, MCL_A_SCALAR, &R[8 + a->di], a->size); break;
        case AK_STK: qarg(q, MCL_A_SCALAR, stk + a->soff, a->size); break;
        case AK_HFA: {
            uint8_t tmp[32];
            for (int k = 0; k < a->nmem; k++) memcpy(tmp + k * a->msz, &R[8 + a->di + k], a->msz);
            qarg(q, MCL_A_SCALAR, tmp, a->nmem * a->msz);
            break;
        }
        case AK_SREG: qarg(q, MCL_A_SCALAR, &R[a->xi], a->size); break;
        case AK_SREF: qarg(q, MCL_A_SCALAR, (void *)R[a->xi], a->size); break;
        case AK_ID: obj_arg(q, (id)R[a->xi]); break;
        case AK_BLOCK: {
            id blk = (id)R[a->xi];
            if (!blk) { qarg(q, MCL_A_NIL, NULL, 0); break; }
            NSDictionary *bd = encodeBlock(blk, @(pl->name));   // registers the block for the pump thread
            uint64_t bid = [bd[@"blk"] unsignedLongLongValue];
            NSArray *types = bd[@"sig"];
            uint8_t buf[8 + 1 + 16];
            memcpy(buf, &bid, 8);
            uint32_t nt = (uint32_t)MIN(types.count, 15);
            buf[8] = (uint8_t)nt;
            for (uint32_t k = 0; k < nt; k++) buf[9 + k] = (uint8_t)[types[k] UTF8String][0];
            qarg(q, MCL_A_BLOCK, buf, 9 + nt);
            break;
        }
        case AK_IDARR: {
            id *p = (id *)R[a->xi];
            if (!p) { qarg(q, MCL_A_NIL, NULL, 0); break; }
            q64(q, (uint64_t)MCL_A_IDARRAY | ((uint64_t)cnt[i] * 8 << 32));
            qneed(q, cnt[i] * 8);
            uint64_t *o = (uint64_t *)(q->buf + q->len);
            for (uint32_t k = 0; k < cnt[i]; k++) o[k] = handle_of(p[k]);
            q->len += cnt[i] * 8;
            break;
        }
        case AK_DATA: {
            const void *p = (const void *)R[a->xi];
            if (!p) qarg(q, MCL_A_NIL, NULL, 0);
            else if (bigData) qarg(q, MCL_A_SCALAR, &p, 8);   // synchronous call: the host reads the caller's memory directly
            else qarg(q, MCL_A_DATA, p, cnt[i] * a->elem);
            break;
        }
        }
    }
    rec_end(q, off, pl->nargs);
    q->busy = 0;
    pl->nCalls++;

    uint64_t ret = 0;
    const uint8_t *rp = NULL;
    uint64_t ol = 0;
    pthread_mutex_lock(&G.lock);
    if (pl->forceSync) {   // may block on other GPU work: push everything queued so far, then wait outside the lock
        g_flush_locked(NULL);
    } else {
        g_append_locked(q->buf + 16, q->len - 16);
        if (sync) rp = g_flush_locked(&ol);
        else if (pl->flushAfter || G.len > (96u << 10)) g_flush_locked(NULL);
    }
    for (uint32_t i = 0; i < q->nDefer; i++) drop_locked(q->defer[i]);
    q->nDefer = 0;
    pthread_mutex_unlock(&G.lock);
    if (pl->forceSync) {
        MclMsgHdr h = {MCL_MAGIC, q->len - 16, 0, 0};
        memcpy(q->buf, &h, sizeof h);
        rp = mcl_raw(MCL_OP_BATCH, q->buf, q->len, &ol);
        gNFlush++;
    }
    if (async) {
        gNAsync++;
        ret = (uint64_t)proxy_for(newh, pl->resClass, pl->isNew);
    } else if (!sync) {
        gNBatched++;
        if (pl->isCommit) gNCommit++;
    } else {
        gNSync++;
        pl->nSync++;
        if (rp && ol >= sizeof(MclReply)) {
            MclReply r;
            memcpy(&r, rp, sizeof r);
            const uint8_t *blob = rp + sizeof r;
            switch (r.tag) {
            case MCL_T_VOID:
                if (pl->forceSync) {   // completion handlers of finished work run before the wait returns
                    for (int i = 0; i < 4000 && atomic_load(&gEvHandled) < r.value; i++) usleep(50);
                }
                break;
            case MCL_T_SCALAR:
                if (pl->retKind == RK_F32 || pl->retKind == RK_F64) R[8] = r.value; else ret = r.value;
                if (pl->cacheable) {
                    unsigned s = cslot(self_h, ckey);
                    pthread_mutex_lock(&gCacheLock);
                    gCache[s] = (CEnt){self_h, ckey, r.value};
                    pthread_mutex_unlock(&gCacheLock);
                }
                break;
            case MCL_T_HANDLE:
                if (r.blen) {
                    NSData *bd = [NSData dataWithBytes:blob length:r.blen];
                    NSDictionary *ci = [NSPropertyListSerialization propertyListWithData:bd options:NSPropertyListImmutable format:NULL error:NULL];
                    if (ci) classreg_note(@{@"i": @(r.cls), @"c": ci[@"c"] ?: @[], @"p": ci[@"p"] ?: @[]});
                }
                pl->resClass = r.cls;
                ret = (uint64_t)proxy_for(r.value, r.cls, pl->isNew);
                break;
            case MCL_T_PLIST: {
                NSData *bd = [NSData dataWithBytes:blob length:r.blen];
                id v = [NSPropertyListSerialization propertyListWithData:bd options:NSPropertyListImmutable format:NULL error:NULL];
                ret = (uint64_t)decode(v, pl->isNew);
                break;
            }
            case MCL_T_EXC:
                fprintf(stderr, "[mclmetal] remote exception in %s: %.*s\n", pl->name, (int)r.blen, (const char *)blob);
                break;
            default: break;
            }
        }
    }
    if (gStats) stats_maybe_dump();
    if (t0) pl->nNs += mach_absolute_time() - t0;
    return ret;
}

// R = saved x0-x7, d0-d7. The dispatcher returns x0; float/double results are written to R[8] (d0).
__asm__(
    ".text\n.p2align 2\n.globl _mcl_tramp\n_mcl_tramp:\n"
    "  sub sp, sp, #176\n"
    "  stp x29, x30, [sp, #160]\n"
    "  add x29, sp, #160\n"
    "  stp x0, x1, [sp, #0]\n"
    "  stp x2, x3, [sp, #16]\n"
    "  stp x4, x5, [sp, #32]\n"
    "  stp x6, x7, [sp, #48]\n"
    "  stp d0, d1, [sp, #64]\n"
    "  stp d2, d3, [sp, #80]\n"
    "  stp d4, d5, [sp, #96]\n"
    "  stp d6, d7, [sp, #112]\n"
    "  mov x0, sp\n"
    "  add x1, sp, #176\n"
    "  bl _mcl_fast_dispatch\n"
    "  ldr d0, [sp, #64]\n"
    "  ldp x29, x30, [sp, #160]\n"
    "  add sp, sp, #176\n"
    "  ret\n");

// ---- profile (MCL_STATS=1) -------------------------------------------------------------------------------
static BOOL gForceDump;
static void stats_dump_force(void) { gLastDump = gLastDump ?: mach_absolute_time(); gForceDump = YES; stats_maybe_dump(); }
static NSMutableDictionary *gLegacySel;
static void legacy_note(SEL sel) {
    @synchronized([MCLProxy class]) {
        if (!gLegacySel) gLegacySel = [NSMutableDictionary new];
        NSString *k = @(sel_getName(sel));
        gLegacySel[k] = @([gLegacySel[k] unsignedLongLongValue] + 1);
    }
}
static void stats_maybe_dump(void) {
    uint64_t now = mach_absolute_time();
    static mach_timebase_info_data_t tb;
    if (!tb.denom) mach_timebase_info(&tb);
    if (!gLastDump) gLastDump = now;
    if (!gForceDump && (now - gLastDump) * tb.numer / tb.denom < 5000000000ull) return;
    double secs = (double)((now - gLastDump) * tb.numer / tb.denom) / 1e9;
    gLastDump = now;
    unsigned idx[PLAN_TAB], n = 0;
    for (unsigned i = 0; i < gNPlans; i++) if (gPlanList[i]->nCalls) idx[n++] = i;
    for (unsigned i = 0; i < n && i < 12; i++)   // partial selection sort by inclusive time
        for (unsigned j = i + 1; j < n; j++)
            if (gPlanList[idx[j]]->nNs > gPlanList[idx[i]]->nNs) { unsigned t = idx[i]; idx[i] = idx[j]; idx[j] = t; }
    if (n > 12) n = 12;
    uint64_t totNs = 0, totCalls = 0;
    for (unsigned i = 0; i < gNPlans; i++) { totNs += gPlanList[i]->nNs; totCalls += gPlanList[i]->nCalls; }
    double toMs = (double)tb.numer / tb.denom / 1e6;
    fprintf(stderr, "[mclguest] %.1fs calls=%llu (batched=%llu sync=%llu async=%llu legacy=%llu cachehit=%llu) flushes=%llu KB=%llu commits=%llu | per commit: %.1f calls %.1f flushes | guest-ms=%.0f\n",
            secs, totCalls, gNBatched, gNSync, gNAsync, gLegacyCalls, gNCacheHit, gNFlush, gNBytes / 1024, gNCommit,
            gNCommit ? (double)(totCalls + gLegacyCalls) / gNCommit : 0, gNCommit ? (double)gNFlush / gNCommit : 0, totNs * toMs);
    for (unsigned i = 0; i < n; i++) {
        Plan *p = gPlanList[idx[i]];
        fprintf(stderr, "   %-56s n=%-7llu sync=%-6llu ms=%.1f\n", p->name, p->nCalls, p->nSync, p->nNs * toMs);
    }
    @synchronized([MCLProxy class]) {
        NSArray *keys = [gLegacySel keysSortedByValueUsingComparator:^NSComparisonResult(NSNumber *a, NSNumber *b) { return [b compare:a]; }];
        for (NSUInteger i = 0; i < keys.count && i < 5; i++)
            fprintf(stderr, "   [legacy] %-52s n=%llu\n", [keys[i] UTF8String], [gLegacySel[keys[i]] unsignedLongLongValue]);
        [gLegacySel removeAllObjects];
    }
    for (unsigned i = 0; i < gNPlans; i++) { gPlanList[i]->nCalls = gPlanList[i]->nNs = gPlanList[i]->nSync = 0; }
    gNBatched = gNSync = gNAsync = gNFlush = gNBytes = gNCommit = gNCacheHit = gLegacyCalls = 0;
}

#ifndef MCL_LOOPBACK
// dyld has already processed DYLD_INSERT_LIBRARIES; leaving it in the environment confuses
// LaunchServices/XPC in AppKit apps. Children are re-injected by maclator itself.
__attribute__((constructor)) static void mcl_scrub_env(void) {
    unsetenv("DYLD_INSERT_LIBRARIES");
}
#endif

// ---- interposed entry points ---------------------------------------------------------------
extern id MTLCreateSystemDefaultDevice(void);
extern NSArray *MTLCopyAllDevices(void);

#ifdef MCL_LOOPBACK
// While the in-process "host" runs, Metal's own internal calls must reach the real functions.
__thread int mcl_in_host;
#endif
static id mcl_MTLCreateSystemDefaultDevice(void) {
    if (getenv("MCL_NIL")) return nil;
#ifdef MCL_LOOPBACK
    if (mcl_in_host) return MTLCreateSystemDefaultDevice();
#endif
    NSDictionary *r = mcl_rpc(@{@"c": @"root", @"what": @"device"});
    if (r[@"x"]) { fprintf(stderr, "[mclmetal] %s\n", [r[@"x"] UTF8String]); return nil; }
    return decode(r[@"r"], YES); // MTLCreateSystemDefaultDevice returns +1
}
static NSArray *mcl_MTLCopyAllDevices(void) {
    if (getenv("MCL_NIL")) return [NSArray array];
#ifdef MCL_LOOPBACK
    if (mcl_in_host) return MTLCopyAllDevices();
#endif
    NSDictionary *r = mcl_rpc(@{@"c": @"root", @"what": @"all"});
    return [decode(r[@"r"], NO) retain];
}

#ifdef MCL_LOOPBACK
// Native test: called explicitly by the test app (interposition would also redirect the bridge).
id mcl_default_device(void) { return mcl_MTLCreateSystemDefaultDevice(); }
#endif

struct mcl_interpose { const void *replacement; const void *replacee; };
__attribute__((used)) static const struct mcl_interpose mcl_interposers[] __attribute__((section("__DATA,__interpose"))) = {
    {(const void *)mcl_MTLCreateSystemDefaultDevice, (const void *)MTLCreateSystemDefaultDevice},
    {(const void *)mcl_MTLCopyAllDevices, (const void *)MTLCopyAllDevices},
};

static void stats_atexit(void) { tq_flush_current(); stats_dump_force(); }
