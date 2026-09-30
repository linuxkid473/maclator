# Performance work and measurements

All numbers: Intel Core i5-4440 (Haswell, 4 cores), 12 GB, macOS 26.7.

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
