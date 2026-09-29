# Maclator — handoff / continuation notes

_Last updated 2026-09-29. Latest commit at time of writing: `ea182b8`._

Maclator runs **arm64 macOS programs on Intel (x86-64) Macs**, "reverse Rosetta". It loads
the arm64 Mach-O plus the real arm64 macOS userland (dyld + the arm64e dyld shared cache
from an Apple Silicon macOS image), and executes the arm64 code with an interpreter and an
x86-64 JIT. System calls / Mach traps are passed to the Intel host kernel where semantics
match, and emulated where they don't.

## What works today (verified on real Intel hardware)

Test machine ("the hack"): Intel Core i5-4440 (Haswell, 4 cores, AVX2), 12 GB, macOS 26.7
(25G229) hackintosh, reachable as `ssh vihaannathan@vihaans-mac-pro.local` (key auth works).

| Program | Status |
|---|---|
| `fastfetch` (arm64) | works |
| `neatvi` (arm64 vi) | works |
| custom AppKit calculator (`tests/guest/calc`) | works, window + clicks |
| VS Code 1.139.1 arm64 (Electron) | works: window, editor, typing; slow-ish (see Performance) |
| Chromium 157.0.8079.0 arm64 snapshot | works: browser UI + page rendering + DevTools |
| `tests/guest/{timer,forktest}` | pass |

Not done / not attempted: Overcast (iOS-on-Mac, FairPlay-encrypted, not obtainable here; decided
not to pursue DRM circumvention), GPU/Metal (everything renders in software, `--disable-gpu`),
audio, most IOKit hardware, sandboxed apps, Swift-heavy Apple apps.

## Quick start

```bash
# build (on the dev Mac, arm64 host, cross-compiles the x86_64 binary)
cargo build --release --target x86_64-apple-darwin -p maclator
# result: target/x86_64-apple-darwin/release/maclator

# on the hack (installed at ~/bin/maclator, PATH set in ~/.zshrc):
maclator ./some-arm64-binary [args]        # no flags needed
```

`maclator` finds the arm64 shared cache by itself: `$MACLATOR_SYSROOT`, then
`~/.maclator/sysroot`, then the newest `~/maclator-sysroot/*/` that contains
`dyld_shared_cache_arm64e` (currently `~/maclator-sysroot/25G83__Mac14,2`). `--sysroot DIR`
overrides. The cache on the hack is **macOS 26.6.2 (25G83)** — the host is 26.7 (25G229);
the exact build was not obtainable from ipsw.me/AppleDB. It works anyway.

Deploy to the hack (atomic replace so running processes are not corrupted):
```bash
scp target/x86_64-apple-darwin/release/maclator vihaannathan@vihaans-mac-pro.local:bin/maclator.new
ssh vihaannathan@vihaans-mac-pro.local 'mv ~/bin/maclator.new ~/bin/maclator'
```
(There is also a copy in `~/maclator-run/maclator` used by older test scripts.)

### Things on the hack
- `~/maclator-run/` — test programs: `fastfetch` (+ `libyyjson.0.dylib`, re-signed, rpath'd to
  `@executable_path`), `neatvi`, `calc`, `timer`, `forktest`, helper tools `wl`, `cap`, python
  scripts `typebench.py`, `typetrace.py`, `shot.py`, `wsonly.py`.
- `~/vscode-arm64/Visual Studio Code.app` — official VS Code darwin-arm64 zip.
- `~/vscode-data/` — persistent VS Code profile with tuned `User/settings.json`
  (no welcome page, AI/chat off, telemetry off, no animations, git off).
- `~/Downloads/chromium-arm64/chrome-mac/Chromium.app` (+ zip) — official Chromium snapshot r1706829.
- `~/ipsw-tool/ipsw` — blacktop/ipsw 3.1.728 (used to fetch the dyld cache).
- `~/maclator-sysroot/25G83__Mac14,2/` — the arm64e dyld shared cache (5.5 GB, 12 subcaches).
- AOT cache: `~/Library/Caches/Maclator/aot/` (override with `MACLATOR_CACHE`).

