# Xcode 27 on Maclator: experimental compatibility work

Tested locally on 2026-10-13 with macOS 26.7.1 (x86_64), Xcode 27.0
(27A266a), and the macOS 26.6.2 arm64e shared cache. The original Apple Xcode
executables and package payloads were kept unchanged.

## Runtime fixes

- Workqueue requests use `a[2]` (Darwin `affinity`), not `a[1]` (`item`),
  for the requested thread count. The old behavior deadlocked `ld` in
  `dispatch_apply` after creating one worker for a 31-worker request.
- `bsdthread_create` waits for the new host thread to publish its Mach port in
  guest TSD before returning. The old asynchronous setup could cause libpthread
  to treat thread creation as failed, free its stack, and crash the new thread.
- `PTHREAD_START_SUSPENDED` is `0x20000000`; `0x02000000` is `SETSCHED`.
- `--no-aot` now propagates to re-launched ARM64 children.
- `--dump-on-fault` reports the host fault address and current CPU even when
  the global thread registry is locked.

The thread ABI matches Apple's [libpthread source](https://github.com/apple-oss-distributions/libpthread/blob/main/src/pthread.c).

## Local prerequisite payload overlay

The native PackageKit service receives Maclator's real host identity. It rejects
first-launch installation because Maclator does not hold Apple's private
installation entitlements; sudo does not grant those entitlements. This is
separate from the ARM-only `IDEInstallService.xpc`, which is not the service
used for the initial component installation.

The optional `--overlay-root ROOT` makes existing, original prerequisite
payload files visible at these guest locations:

- `/Library/Developer/PrivateFrameworks`
- `/Library/Developer/DeviceKit`
- `/Library/Developer/CoreSimulator`
- `/Library/Developer/CoreDevice`
- `/Library/Apple`
- `/System/Library/CoreServices/CoreTypes.bundle`

Extract Xcode's embedded CoreTypes, MobileDevice, MobileDeviceDevelopment, and
XcodeSystemResources packages without executing package scripts. Place their
payloads beneath ROOT using the original installation locations. MobileDevice
and MobileDeviceDevelopment use `/Library/Apple` as their installation root.
No receipt, version, license state, or successful installation response is
invented. Guest file existence and version checks read the actual Apple files.

The overlay is opt-in and forwarded to translated children. It handles relative
directory lookups, duplicate/inherited descriptors, normalized paths, F_GETPATH,
and FULLPATH-only attribute requests. Missing files fall back to the host.
It is a path overlay, not a sandbox or complete union filesystem: directory
enumeration is not merged, mixed FULLPATH attributes are unchanged, and writes
retain normal host semantics. Do not run package scripts as if the entire
system were isolated by this flag.

```sh
maclator --no-aot --overlay-root ROOT Xcode.app/Contents/MacOS/Xcode
```

## Verified and remaining behavior

The welcome window and project editor opened. Xcode's Swift build service built
a real macOS ARM64 command-line project; the resulting program ran through
Maclator and printed its expected result. Independent C and Swift compilation,
linking, and execution also passed.

The host launchd still cannot directly execute ARM-only XPC services such as
Xcode's Git helper, CoreDeviceService, and CoreSimulatorService. Simulator,
device deployment, GUI debugging, source-control integration, and arbitrary
project types are not established by the command-line project test.

Regression sources: `tests/guest/thread_create.c`, `spawn_options.c`, and
`overlay_paths.c`; the spawn propagation check is `tests/check_spawn_options.py`.
Run Rust unit tests with `cargo test -p maclator`.
