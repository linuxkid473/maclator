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

#ifdef MCL_LOOPBACK
// Native test mode: the "host" is in the same process.
extern void *mcl_call(uint64_t op, const void *in, uint64_t inlen, uint64_t *outlen);
#endif

static NSMutableArray *gDrops;
static NSLock *gDropLock;
static NSMapTable *gProxies; // handle -> proxy (weak)
static NSLock *gProxyLock;
static BOOL gTrace;

static void mcl_init(void) {
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        gDrops = [NSMutableArray new];
        gDropLock = [NSLock new];
        gProxies = [[NSMapTable mapTableWithKeyOptions:NSPointerFunctionsOpaquePersonality | NSPointerFunctionsOpaqueMemory valueOptions:NSPointerFunctionsOpaquePersonality | NSPointerFunctionsOpaqueMemory] retain];
        gProxyLock = [NSLock new];
        gTrace = getenv("MCL_TRACE") != NULL;
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

static NSDictionary *mcl_op(uint64_t op, NSDictionary *req) {
    mcl_init();
    NSData *d = nil;
    if (req) {
        NSMutableDictionary *r = [[req mutableCopy] autorelease];
        [gDropLock lock];
        if (gDrops.count) { r[@"drop"] = [[gDrops copy] autorelease]; [gDrops removeAllObjects]; }
        [gDropLock unlock];
        NSError *e = nil;
        d = [NSPropertyListSerialization dataWithPropertyList:r format:NSPropertyListBinaryFormat_v1_0 options:0 error:&e];
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
@end

static id decode(id v, BOOL retainedNew);
static id encodeObject(id o);
static id encodeBlock(id blk, NSString *selName);
static NSUInteger idArrayCount(NSInvocation *inv, NSMethodSignature *sig, NSArray *parts);
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
- (void)dealloc {
    [gProxyLock lock];
    if (NSMapGet(gProxies, (void *)_h) == (void *)self) NSMapRemove(gProxies, (void *)_h);
    [gProxyLock unlock];
    [gDropLock lock];
    [gDrops addObject:@(_h)];
    [gDropLock unlock];
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
    SEL sel = inv.selector;
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
    if (isDescriptorClass(o)) return describe(o);
    return @{@"u": @(object_getClassName(o))};
}

// ---- blocks (completion handlers): invoked later from a pump thread ----------------------
static NSMutableDictionary *gBlocks;
static NSLock *gBlockLock;
static uint64_t gNextBlock = 1;
static void *pump(void *arg);

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
            if (!ent) continue;
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
        uint64_t h = [d[@"h"] unsignedLongLongValue];
        [gProxyLock lock];
        MCLProxy *p = NSMapGet(gProxies, (void *)h);
        if (p) {
            [p retain];
            [gProxyLock unlock];
            // The host counted this mention; give the extra reference back.
            [gDropLock lock]; [gDrops addObject:@(h)]; [gDropLock unlock];
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
