# Setup

## Requirements

| | |
|---|---|
| Machine | Intel Mac (x86-64). Developed on a Haswell i5-4440 (4 cores, 12 GB). The JIT needs SSE2 plus SSSE3/SSE4.1 (any Mac since ~2008–2010 has them). More cores and RAM help a lot: Chromium and VS Code are ~10 processes. |
| macOS | 26 (Tahoe). Older releases are untested. The host's `/usr/lib/dyld` is a universal binary that contains the arm64e slice Maclator uses as guest dyld (`lipo -archs /usr/lib/dyld` should print `x86_64 arm64e`). |
| Tools | Xcode Command Line Tools (`xcode-select --install`), Rust (`curl https://sh.rustup.rs -sSf \| sh`). |
| Disk | ≈ 7 GB for the arm64 dyld shared cache, plus ≈ 1 GB for the AOT cache after heavy use. |

A second (any architecture) Mac is convenient for development: `cargo build --release --target x86_64-apple-darwin`
cross-compiles, and the `bench/` scripts drive an Intel test Mac over ssh. It is not required to *use* Maclator.

## 1. Build and install

```bash
git clone https://github.com/OWNER/maclator && cd maclator
scripts/install.sh
```

`scripts/install.sh` does the following (you can do it by hand):

```bash
cargo build --release                       # on an Intel Mac; add --target x86_64-apple-darwin when cross-compiling
install -d ~/bin ~/bin/gpu
install target/release/maclator ~/bin/maclator
gpu/build.sh                                # builds libmclbridge.dylib (x86-64) + libmclmetal.dylib (arm64 guest shim)
cp gpu/out/libmclbridge.dylib gpu/out/libmclmetal.dylib ~/bin/gpu/
```

Make sure `~/bin` is on your `PATH` (`export PATH="$HOME/bin:$PATH"` in `~/.zshrc`).

The GPU bridge libraries are only used with `maclator --gpu`; they are searched in `$MACLATOR_GPU_DIR`, next to
the `maclator` binary (and its `gpu/` subdirectory) and `~/.maclator/gpu`; the installer uses `~/bin/gpu/`.

## 2. Get the arm64 userland (the dyld shared cache)

Maclator needs the arm64e **dyld shared cache** of a macOS release for Apple Silicon: one big file set that
contains the whole arm64 system (Foundation, AppKit, Metal, libSystem, …). Apple does not ship it on Intel
Macs, so you download it once from an Apple Silicon macOS IPSW using [`ipsw`](https://github.com/blacktop/ipsw)
(it extracts just the cache from the remote IPSW; nothing else is downloaded):

```bash
scripts/fetch-sysroot.sh            # or: scripts/fetch-sysroot.sh 26.6.2
```

which is equivalent to:

```bash
brew install blacktop/tap/ipsw      # or download a release binary from GitHub
ipsw download ipsw --macos --version 26.6.2 --dyld --dyld-arch arm64e --extract-device Mac14,2 \
     --output ~/maclator-sysroot/25G83__Mac14,2
```

The directory must end up containing `dyld_shared_cache_arm64e`, `dyld_shared_cache_arm64e.01` … `.12` and the
`.a2s/.atlas/.map` files. Maclator finds it automatically, in this order: `$MACLATOR_SYSROOT`, `~/.maclator/sysroot`
(a directory, or a file containing a path), then the newest `~/maclator-sysroot/*/` that contains a
`dyld_shared_cache_arm64e`. `--sysroot DIR` overrides.

Which version? The cache used during development is **macOS 26.6.2 (25G83)** on a **26.7** host; the exact
matching build was not obtainable and it works anyway. Use the newest release that `ipsw` can fetch that is close
to your host version. A cache that is *newer* than the host kernel is more likely to hit unimplemented syscalls.

## 3. Check it works

```bash
maclator --selftest-jit dpreg 20000        # JIT vs interpreter; expect mismatch=0
maclator ./tests/guest/hello               # (build guest tests first, see below)
```

Build a tiny arm64 program with the Command Line Tools and run it:

```bash
cat > hello.c <<'EOF'
#include <stdio.h>
int main(void) { puts("hello from arm64"); return 0; }
EOF
clang -arch arm64 hello.c -o hello && codesign -f -s - hello
maclator ./hello
```

First launches are slow (seconds to minutes for big apps) because every code block is translated on the fly.
The translation cache in `~/Library/Caches/Maclator/aot/` speeds later runs up; it is only written when the
program exits normally.

## 4. Optional: GPU acceleration

Launch with `--gpu` (see [GPU.md](GPU.md) for what that does). For Chromium/Electron apps only the GPU process
should get the bridge: `--gpu --gpu-only --type=gpu-process`. Ready-made launchers are in [USAGE.md](USAGE.md).
The GPU bridge needs a working Metal device on the Intel Mac; on GPUs without Metal support it cannot work.

## Where things live

| path | what |
|---|---|
| `~/bin/maclator`, `~/bin/gpu/` | the emulator and GPU bridge libraries |
| `~/maclator-sysroot/<build>__<model>/` | arm64e dyld shared cache |
| `~/Library/Caches/Maclator/aot/` | persistent translation cache (override: `MACLATOR_CACHE`) |
| `/tmp/maclator-*.log` | fault dumps / traces when the corresponding flags are used |
