# macOS application entries

The user LaunchAgent runs a lightweight scan at login and every 60 seconds.
It detects ARM-only executable application bundles and creates independently
signed x86_64 launcher bundles in `~/Applications/Maclator Apps`. Open these
entries normally from Finder, Spotlight, Dock, or `open`; they launch Maclator
automatically. The source application is read without changing its bundle,
signature, minimum OS requirement, or quarantine attributes.

This is a user-space integration. It does not intercept direct execution of
the original ARM path, or host launchd/XPC services. macOS has no supported
third-party equivalent of Linux binfmt for this use. Standard scan roots are
`/Applications`, `~/Applications`, and two directory levels of `~/Downloads`;
additional paths can be registered explicitly. Embedded helpers are excluded.
Universal apps with any x86_64 slice, encrypted Mach-Os, non-executable file
types, and ARM64e-only applications do not receive entries.

## Build and install

On the tested Intel macOS 26 host, with Rust and Command Line Tools:

```sh
bash integration/build.sh
python3 integration/maclator_agent.py install \
  --runtime /absolute/path/to/maclator \
  --sysroot /absolute/path/to/arm64-sysroot \
  --app /absolute/path/to/App.app
```

Install `integration/build/libmclappicon.dylib` next to the runtime. The
`install` command copies the native launcher and discovery script into
`~/Library/Application Support/Maclator`, installs the LaunchAgent in
`~/Library/LaunchAgents`, and adds `~/bin/maclator-apps`. No sudo is required.

```sh
maclator-apps add /directory/containing/apps
maclator-apps scan
maclator-apps status
maclator-apps uninstall
```

Configuration is `~/Library/Application Support/Maclator/integration.json`;
per-app profiles supply runtime arguments and environment variables. The
optional `application_arguments` array is appended after the target executable,
so browser flags are passed to the translated browser rather than Maclator.
Setting `register_browser_types` copies the source browser's declared URL and
document types to the native entry; it does not change default handlers.
Protected browser defaults require the supported `NSWorkspace` API from an
application context and may require system user consent. Xcode's
profile uses its actual prerequisite payload overlay and `DEVELOPER_DIR`.
The default GUI profile uses `--no-aot` to avoid unbounded background cache
builds. GPU bridging is enabled per verified application profile, rather than
adding GPU initialization to every app/helper.

## Icon and launch behavior

The native entry copies the original icon resource. `--app-icon PATH` injects
the optional ARM `libmclappicon.dylib` into the top-level translated application.
After AppKit finishes launching it sets `applicationIconImage` directly from
the resource. It does not load AppKit into command-line tools. Exec/spawn strips
the icon shim and icon environment variable from ARM and native children; GPU
shims continue to be reinjected only where the existing GPU options require.

This fixes the entry's Finder/Spotlight icon and the running Dock/application
icon. It does not change the architecture badge on the original application
bundle, or all application-specific images obtained through `NSWorkspace`.

The entry keeps a shared lock for its lifetime; scans defer replacing or
pruning that entry while it is running. Source applications that disappear or
become native/universal are pruned on a later scan. Generated bundles are ad-hoc
signed with a separate bundle identifier; they do not copy Apple's identity.

Explicit `open -a /path/to/Entry.app /path/to/document` and URL events are sent
to the translated process. Default document or URL handlers are not changed by
discovery. Reopening the entry activates the existing process.
Quit the translated app normally. The native entry exits when its guest exits;
quitting the entry also terminates its own guest.

## Verification

`integration/test_discovery.py` covers thin/FAT/FAT64 headers, both table
endiannesses, late x86_64 slices, encryption, executable types, malformed
metadata, source preservation, name collisions, running-entry locks, and stale
entry cleanup. `check_launcher.py` uses a disposable ARM AppKit app with the real
native entry and `open(1)` to verify launch, icon application, document/URL
delivery, no duplicate guest, and update deferral. `check_runtime.py` checks the
combined JIT, pthread, spawn, overlay and shim-child behavior with deadlines.

The deployment on this host also passed a fresh Swift Xcode project build and
execution. The user confirmed that Xcode's Dock icon was normal and its window
usable. This does not resolve Xcode's ARM SKAgent/CoreSimulator/CoreDevice host
XPC limitations.
