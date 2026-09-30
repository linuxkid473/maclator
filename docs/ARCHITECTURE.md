# Architecture

How Maclator is put together. (Older notes call the Intel test machine "the test Mac"; it is simply the Intel Mac the project is developed against.)

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