Handy commands:
```bash
# VS Code
maclator "./Visual Studio Code.app/Contents/MacOS/Code" --no-sandbox --disable-gpu \
  --js-flags=--jitless --user-data-dir=$HOME/vscode-data --disable-workspace-trust
# Chromium
maclator ./Chromium --no-sandbox --disable-gpu --user-data-dir=/tmp/chromium-data \
  --no-first-run --no-default-browser-check --remote-debugging-port=9333 about:blank
# Electron in node mode (V8 JIT smoke test)
ELECTRON_RUN_AS_NODE=1 maclator ".../Contents/MacOS/Code" -e "console.log(process.version)"
```
Shell gotchas on the hack: zsh with aliased `ls/grep/sed`; use `command grep`, `find /tmp/` (with
trailing slash — `/tmp` is a symlink), no `timeout` binary (use `perl -e 'alarm N; exec @ARGV'`),
globs that don't match abort the command line in zsh. `screencapture` /
`CGWindowListCreateImage` fail over ssh (no Screen Recording permission): capture via DevTools
(`Page.captureScreenshot`) or have the app snapshot itself (`CALC_SNAPSHOT=1`).

## Architecture

Workspace: `crates/core` (CPU state, interpreter, softfloat, mem), `crates/maclator` (runtime),
`crates/difftest`.

- `core/interp/*` — reference AArch64 interpreter (dp, ldst, simd incl. FP16 `fp16.rs`, sys).
  It is the semantic ground truth; JIT falls back to it per instruction (`helper_interp`).
- `maclator/loader.rs, macho.rs` — loads the main Mach-O + arm64 dyld at fixed addresses;
  the *guest's own dyld* then maps the shared cache and dylibs.
- `maclator/cache.rs` — emulates `shared_region_*` syscalls; maps the cache at `0x6_0000_0000`.
- `maclator/paths.rs` — redirects `/System/**/dyld` cache paths to `--sysroot`; tracks dirfd
  virtual paths (dyld walks paths with `openat(dirfd,"rel")`); `discover_sysroot()`.
- `maclator/syscalls.rs` — BSD syscall + Mach trap emulation/pass-through (see "Emulation notes").
- `maclator/hostsys.rs` — raw x86-64 Darwin syscall/Mach-trap trampoline (8 args).
- `maclator/workq.rs` — libdispatch workqueue/workloop/kevent emulation (worker threads,
  single event-manager rule, QoS restore, wl monitors).
- `maclator/spawn.rs` — `posix_spawn/execve`: arm64-only Mach-Os are re-launched through
  maclator with forwarded flags (`--dyld --sysroot --interp --trace --dump-on-fault`).
- `maclator/iokit.rs` — `mach_msg2` shim: fake Apple-Silicon-only IOKit services
  (`AppleDiagnosticDataAccessReadOnly`), and thread suspend/resume/get_state neutralisation.
- `maclator/jit/{mod,emit,selftest}.rs` — x86-64 JIT (iced-x86), tiering (interpret cold
  blocks, translate after `MACLATOR_JIT_THRESHOLD`, default 24), block chaining via patchable
  slots, per-thread IBTC (32768 entries), per-unit precise invalidation (`INDEX`).
- `maclator/aot.rs` — persistent translation cache (`MCLAOT04`); see below.
- `maclator/engine.rs` — run loop, fault/USR1/USR2 dump handlers. `signals.rs`, `threads.rs`.

### AOT cache
Images registered: main exe (fixed base), dyld, shared cache, and **every dyld-loaded arm64
library** (registered when dyld `mmap`s its executable `__TEXT` at file offset 0; key =
`lib-<uuid>-<base>`). Bases are deterministic because `mach_vm_allocate(ANYWHERE)` is served from
a 64 GB arena (`syscalls.rs`, candidate bases tried in order). Every block that executes
(cold-interpreted or JIT-translated) is recorded; on exit `save_profile()` forks a builder that
writes `<key>.aot` (position-independent x86 + index). Loaded by **copying into the JIT code
cache** — do NOT mmap the files PROT_EXEC (macOS Gatekeeper pops "Apple could not verify…"
dialogs for every file). Bump `TRANSLATOR_VERSION` (currently **12**) whenever codegen changes.
Processes that are killed (`kill -9`) do not save profiles; quit gracefully (e.g. DevTools
`Browser.close`, see `wsonly.py`) to build the cache. Rebuilds can take several minutes.

