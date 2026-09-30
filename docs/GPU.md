# GPU passthrough (Metal → host GPU)


`maclator --gpu ./app` routes the guest's Metal API to the host's real GPU (AMD RX 460 on the test Mac).
Verified: `tests/guest/metaltri` (clear + triangle + readback, `STRESS=1` for ANGLE-like load) and
Chromium 157 with ANGLE-Metal (GPU compositing, rasterization, WebGL, WebGPU, Graphite enabled).

Launch Chromium with acceleration: `~/bin/chromium-gpu [url]` on the test Mac (source: `gpu/chromium-gpu`; uses
`--gpu --gpu-only --type=gpu-process` so only the GPU process loads the bridge — giving every helper the bridge
starved the renderers; flags: `--use-angle=metal --use-gl=angle --enable-gpu --ignore-gpu-blocklist
--disable-gpu-sandbox --disable-gpu-watchdog --no-sandbox`, plus the "lean" flags, see Chromium configuration).

### Results (animated 2D canvas, hack; `bench/gpubench.py`)
| stage | median frame | notes |
|---|---|---|
| software raster/compositing | 1000 ms | |
| bridge v1 (plist RPC per Metal message) | ~100 ms (10 fps) | ~100 bridged calls/frame; ~97 % of the frame was guest-side marshalling of one call: `renderCommandEncoderWithDescriptor:` (22 ms, generic descriptor introspection) |
| bridge v2 (this section) | **16.7 ms (vsync-capped 60 fps)** | still 60 fps with 400 arcs; guest time spent in the bridge fell from ~4300 ms to ~650 ms per 5 s |

### Design (all in `gpu/` + `crates/maclator/src/gpu.rs`)
- `gpu/mclmetal.m` → **guest** arm64 `libmclmetal.dylib`, injected with `DYLD_INSERT_LIBRARIES` (maclator does this
  for `--gpu`, then the shim `unsetenv`s it). It interposes `MTLCreateSystemDefaultDevice/MTLCopyAllDevices` and
  returns `MCLProxy` (NSProxy) objects for host Metal objects.
- `gpu/mclbridge.m` → **host** x86-64 `libmclbridge.dylib`, dlopen'ed by maclator (`gpu.rs`). Handle table of real
  Metal objects; calls executed with `NSInvocation`. Guest and host share one address space, so raw pointers
  (`setVertexBytes`, `getBytes`, `MTLBuffer.contents`) just work.
- `gpu/mclproto.h`: wire format shared by both sides.
- Transport: `svc #0x80` with x16 = `0x4D43` (`SYS_MCL_HOSTCALL`), x0=op x1=request ptr x2=len → x0=reply ptr x1=len.
  Ops: 1 legacy plist RPC, 2 free reply, 3 wait for host event (completion handlers), 4 **batch**, 5 define selector.

**Fast path (op 4).** The first message to a proxy for a selector still goes through `forwardInvocation:` (the
signature comes from the host), but `methodSignatureForSelector:` then builds a *plan* (register classification
per AAPCS64/Apple arm64: ints, floats, HFAs, structs by value/by reference, `const id *`/`const T *` arrays with their
count taken from the `NSRange`/`count`/`length` argument, stack arguments, blocks) and installs a real method on
`MCLProxy` pointing at an assembly trampoline (`mcl_tramp`, saves x0–x7/d0–d7, calls `mcl_fast_dispatch`). No
NSProxy forwarding, no NSInvocation, no plist:
- void calls are appended as a compact binary record to **one global, ordered command queue** (threads encode into
  thread-local scratch and append under a short lock; ordering across threads = order the calls returned);
- the queue is sent in a single trap on `commit`/`present*`, on the next call that returns a value, before any legacy
  RPC, or above 96 KB; `waitUntil*` flushes then waits *outside* the lock (no deadlock with other GPU threads), and
  waits until completion handlers of finished work have run;
- `commandBuffer`, `*CommandEncoder…` calls get a **guest-chosen handle** (`≥ 2^40`) and stay asynchronous;
- immutable getters (`MTLBuffer.length/contents`, `MTLTexture.width/…`, `supportsFamily:` …) are cached per proxy;
- `MTLRenderPassDescriptor`/pipeline/sampler/… descriptors use a **binary schema encoding** (`desc_emit`): per class
  the guest defines a schema once (getters/setters, values of a pristine instance) and the host checks *its* defaults
  (it asks for properties whose defaults differ to be always sent); per call only properties that differ from the
  defaults are sent, nested descriptors and unused array elements (8 colour attachments, 31 buffer/attribute slots)
  disappear;
- completion-handler blocks (`addCompletedHandler:` …) are registered as before but travel in the batch;
- host-side **memo**: `newSamplerState/DepthStencilState/Function/RenderPipelineState/Library…` with identical
  arguments return the same object (Chromium recreates identical ones constantly).
- Pointers in batched calls are copied into the stream when small (`setVertexBytes` …); above 4 KB the call is made
  synchronous and the host reads the guest memory directly.

