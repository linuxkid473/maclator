# Debugging, profiling and JIT self-tests

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
```
`ldst` reports ~13 mismatches, all `ldr <v>, literal` (`$+0x…`): a harness artifact (the literal pool is outside the test
page); the pre-change tree shows the same. Sensitivity check of `blocks`: removing one `rc_inval` gives ~60 mismatches
per 200k blocks. All three new classes also pass on the test Mac's real Haswell.
Prints `ok= mismatch= skipped= native=`. **Always check `native=`**: an instruction that falls
back to the interpreter trivially matches it (that hid four bugs for a while). Masks must leave
the U bit (29) and Q bit (30) free when testing both variants. Known accepted mismatch: the sNaN
case above.
