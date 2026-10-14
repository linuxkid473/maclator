# Debugging, profiling and JIT self-tests

## Debugging / profiling toolbox

Flags & env: `--interp` (no JIT), `--no-aot`, `--trace` (syscalls/Mach msgs/paths), `--trace-file`
(per-process `/tmp/maclator-trace-<pid>.log`), `--dump-on-fault` (`MACLATOR_DUMP_ON_FAULT=1`,
guest regs + disasm to `/tmp/maclator-fault-<pid>.log`), `MACLATOR_STATS=1`,
`MACLATOR_FALLBACK_HIST=1` (histogram of instructions falling back to the interpreter, written
to `/tmp/maclator-hist-<pid>.txt`, digit-normalised so exact forms show),
`MACLATOR_JIT_THRESHOLD`, `MACLATOR_JIT_DISABLE=dpimm,branch,ldst,dpreg,simd,…`,
`MACLATOR_JIT_FMA=0` (disable guarded hardware scalar FMA),
`MACLATOR_JIT_OPERANDS=0` (disable direct cached ALU RHS operands),
`MACLATOR_JIT_LOOP_CACHE=0` (disable fixed GPR/vector mappings across self-loop backedges),
`MACLATOR_JIT_REGISTER_BUDGET=0` (use the memory counter instead of the reserved r14 counter),
`MACLATOR_JIT_BUDGET_COLD=0` (restore inline budget-exhaustion exits),
`MACLATOR_JIT_PAIR_FAST=1` (experimentally enable adjacent GPR-pair transfers through Cpu vector slots;
default off after a recursive-call performance regression),
`MACLATOR_JIT_CSEL_CHAIN=0` (disable live-flag reuse across consecutive conditional selects),
`MACLATOR_SPIN_DEBUG=1` (reports a dispatcher spinning on one pc), `MACLATOR_AOT_SYNC`,
`MACLATOR_NO_AOT_BUILD`, `MACLATOR_CACHE`, `MACLATOR_SYSROOT`.
Signals: `kill -USR1 <pid>` all guest threads (pc/regs/backtrace), `kill -USR2 <pid>` lock-free
register dump of the interrupted thread + JIT counters (works in forked children).
`maclator --jit-dump <hex insns…>` prints generated x86 hex for a block (disassemble via a `.byte`
file + `clang -c` + `objdump -d --x86-asm-syntax=intel`). `ipsw dyld a2s/disass` (on the test Mac) maps
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
maclator --selftest-jit branchcache 12000 # CBZ/CBNZ W/X and TBZ/TBNZ, cached/uncached registers,
                                        #   zero registers, random NZCV and successor PC comparison
maclator --selftest-jit fma 24000         # 32768 edge triples + 24000 random cases, f32/f64,
                                        #   all four FMA modes, GPR cache across fast/slow paths,
                                        #   NaN signs/priority, FPCR guard, upper-lane clearing
maclator --selftest-jit operandcache 24000 # valid ALU blocks with aliases, eviction, W/X and XZR
MACLATOR_JIT_FMA=0 maclator --selftest-jit fma 24000
MACLATOR_JIT_OPERANDS=0 maclator --selftest-jit operandcache 24000
```
The random `ldst` harness excludes PC-relative literal loads because it does not model a literal pool.
The old harness could fault in the reference before running JIT code; that was reproduced with the installed baseline.
Use controlled Mach-O or executable fixtures for literal-load coverage. Sensitivity check of `blocks`: removing one `rc_inval` gives ~60 mismatches
per 200k blocks. All three new classes also pass on the test Mac's real Haswell.
Prints `ok= mismatch= skipped= native=`. **Always check `native=`**: an instruction that falls
back to the interpreter trivially matches it (that hid four bugs for a while). Masks must leave
the U bit (29) and Q bit (30) free when testing both variants. Known accepted mismatch: the sNaN
case above.

The `fma` suite compares against the reference interpreter. Non-default scalar
f32/f64 FMA, FMUL/FNMUL, FADD/FSUB and min/max now use exact controlled rounding
and accumulate the supported FPSR flags. Core tests compare finite results with
independent hardware rounding and analytical ARM cases. Default-mode FPSR, FP16,
division/sqrt, conversions, vector FP, traps and AFP modes remain incomplete.

`--selftest-jit fp-modes 10000` exercises 32768 non-default FPCR edges plus random
scalar instructions and GPR cache preservation. `--selftest-jit loopcache 2000`
checks whole finite loops, aliases, NaNs, non-default FPCR, repeated budget
yield/resume, exact memory writes and commpage helpers that clobber xmm8..15.
It also checks 48 deterministic aligned/unaligned pair loops, read-only loads,
base/destination aliases, writeback and whole-page memory at every checkpoint.
Disable the loop cache, pair fast path and register budget separately for A/B controls.

Each compiled block charges its instruction count against a 65536-instruction
entry budget. On exhaustion, chained code exits before the next block and
resumes at its external entry with authoritative memory state. The dispatcher
can then deliver asynchronous guest signals and observe cache invalidation.
The r14 implementation reserves that callee-saved host register for the budget,
leaving four guest GPR cache slots. Entry loads the budget and every exit writes
it back; C ABI helpers preserve r14. The cold-layout variant places exhaustion
tails after the common exit, letting the normal execution path fall through.
Loop-cache counters count code-generation attempts, including assembler retries;
they are not counts of unique executed blocks or a performance ratio.

Aligned 128-bit GPR/SIMD pairs and scalar vector loads/stores use a single
VMOVDQA access on Intel/AMD hosts with OS-enabled AVX. Read-only loads perform
no write or RMW. Other hosts/alignment retain ordinary accesses. Full FEAT_LSE2
is not advertised because its unaligned 16-byte-granule contract remains incomplete.
AOT version 22 records loop-cache, atomic-pair, register-budget, adjacent-pair,
cold-exit, conditional-select and packed-NZCV choices in its feature key.
Packed NZCV is enabled by default; `MACLATOR_JIT_PACKED_FLAGS=0` restores byte
stores. Optional effective-address, cached-store, BMI2 TBI, structure-load,
RET-index, scalar-writeback and GPR-writeback paths remain disabled by default
and occupy AOT feature bits 12 through 18 respectively. Each requires its
explicit `MACLATOR_JIT_*` environment value `1`; an opposite-mode AOT cache is
rejected. The complete variable names and status are listed in
[the contribution overview](../contributions/README.md).

Signal regression probes are under `tests/compatibility_matrix`; supported
asynchronous delivery includes ARM ucontext restoration, masks, nesting,
sigaltstack and selected SA_RESTART waits. Synchronous hardware faults and
a signal routed to a host-only helper while the guest is blocked still have
limitations. These tests do not establish all ARM application compatibility.
Guarded native instructions contain a compiled fallback, so the static `native`
counter does not measure the number of actual fast-path executions. Instruction
coverage is not a native-speed ratio; use `bench/native-ratio.py` for that.