Also fixed on the way: `newTextureWithDescriptor:iosurface:plane:` used to pass the *guest's* IOSurfaceRef pointer to
Metal (a bogus host CF object → crash when the texture was deallocated; this was the "unsupported IOSurface" gap).
The guest now sends `IOSurfaceGetID`, the host does `IOSurfaceLookup`.

Profiling: `MCL_STATS=1` prints, every 5 s, from the guest `[mclguest]` (calls per commit, batched/sync/async/legacy
counts, flushes per commit, top selectors by inclusive time, top legacy selectors) and from the host `[mclstats]`
(messages, per-selector host time, `*` = batched). In steady state on the canvas benchmark: 47 calls and 5.5 flushes
per commit, 0 legacy calls, ~0.6 s of guest time per 5 s.
Debug toggles (guest env): `MCL_NOFAST=1` (everything through `forwardInvocation:`; drops are disabled in this mode),
`MCL_NOASYNC`, `MCL_NOCACHE`, `MCL_NOBATCH` (every call synchronous), `MCL_OLDDESC` (plist descriptors),
`MCL_TRACE=1` (per message, host side also mentions/drops of handles). `MACLATOR_GPU_SPIN=caller,bridge` tunes the
spin-before-park of the thread hand-off in `gpu.rs` (default 20,20 µs; measured: no effect at this call rate).

- `gpu/build.sh` builds everything (also a native arm64 *loopback* build, `out/metaltri_loop`, which runs shim+bridge
  in one process on an Apple Silicon Mac — debug marshalling there first; the fast path works in loopback too).
- Install on the test Mac: `libmclbridge.dylib` + `libmclmetal.dylib` in `~/bin/gpu/` (also searched: next to maclator,
  `~/.maclator/gpu`, `$MACLATOR_GPU_DIR`; the original v1 libraries are in `~/bin/gpu.orig/`).

Hard-won constraints (do not regress):
1. **Host frameworks must never run on a guest thread.** The kernel has one special reply port per thread; guest
   libxpc and host libxpc both cache/recycle it → guest XPC dies with `MACH_RCV_INVALID_NOTIFY` (0x10004007),
   seen as libdispatch "Unexpected error from mach_msg_receive" BRK in LaunchServices/CFPreferences. `gpu.rs` gives
   every guest thread its own bridge thread (persistent hand-off slot, spin then park).
2. Host CoreFoundation/Metal must be initialised **before the guest starts** (`--gpu` preloads the bridge and runs
   `mcl_warmup` on a scratch thread); initialising them mid-run disturbed guest XPC too.
3. Never pass `DYLD_INSERT_LIBRARIES=<arm64 dylib>` to host processes (`spawn.rs` scrubs it; maclator re-injects).
4. `dispatch_data_t` is toll-free bridged to NSData — check for it before NSData (`newLibraryWithData:`).
5. Reply plists must be parsed from a private copy (parsed objects can reference the source bytes).
6. Chromium's GPU watchdog / objc-zombie crashes look like `str wzr,[xzr]` in the Chromium Framework
   (`--disable-gpu-watchdog`). `--dump-on-fault` survives the guest resetting SIGSEGV and marks the faulting thread.
7. **Command ordering across threads.** A per-thread queue looked natural but breaks as soon as one thread uses an
   object another thread created asynchronously (ANGLE adds handlers to a command buffer from another thread): the
   creation sat unflushed in the creator's queue. Keep the single global queue.
8. Never let a proxy's drop be sent before the queued commands that use the handle (drops are records in the same
   queue; drops raised while a record is being built are deferred until it is appended).
9. Host must not release/adopt inconsistently: new-family results are +1 (the table takes its own reference, the
   host drops its own); memoised objects are shared, the cache holds one reference.
10. `document.visibilityState` of the test Mac's Chromium window is often `hidden` (no display attached to the active
    Space); rAF then runs at 1 fps and *looks* like a GPU/emulator regression. Benchmarks use
    `--disable-backgrounding-occluded-windows --disable-renderer-backgrounding --disable-background-timer-throttling`
    and DevTools focus emulation.

Known gaps / next steps: `CAMetalLayer` (apps that present directly, not via Chromium) is not bridged;
`MTLFunctionConstantValues`, argument buffers, `MTLSharedEvent` listeners and NSURL-based APIs are only partly covered;
`status`/`signaledValue` polling is one synchronous round trip per frame each (~100 µs); pipeline/library compile
caching across launches is left to Metal's own on-disk shader cache (measured: warm `newLibraryWithSource` ≈ 0.3 ms
per call versus ~90 ms cold, ANGLE's big library ~2.7 s once) — a `MTLBinaryArchive` layer was not needed.
Presentation: steady-state per-frame traffic contains no readbacks/uploads (`getBytes`/`replaceRegion` never appear,
only two small buffer-to-buffer blits per frame); output goes to IOSurface-backed textures, i.e. no frame-sized copy.
