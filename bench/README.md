# Benchmarks and test drivers

Everything here is driven from a dev Mac against the Intel test Mac over ssh:

```bash
export HACK=user@your-intel-mac.local     # ssh target (key auth)
```

The Python scripts run **on the Intel Mac** (Python 3.9 from the Command Line Tools is enough, no packages;
`ws.py` is a tiny DevTools websocket client). Copy them to `~/maclator-run/` there (`gpu-exp.sh` and friends do this
for the files they need).

| file | what |
|---|---|
| `bench.c` | arm64 CPU micro-benchmark (qsort, sieve, crc32, list walk, matmul, memops). Build with `clang -arch arm64 -O2 bench.c -o bench && codesign -f -s - bench`, run `maclator --no-aot ./bench`. Use `--no-aot` when comparing translator builds: AOT caches are keyed by `TRANSLATOR_VERSION` and different builds keep invalidating each other. |
| `branch.c`, `branch-arm64.S` | deterministic CBZ/CBNZ and TBZ/TBNZ loop, with a checked checksum and wall/thread CPU times. Build with `clang -arch arm64 -O2 branch.c branch-arm64.S -o branch-bench && codesign -f -s - branch-bench`. This measures one code-generation path, not whole-app speed. |
| `compare-cpu.py` | bounded A/B CPU benchmarks, alternate run order and use `--no-aot`, nice priority 10 and a per-process deadline. Example: `python3 compare-cpu.py /path/to/old-maclator /path/to/new-maclator /path/to/branch-bench --sysroot /path/to/sysroot --rounds 7 --output /tmp/branch-results.json`. Inspect individual runs as well as medians; do not infer a speedup from noisy samples. |
| `native-ratio.py` | same-host x86 native / ARM translated benchmark. Compile `bench.c` twice with `clang -O2 -arch x86_64` and `clang -O2 -arch arm64`, sign the ARM binary, then run with `--native`, `--guest`, `--baseline`, `--optimized`, `--sysroot`, `--rounds 9`, `--output results.json`. Stores hashes, raw interleaved runs and checksums; excludes a full warmup round. Ratio is native median time / translated median time, with per-kernel, geometric-mean, total and worst-case results. Never substitute JIT coverage for runtime speed. ARM and x86 system-library implementations also differ. |
| `gpubench.py <secs> <arcs> [js]` | animated 2D-canvas page in the running Chromium (DevTools on :9333): median frame time / fps, optional JS compute timing. Uses focus emulation because the test Mac's window is often reported `hidden` (which throttles rAF). |
| `gpu-exp.sh "ENV=1 ..." [secs]` | restart `chromium-gpu` with `MCL_STATS=1` plus extra env, run `gpubench.py`, print the guest-side GPU profile lines. |
| `jsbench.sh <suffix>` | same, with `~/bin/maclator.<suffix>` as the emulator (A/B of builds). |
| `cfgbench.py`, `chromium-cfg.sh` | Chromium command-line experiments: time to DevTools, page-load time of a 4000-row DOM, process count, RSS, idle CPU, JS timing. |
| `vscode-run.sh`, `typebench.py` | VS Code launch + keystroke-latency driver (DevTools on :9222) + graceful close. |
| `closebrowser.py` | `Browser.close` over DevTools: a graceful exit saves the AOT profile. |

JIT correctness tests are built into the binary, see [../docs/DEBUGGING.md](../docs/DEBUGGING.md).

For same-binary feature A/B, pass `--variants /path/to/variants.json` to
`native-ratio.py`. The JSON maps distinct labels to a `runtime` path and an
optional `environment` object with string-valued `MACLATOR_` overrides, e.g.
`{"memory_budget":{"runtime":"/path/to/maclator","environment":{"MACLATOR_JIT_REGISTER_BUDGET":"0"}}}`.
Every extra variant is interleaved with native and baseline runs; its environment
and binary hash are recorded. Three rounds are useful for exploration; use more
rounds for decisions and inspect workload-specific regressions.
