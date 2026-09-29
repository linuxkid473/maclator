# Maclator — handoff / continuation notes

_Last updated 2026-09-29 (evening). See `git log` for the latest commit; the GPU fast path and the JIT register cache landed after `c09b49c`._

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
not to pursue DRM circumvention), audio, most IOKit hardware, sandboxed apps, Swift-heavy Apple apps.
GPU/Metal is bridged to the host GPU (`--gpu`, see "GPU passthrough"); without it everything renders in
software.

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
dialogs for every file). Bump `TRANSLATOR_VERSION` (currently **15**) whenever codegen changes. Every
version bump invalidates the *whole* cache (files are shared between binaries: two builds with different versions on
the same machine keep overwriting each other's cache, so A/B-test translators with `--no-aot`).
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
maclator --selftest-jit fuse   200000   # (flag setter, b.cond | csel) pairs: compare+branch/select fusion
maclator --selftest-jit fuse2  200000   # [setter, b.cond, A, B, svc] with A/B overwriting all flags, run to the svc:
                                        #   exercises dead-flag-store elimination (prints dead_flag_skips)
maclator --selftest-jit blocks 300000   # random straight-line blocks over a small register set (reuse!) incl.
                                        #   loads/stores/pre/post-index writeback: exercises the register cache
```
`ldst` reports ~13 mismatches, all `ldr <v>, literal` (`$+0x…`): a harness artifact (the literal pool is outside the test
page); the pre-change tree shows the same. Sensitivity check of `blocks`: removing one `rc_inval` gives ~60 mismatches
per 200k blocks. All three new classes also pass on the hack's real Haswell.
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

### JIT code quality work (2026-09-29 evening; `TRANSLATOR_VERSION` 13→15)
Three changes in `jit/emit.rs`, measured with `bench/bench.c` (deterministic, `--no-aot`, hack; ms):

| kernel | before | after all three |
|---|---|---|
| qsort 1.5M | 759 | 610 |
| sieve 30M | 721 | 683 |
| crc32 80 MB | 888 | 539 |
| list walk | 576 | 574 (latency bound) |
| matmul 300 | 639 | 595–630 |
| memops | 489 | 486 |
| **total** | **4126** | **3520–3660 (−11…−15 %)** |
Chromium JS test (fib(27)+sort+loop in the renderer, warm AOT): ~1600 → ~1450 ms. VS Code (`bench/vscode-run.sh`,
`typebench.py`, warm process): in-page 3e6 loop (`--jitless`) 708 → 545 ms, median frame after a keystroke 38 ms,
key dispatch median 47 ms, idle frame 29 ms (the older "~100 ms" figure above was measured under other conditions
and is not directly comparable).

1. **Compare + branch/select fusion.** A flag-setting instruction (`adds/subs/ands/bics`, imm/shifted/extended) records
   `Unit::fuse` (which x86 op produced the flags); if the *next* instruction is `b.cond` or `csel/csinc/csinv/csneg`
   it branches/`cmov`s on the still-live x86 flags instead of re-deriving the condition from the four NZCV bytes
   (`x86_cc` maps ARM conditions per producer kind; add-HI/LS and logic-carry conditions are constant/unmappable and
   fall back). The stored NZCV bytes are still written unless (3) applies.
2. **Write-through guest register cache** (`RegCache`, `CREGS = rbx, rbp, r12, r13, r14` — callee-saved x86 registers the
   JIT never used; `maclator_jit_enter` already saves them). All guest GPR access goes through `ldx/ldw/ld_nf/stx/st_imm`
   plus three base-register writeback sites; each read/write updates a per-block, compile-time map guest reg → x86 reg.
   Memory stays authoritative (every write is still stored), so faults, signals and helper calls see current state;
   the cache only removes reloads (store-forwarding latency on dependent chains). It is cleared at block start and after
   interpreter fallbacks; after any internal label is created in an instruction (`new_label()` sets `rc_frozen`)
   nothing new is cached in that instruction because code after a join may run on paths that did not execute the
   caching code. Allocation is driven by a 10-instruction **lookahead** (`Unit::look`): a register is only cached
   when a later instruction of the block names it (otherwise the extra moves are pure overhead — measured on the
   sieve loop). Policy is performance-only; correctness never depends on it.
3. **Dead flag store elimination.** For `setter; b.cond` where both successors overwrite NZCV before reading it
   (`flags_dead_at`: straight-line scan ≤ 24 instructions, follows ≤ 3 unconditional `b`), the four `setcc` stores
   are skipped (`Unit::dead_flags`). Guards: the branch must not be a unit leader, must fit in the block, and
   `MACLATOR_JIT_DISABLE=branch` disables it.
Ideas not done: carry the register cache across chained blocks/loops (loop-carried values still reload at every
iteration), keep flags in `lahf`/`seto` form, fuse `cmp + cset`, use cache registers as direct operands.
One unexplained event: a single Chromium launch right after the AOT version bump hung at 0 % CPU (parent in `wait4`,
forked child in libmalloc after `fork` from a multithreaded GPU process); ~25 later launches of both the old and new
binary were fine. Suspected fork-in-multithreaded-process/malloc lock, i.e. pre-existing; keep an eye on it.

### Chromium configuration (hack, `bench/chromium-cfg.sh`; 4000-row DOM page load, idle, RSS)
| flags | processes | page load | RSS |
|---|---|---|---|
| default | 13 | 5.4–6.7 s | 4.5 GB |
| `--renderer-process-limit=1` | 11 | 4.9–5.0 s | 5.4 GB |
| + `--disable-extensions --disable-sync --disable-background-networking --disable-component-update --disable-features=Translate,OptimizationHints,MediaRouter,AutofillServerCommunication,SpareRendererForSitePerProcess,GlobalMediaControls` | **10** | **3.4–3.6 s** | 5.2 GB |
`--app=` (no toolbar) gave nothing; `--in-process-gpu` is not a win (same process count in practice, slower load, one
process at 100 % idle CPU). These are now in `gpu/chromium-gpu` (deployed to `~/bin/chromium-gpu`; old copy
`chromium-gpu.orig`). V8's JIT tiers are on in Chromium (3e6-iteration loop ≈ 120 ms in the renderer versus ~700 ms
`--jitless`).

## Remaining hot spots / next steps (rough priority)

1. **JIT code quality** (still the biggest lever): the first round (fusion, dead flag stores, a write-through
   register cache inside a block) is done, see "JIT code quality work". Remaining: registers still reload at every
   block boundary (a loop iteration is one block), the TBI mask (`shl/shr 8`) on every access, block-exit/IBTC cost,
   flags kept as four bytes.
2. Still-interpreted instructions: SHA-256 (`sha256h/h2/su0/su1`), `sys` (dc zva/cvau…), `tbl`,
   FRINT*, FCVTZU, UCVTF vector, unsigned/other shifts (SQSHL…), FMAX/FMIN, FMADD (needs FMA or
   double-double), LDXR/STXR exclusives, many rarer NEON ops. Use `MACLATOR_FALLBACK_HIST` on the
   GPU (`--type=gpu-process`) and renderer processes to see what matters.
3. Startup: work is ~10 processes on 4 cores; per-process translation. Ideas: pre-warm AOT,
   fewer VS Code processes/features, check Chromium/Electron flags (`--in-process-gpu`,
   `--disable-features=…`), persistent V8 code cache (don't wipe the profile dir).
4. Chromium-specific: run with a real profile; check `about:blank` page target had `url ""`
   for a long time in one run (renderer for the initial page slow to start) — investigate.
5. GPU: done for Chromium via the Metal bridge (see "GPU passthrough"); apps that present through
   `CAMetalLayer`/`MTKView` directly still need that bridged.
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

## GPU passthrough (Metal → host GPU) — added 2026-09-29, fast path added the same evening

`maclator --gpu ./app` routes the guest's Metal API to the host's real GPU (AMD RX 460 on the hack).
Verified: `tests/guest/metaltri` (clear + triangle + readback, `STRESS=1` for ANGLE-like load) and
Chromium 157 with ANGLE-Metal (GPU compositing, rasterization, WebGL, WebGPU, Graphite enabled).

Launch Chromium with acceleration: `~/bin/chromium-gpu [url]` on the hack (source: `gpu/chromium-gpu`; uses
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
- Install on the hack: `libmclbridge.dylib` + `libmclmetal.dylib` in `~/bin/gpu/` (also searched: next to maclator,
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
10. `document.visibilityState` of the hack's Chromium window is often `hidden` (no display attached to the active
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
