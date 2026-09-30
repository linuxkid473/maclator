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
| `gpubench.py <secs> <arcs> [js]` | animated 2D-canvas page in the running Chromium (DevTools on :9333): median frame time / fps, optional JS compute timing. Uses focus emulation because the test Mac's window is often reported `hidden` (which throttles rAF). |
| `gpu-exp.sh "ENV=1 ..." [secs]` | restart `chromium-gpu` with `MCL_STATS=1` plus extra env, run `gpubench.py`, print the guest-side GPU profile lines. |
| `jsbench.sh <suffix>` | same, with `~/bin/maclator.<suffix>` as the emulator (A/B of builds). |
| `cfgbench.py`, `chromium-cfg.sh` | Chromium command-line experiments: time to DevTools, page-load time of a 4000-row DOM, process count, RSS, idle CPU, JS timing. |
| `vscode-run.sh`, `typebench.py` | VS Code launch + keystroke-latency driver (DevTools on :9222) + graceful close. |
| `closebrowser.py` | `Browser.close` over DevTools: a graceful exit saves the AOT profile. |

JIT correctness tests are built into the binary, see [../docs/DEBUGGING.md](../docs/DEBUGGING.md).