### Emulation notes / important decisions
- **Page size 16 KiB**: `mmap` returns 16K-aligned, 16K-rounded regions (`mmap_16k`), sysctl
  `hw.pagesize` = 16384.
- **Top-byte-ignore**: guest addresses have bits 63:56 masked in `mem::host()` and in the
  JIT's `commpage_fix` (shl/shr 8) — libobjc/libmalloc use tagged pointers.
- **Code-signing fcntls** (`F_ADDFILESIGS*`, `F_CHECK_LV`) accepted (host rejects arm64 sigs).
- **fork** goes through libc `fork()` (raw syscall left the child with a stale `mach_task_self`,
  making the JIT's `mach_vm_read` of guest code fail → empty blocks → infinite loop).
- `bsdthread_ctl(SET_SELF)` accepted (host EINVALs some QoS combos; libdispatch aborts).
- `task_restartable_ranges_register` (mach_msg2 id 8000) reply patched to success.
- Thread suspend/resume/get_state MIG messages (ids 3605/3606/3603) neutralised
  (Chromium's stack-sampling profiler otherwise deadlocks the process).
- Dispatch event manager: host kqueue drops the `qos` (event-manager flag) on fired events →
  Maclator records each knote's QoS at registration and restores it; only one manager thread at
  a time (libdispatch aborts otherwise).
- FP: ARM default NaN vs x86 "real indefinite" fixed up in JIT scalar+vector arithmetic. FPCR
  rounding/DN/FZ modes are ignored by JIT-native FP (assumes defaults). QC (saturation) flag
  not maintained by native saturating ops. sNaN-vs-qNaN operand priority differs (x86 prefers
  qNaN) — 1 in ~15000 random self-test cases, not seen in practice.
- x86 baseline for JIT is SSE2 + selected SSE4.1/SSSE3 (pshufb, pmin/pmax, pmulld, pmovzx…);
  the Haswell hack and Rosetta (dev Mac) both support these.

## Debugging / profiling toolbox

Flags & env: `--interp` (no JIT), `--no-aot`, `--trace` (syscalls/Mach msgs/paths), `--trace-file`
(per-process `/tmp/maclator-trace-<pid>.log`), `--dump-on-fault` (`MACLATOR_DUMP_ON_FAULT=1`,
guest regs + disasm to `/tmp/maclator-fault-<pid>.log`), `MACLATOR_STATS=1`,
`MACLATOR_FALLBACK_HIST=1` (histogram of instructions falling back to the interpreter, written
to `/tmp/maclator-hist-<pid>.txt`, digit-normalised so exact forms show),
`MACLATOR_JIT_THRESHOLD`, `MACLATOR_JIT_DISABLE=dpimm,branch,ldst,dpreg,simd,…`,
`MACLATOR_SPIN_DEBUG=1` (reports a dispatcher spinning on one pc), `MACLATOR_AOT_SYNC`,
`MACLATOR_NO_AOT_BUILD`, `MACLATOR_CACHE`, `MACLATOR_SYSROOT`.
Signals: `kill -USR1 <pid>` all guest threads (pc/regs/backtrace), `kill -USR2 <pid>` lock-free
register dump of the interrupted thread + JIT counters (works in forked children).
`maclator --jit-dump <hex insns…>` prints generated x86 hex for a block (disassemble via a `.byte`
file + `clang -c` + `objdump -d --x86-asm-syntax=intel`). `ipsw dyld a2s/disass` (on the hack) maps
shared-cache addresses to symbols (unslid vaddr = `0x180000000 + (pc - 0x600000000)`).
`sample <pid> 8 -file f` for host-side profiles.

**Differential JIT self-test** (the important safety net):
```bash
MACLATOR_SELFTEST_MEM=1 MACLATOR_SELFTEST_SIMDMEM=1 \
  maclator --selftest-jit MASK:VALUE 8000      # hex mask/value of the encoding class
# also classes: dpimm dpreg ldst branch
```
Prints `ok= mismatch= skipped= native=`. **Always check `native=`**: an instruction that falls
back to the interpreter trivially matches it (that hid four bugs for a while). Masks must leave
the U bit (29) and Q bit (30) free when testing both variants. Known accepted mismatch: the sNaN
case above.

## Performance work & measurements (VS Code, hack, warm)

Benchmark scripts (in the session scratchpad, easily rebuilt from `~/maclator-run/typebench.py`):
launch VS Code with `--remote-debugging-port=9222`, wait for idle, measure in-page JS/DOM time,
per-key dispatch latency and `2×requestAnimationFrame` time after a key (the felt latency).

| metric | start of perf work | now |
|---|---|---|
| frame after keystroke (median) | 600–1400 ms | ~100 ms (idle frame ≈ 28 ms) |
| keystroke handling | 50–90 ms | ~39 ms |
| compositor `DrawRenderPass` | ~970 ms | ~155 ms |
| startup to idle | ~4.5 min | ~80 s (cold cache); |
| in-page JS 3e6 loop | 708 ms (`--jitless`) / 53 ms (JIT tiers on) | same |

What moved the needle: (1) native JIT for atomics (LDAR/LDAPR/STLR, CAS, LDADD/CLR/EOR/SET/SWP),
BFM, RBIT; (2) native NEON/FP for Skia's pixel pipeline (logic, add/sub/cmp/min/max/mul/sat,
umull/raddhn/urshr/ushr/shrn/rev/ushl/uzp/zip/xtn/ushll, ld1/st1 multi+lane+r, ld4/st4, FP
vector/scalar arithmetic, fcmp/fccmp/fcsel, fmov, scvtf/fcvtzs, movi/dup/umov/ins);
(3) precise per-unit invalidation (V8 rewrites code constantly; the old flush-everything policy
thrashed); (4) fast hashing/bigger IBTC; (5) persistent AOT for Electron/dyld libs.
Chromium trace method: DevTools `Tracing.start` (cats `toplevel,cc,viz,gpu,blink,…`, NOT
`disabled-by-default-cc.debug.display_items`, which inflates costs) over the *browser* websocket,
type keys, `Tracing.end`, then rank slice durations by thread.

## Remaining hot spots / next steps (rough priority)

1. **JIT code quality** (biggest remaining lever): every guest register access goes through
   memory (`[r15+off]`); each flag-setting op does 4 `setcc` byte stores and `b.cond` re-derives
   from bytes. Ideas: fuse `cmp/subs/adds/tst` + `b.cond` into a direct `jcc`, skipping flag stores
   when a lookahead shows they are dead; in-block register caching; skip the TBI mask when the base
   is SP; cheaper block-exit/IBTC paths.
2. Still-interpreted instructions: SHA-256 (`sha256h/h2/su0/su1`), `sys` (dc zva/cvau…), `tbl`,
   FRINT*, FCVTZU, UCVTF vector, unsigned/other shifts (SQSHL…), FMAX/FMIN, FMADD (needs FMA or
   double-double), LDXR/STXR exclusives, many rarer NEON ops. Use `MACLATOR_FALLBACK_HIST` on the
   GPU (`--type=gpu-process`) and renderer processes to see what matters.
3. Startup: work is ~10 processes on 4 cores; per-process translation. Ideas: pre-warm AOT,
   fewer VS Code processes/features, check Chromium/Electron flags (`--in-process-gpu`,
   `--disable-features=…`), persistent V8 code cache (don't wipe the profile dir).
4. Chromium-specific: run with a real profile; check `about:blank` page target had `url ""`
   for a long time in one run (renderer for the initial page slow to start) — investigate.
5. GPU: everything is software raster/compositing (`--disable-gpu`). A Metal/IOKit thunk layer
   (design intent: thunks only at hardware boundaries) would be a big project; the hack has an AMD
   RX 460/560.
6. Robustness: stale-IBTC hazard if a thread only ever hits IBTC entries after invalidation
   (accepted risk); AOT profile saving only on graceful exit; concurrent builders write the same
   files (last writer wins, atomic rename); `MACLATOR_SPIN_DEBUG` for infinite-dispatch bugs.
7. Overcast / iOS-on-Mac apps: would need the iOS-on-Mac runtime and unencrypted binaries;
   FairPlay-encrypted App Store binaries are not runnable off Apple Silicon and circumventing DRM
   is out of scope. Suggested alternative: build your own UIKit "Designed for iPad" or Mac
   Catalyst app.

## Key bugs found & fixed (so you recognise symptoms)

- `--sysroot` was parsed but never used; dyld walks paths via dirfd (fixed with vpath tracking).
- Host rejects arm64 code signatures (errno 85) → emulate `fcntl` code-sign ops.
- libobjc aborts on `task_restartable_ranges_register` → patch mach_msg2 reply.
- Non-canonical address GP faults from TBI-tagged pointers → mask in interp and JIT.
- IOKit: MobileGestalt waits forever for `AppleDiagnosticDataAccessReadOnly` → fake service+iterator.
- libdispatch timers never fire → event-manager QoS lost on fired events.
- Two event-manager threads → libdispatch "Locking the manager should not fail".
- `bsdthread_ctl` EINVAL → libdispatch "_pthread_set_properties_self failed".
- 4K vs 16K pages → V8 allocator crash (`munmap` EINVAL) → 16K mmap.
- `posix_spawn` of arm64 helpers failed on the Intel kernel → re-launch through maclator.
- JIT cache exhausted / thrash from flush-all invalidation → per-unit invalidation, 4 GiB cache.
- Forked child spun forever (stale task port) → libc fork.
- Vacuous self-tests (fallback == interpreter) → report `native=` count.
- Wrong bit masks pinned the U bit for unsigned SIMD variants (umull/raddhn/ushr/ushll/uminv).
- AdvSIMD load/store were never routed to the SIMD translator (dispatched via `ldst`).
- LD4/ST4 element size is bits 11:10 (not 23:22).
- Two labels at the same address need a `nop` between (iced).
- Exec-mmap of AOT files triggers Gatekeeper dialogs → copy into the code cache.
- Chromium hang: profiler `thread_suspend` (msg id 3605) → no-op shim.

## GPU passthrough (Metal → host GPU) — added 2026-09-29

`maclator --gpu ./app` routes the guest's Metal API to the host's real GPU (AMD RX 460 on the hack).
Verified: `tests/guest/metaltri` (clear + triangle + readback, `STRESS=1` for ANGLE-like load) and
Chromium 157 with ANGLE-Metal (`chrome://gpu`-equivalent via DevTools `SystemInfo.getInfo`: GPU compositing,
rasterization, WebGL, WebGPU, Graphite all enabled). Benchmark (animated canvas+transform page, hack):
software 1.4 fps (median frame 1000 ms) → GPU 28 fps (18 ms).

Launch Chromium with acceleration: `~/bin/chromium-gpu [url]` on the hack (source: `gpu/chromium-gpu`; uses `--gpu --gpu-only --type=gpu-process` so only the GPU process loads the bridge — giving every helper the bridge starved the renderers and made clicks unresponsive; flags: `--use-angle=metal --use-gl=angle
--enable-gpu --ignore-gpu-blocklist --disable-gpu-sandbox --disable-gpu-watchdog --no-sandbox`).

Design (all in `gpu/` + `crates/maclator/src/gpu.rs`):
- `gpu/mclmetal.m` → **guest** arm64 `libmclmetal.dylib`, injected with `DYLD_INSERT_LIBRARIES` (maclator does this
  for `--gpu`, then the shim `unsetenv`s it). It interposes `MTLCreateSystemDefaultDevice/MTLCopyAllDevices` and
  returns `MCLProxy` (NSProxy) objects. `forwardInvocation:` marshals every message (binary plist) using the host's
  method type encodings; guest-side `MTL*Descriptor` objects are serialized by property introspection; blocks
  (completion handlers) are registered and invoked later from a guest pump thread (`op 3` = wait for event).
- `gpu/mclbridge.m` → **host** x86-64 `libmclbridge.dylib`, dlopen'ed by maclator (`gpu.rs`). Handle table of real
  Metal objects, calls executed with `NSInvocation`. Guest and host share one address space, so raw pointers
  (`setVertexBytes`, `getBytes`, `MTLBuffer.contents`) just work.
- Transport: `svc #0x80` with x16 = `0x4D43` (`SYS_MCL_HOSTCALL`), x0=op x1=request ptr x2=len → x0=reply ptr x1=len.
- `gpu/build.sh` builds everything (also a native arm64 *loopback* build, `out/metaltri_loop`, which runs shim+bridge
  in one process on an Apple Silicon Mac — debug marshalling there first, e.g. with `NSZombieEnabled=YES`).
- Install on the hack: `libmclbridge.dylib` + `libmclmetal.dylib` in `~/bin/gpu/` (also searched: next to maclator,
  `~/.maclator/gpu`, `$MACLATOR_GPU_DIR`). Env: `MCL_TRACE=1` (log every message), `MCL_STATS=1` (per-5 s RPC table
  in the GPU process log), `MCL_NIL=1` (debug: no device).

Hard-won constraints (do not regress):
1. **Host frameworks must never run on a guest thread.** The kernel has one special reply port per thread; guest
   libxpc and host libxpc both cache/recycle it → guest XPC dies with `MACH_RCV_INVALID_NOTIFY` (0x10004007),
   seen as libdispatch "Unexpected error from mach_msg_receive" BRK in LaunchServices/CFPreferences. `gpu.rs` gives
   every guest thread its own bridge thread.
2. Host CoreFoundation/Metal must be initialised **before the guest starts** (`--gpu` preloads the bridge and runs
   `mcl_warmup` on a scratch thread); initialising them mid-run disturbed guest XPC too.
3. Never pass `DYLD_INSERT_LIBRARIES=<arm64 dylib>` to host processes (`spawn.rs` scrubs it; maclator re-injects).
4. `dispatch_data_t` is toll-free bridged to NSData — check for it before NSData (`newLibraryWithData:`).
5. Reply plists must be parsed from a private copy (parsed objects can reference the source bytes).
6. Chromium's GPU watchdog / objc-zombie crashes look like `str wzr,[xzr]` in the Chromium Framework
   (`--disable-gpu-watchdog`; zombies were my refcount bugs). `--dump-on-fault` now survives the guest resetting
   SIGSEGV and marks the faulting thread `(THIS THREAD)`.

Known gaps / next steps: presentation is via IOSurface (`newTextureWithDescriptor:iosurface:plane:` works — IOSurface
arguments are currently sent as unsupported and need the id→`IOSurfaceLookup` translation if the browser-side
display path needs it); `CAMetalLayer` (apps that present directly, not via Chromium) is not bridged;
`MTLFunctionConstantValues`, argument buffers, `MTLSharedEvent` listeners and NSURL-based APIs are only partly covered;
per-call cost is dominated by plist marshalling in the emulated guest (batching / binary protocol is the next
performance lever); compile time of shaders is on the host and is one-time (~2.7 s for ANGLE's library);
apps needing `--disable-gpu-watchdog`-like relief may still hit their own hang detectors during long inits.

## Session log summary (chronological)

1. Obtained arm64e dyld cache (macOS 26.6.2) on the hack via `ipsw` partial IPSW extraction
   (26.7/25G229 not indexed). 2. Made fastfetch, then neatvi, run on the hack. 3. IOKit shim,
   FP16 NEON, dispatch timers, single event manager → AppKit calculator. 4. 16K pages, arm64
   child spawn, libc fork, precise invalidation → VS Code Electron boots. 5. Long performance
   campaign (native NEON/atomics, AOT for libs, invalidation) using VS Code as the benchmark.
6. `maclator ./binary` auto-sysroot. 7. Chromium snapshot downloaded to Downloads (both Macs),
   thread-suspend shim, Gatekeeper fix → Chromium renders.

Memory notes for Claude Code sessions live in
`~/.claude/projects/-Users-vihaannathan-Desktop-Maclator/memory/` (`maclator-architecture.md`,
`maclator-hack-testing.md`).
