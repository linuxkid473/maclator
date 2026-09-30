# Usage

## Command line

```
maclator [options] ./arm64-binary [args...]

  --dyld PATH        arm64 dyld to use (default /usr/lib/dyld; $MACLATOR_DYLD)
  --sysroot DIR      directory with the arm64e dyld shared cache (else auto-discovered, see SETUP.md)
  --gpu              route the guest's Metal API to the host GPU
  --gpu-only SUBSTR  give the GPU only to guest processes whose arguments contain SUBSTR (e.g. --type=gpu-process)
  --interp           interpreter only (no JIT/AOT): slow, useful to rule out JIT bugs
  --no-aot           do not load or save the translation cache
  --trace            log syscalls / Mach messages / path redirections
  --dump-on-fault    on a crash write registers + disassembly to /tmp/maclator-fault-<pid>.log
```

More flags, environment variables and signals: [DEBUGGING.md](DEBUGGING.md).

Notes that apply to every app:

- Programs must be **arm64** Mach-Os (`file app` should say `arm64`); arm64e-only or FairPlay-encrypted
  (App Store / iOS-on-Mac) binaries do not work.
- Run **ad-hoc re-signed** copies if a binary complains about signatures: `codesign -f -s - file`.
- First launch of a big app is slow (translation happens on the fly). **Quit apps normally** (Cmd+Q, or a
  graceful `Browser.close` over DevTools): the translation cache (`~/Library/Caches/Maclator/aot/`) is written on
  a normal exit, and subsequent launches are several times faster. A killed process saves nothing.
- Electron/Chromium apps need `--no-sandbox` (the Chromium sandbox cannot initialise under Maclator).
- Every Maclator version bump that changes code generation invalidates the translation cache.

## Chromium (with GPU acceleration)

1. Download a Chromium snapshot for **Mac_Arm** and unzip it, e.g. into `~/Downloads/chromium-arm64`:
   `https://commondatastorage.googleapis.com/chromium-browser-snapshots/Mac_Arm/1706829/chrome-mac.zip`
   (revision 1706829 = Chromium 157.0.8079.0, the one this was developed with; other revisions may work).
2. Launch (installed to `~/bin` by `scripts/install.sh`):

```bash
chromium-gpu [url]                       # arm64 Chromium, GPU bridge, DevTools on 127.0.0.1:9333
CHROMIUM_DIR=/path/to/Chromium.app/Contents/MacOS chromium-gpu   # if it is somewhere else
```

The launcher (`gpu/chromium-gpu`) runs:

```bash
maclator --gpu --gpu-only --type=gpu-process ./Chromium --no-sandbox --disable-gpu-sandbox --disable-gpu-watchdog \
  --use-angle=metal --use-gl=angle --enable-gpu --ignore-gpu-blocklist \
  --renderer-process-limit=1 --disable-extensions --disable-sync --disable-background-networking --disable-component-update \
  --disable-features=Translate,OptimizationHints,MediaRouter,AutofillServerCommunication,SpareRendererForSitePerProcess,GlobalMediaControls \
  --user-data-dir=~/chromium-data --no-first-run --no-default-browser-check --remote-debugging-port=9333
```

Why these flags: only the GPU process loads the bridge (giving every helper process the bridge starved the
renderers); `--disable-gpu-watchdog` because long first-time shader compiles trip Chromium's watchdog; the
"lean" flags cut the process count from 13 to 10 and page load time by ~40 % on a 4-core machine.
Without a GPU: `maclator ./Chromium --no-sandbox --disable-gpu --user-data-dir=/tmp/chromium-data …`.

Check acceleration through DevTools: open `http://127.0.0.1:9333/json/version`, or in the browser `chrome://gpu`
(should report "Metal" ANGLE, hardware accelerated compositing/rasterization).

## VS Code

Download the **darwin-arm64** zip (`https://update.code.visualstudio.com/latest/darwin-arm64/stable`), unzip into
`~/vscode-arm64`, then:

```bash
vscode-gpu                       # installed by scripts/install.sh; VSCODE_DIR / VSCODE_DATA override locations
# software rendering variant:
cd ~/vscode-arm64 && maclator "./Visual Studio Code.app/Contents/MacOS/Code" --no-sandbox --disable-gpu \
    --js-flags=--jitless --user-data-dir=$HOME/vscode-data --disable-workspace-trust
```

`--js-flags=--jitless` was chosen during development for stability; you can experiment without it.
The integrated terminal may warn "unable to resolve shell environment"; that only affects its environment.

## balenaEtcher

```bash
etcher-gpu        # runs /Applications/balenaEtcher.app (arm64) with the GPU bridge and --no-sandbox
```

Without `--gpu` the app aborts ("GPU … usable. Goodbye"): Electron cannot start its GPU process without a Metal
device, so either use the launcher or add `--disable-gpu`. Flashing drives (raw disk access) has not been tested.

## Any other Electron/Chromium app

```bash
maclator --gpu --gpu-only --type=gpu-process ./App --no-sandbox --disable-gpu-sandbox --disable-gpu-watchdog \
  --use-angle=metal --use-gl=angle --enable-gpu --ignore-gpu-blocklist
```

## Plain command line tools

```bash
maclator ./fastfetch
ELECTRON_RUN_AS_NODE=1 maclator "…/Code" -e "console.log(process.version)"
```
