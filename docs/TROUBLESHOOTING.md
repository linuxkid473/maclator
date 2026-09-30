# Troubleshooting

| Symptom | Cause / fix |
|---|---|
| dyld fails opening `/System/Library/dyld/dyld_shared_cache_arm64e` (or similar at startup) | No sysroot. Run `scripts/fetch-sysroot.sh` or pass `--sysroot DIR`; see [SETUP.md](SETUP.md). |
| Immediate crash with `sandbox initialization failed` | Electron/Chromium apps need `--no-sandbox`. |
| App aborts with "GPU … usable. Goodbye." / `gpu_data_manager_impl_private.cc` | The app's GPU process cannot start without a Metal device. Launch with the GPU bridge (`--gpu --gpu-only --type=gpu-process` + ANGLE flags, see [USAGE.md](USAGE.md)) or add `--disable-gpu`. |
| `maclator: cannot load GPU bridge …` | `libmclbridge.dylib`/`libmclmetal.dylib` not found. Install to `~/bin/gpu/` (`scripts/install.sh`) or set `MACLATOR_GPU_DIR`. |
| Chromium runs at ~1 fps after the GPU bridge is enabled | Often not a GPU problem: a window that is not visible (`document.visibilityState == "hidden"`) throttles requestAnimationFrame. For benchmarks add `--disable-backgrounding-occluded-windows --disable-renderer-backgrounding --disable-background-timer-throttling`. Also check `MCL_STATS=1` output for `remote exception` lines, and that the GPU process did not crash ("GPU process exited unexpectedly" in the log; Chromium then falls back to software). |
| Very slow first launch, then fast | Expected: code is translated on the fly. Quit normally so the translation cache is saved. The cache is invalidated by every Maclator update that changes code generation. |
| Second launch is slow again | The previous run was killed or crashed (no cache saved), or another Maclator build with a different `TRANSLATOR_VERSION` overwrote/discarded the cache. |
| macOS shows "Apple could not verify …" dialogs | Something mapped translation-cache files executable; never do that (the cache is loaded by copying). If you see it for a guest binary, re-sign it ad hoc: `codesign -f -s - file`. |
| Guest crashes at a `str wzr, [xzr]` or similar | A guest-side abort (ObjC/Chromium `CHECK`). Run with `--dump-on-fault` and read `/tmp/maclator-fault-<pid>.log`; `kill -USR1 <pid>` dumps every guest thread of a hung process. |
| Process hung at 0 % CPU | Use `kill -USR1 <pid>` (thread dump). A rare hang was seen once when a multithreaded Chromium GPU process forked (child stuck in malloc); relaunch. |
| Everything is slow | Check `Activity Monitor`: Chromium/VS Code are ~10 processes. Use the "lean" Chromium flags, more cores, and let the translation cache warm up. `--interp` is 10–100× slower and only for debugging. |
| Suspected JIT bug | Compare with `maclator --interp ./app`; run `maclator --selftest-jit blocks 200000`; see [DEBUGGING.md](DEBUGGING.md). |
| Wrong colours / missing rendering with `--gpu` | Try `MCL_NOFAST=1`, `MCL_OLDDESC=1`, `MCL_NOBATCH=1` (guest env) to bisect the bridge fast path, and `MCL_TRACE=1` for a message trace ([GPU.md](GPU.md)). |
