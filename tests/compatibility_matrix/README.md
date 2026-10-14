# Independent application compatibility probes

These probes exercise application behavior outside the instruction differential
tests. They do not modify installed applications or register a privileged
service. Directly launched children have deadlines and run at `nice 10`.
Self-owned launchd services have a separate, exact-path cleanup after their
client completes; no installed application's processes are terminated.

```sh
/usr/bin/python3 tests/compatibility_matrix/run_probes.py \
  --runtime ./target/release/maclator --jit-threshold 1 \
  --output ./test-results/compatibility/probes

/usr/bin/python3 tests/compatibility_matrix/run_loops_probe.py \
  --runtime /path/to/candidate/maclator --signals \
  --output ./test-results/compatibility/loops-signals

/usr/bin/python3 tests/compatibility_matrix/run_xpc_probe.py \
  --runtime /path/to/candidate/maclator --sysroot /path/to/arm-sysroot \
  --output ./test-results/compatibility/xpc

# Add two native-entry ARM-service cases and preserve the runtime for a later
# serialized test window. Preparation does not start clients or services.
/usr/bin/python3 tests/compatibility_matrix/run_xpc_probe.py \
  --runtime /path/to/candidate/maclator --wrap-arm-services --prepare-only \
  --sysroot /path/to/arm-sysroot \
  --output ./test-results/compatibility/xpc-wrapped

/usr/bin/python3 tests/compatibility_matrix/run_xpc_probe.py \
  --run-prepared ./test-results/compatibility/xpc-wrapped/results.json

/usr/bin/python3 tests/compatibility_matrix/inventory.py \
  --root /Applications --root "$HOME/Applications" \
  --root /path/to/Xcode.app \
  --output ./test-results/compatibility/inventory.json
```

`run_probes.py` builds and runs:

- C++ exception matching, stack unwinding and RAII destruction on four guest
  threads, with independent thread-local state.
- Foundation Objective-C exception handling and JSON work on 32 dispatch jobs.
- ARM machine code generated in a private mmap, rewritten sequentially and
  from another parked guest thread with instruction-cache invalidation.
- C11 atomic64 counters, atomic128 CAS invariants and release/acquire payload
  publication. Builds cover the compiler's Apple M1 target, an explicitly
  generic ARMv8 LL/SC target, and a generic ARMv8.1 LSE target. Darwin clang's
  default target is Apple M1, so omitting `-mcpu=generic` does not establish an
  independent LL/SC case.
- ARM GPR/SIMD pair loads on read-only mmap pages, base/output aliasing without
  writeback, immediate offsets, pre/post indexing, zero-register destinations,
  and writeback stores on writable pages. The atomics probes also stress
  atomic128 stores against concurrent loads, separating LDP/STP semantics from
  the CAS128 path.

`run_loops_probe.py` compiles identical finite integer loops for x86 and ARM.
The optional periodic SIGALRM runs during hot loops. Passing requires equal
checksums across every case, successful completion, and nonzero handler
delivery in signal-enabled cases. This checks correctness and interruptibility;
it is not a substitute for the seven-kernel native-performance benchmark.

`run_xpc_probe.py` creates four self-owned signed app/service combinations:
x86/x86, ARM/ARM, ARM/x86, and x86/ARM. The service only echoes a test string.
The native x86 pair is a positive control. The mixed cases distinguish a guest
NSXPC transport/bundle lookup failure from inability to launch an ARM service
in the host kernel. Failures remain recorded in JSON; the runner intentionally
continues after one case fails.

`--wrap-arm-services` adds ARM/ARM and x86/ARM cases whose self-owned service
entry is a native x86 executable. That entry does not initialize Foundation or
libxpc, mutate the environment, or fork: it logs key presence and `execl`s the
saved runtime with the ARM service payload. The service separately logs guest
entry, listener resume and echo receipt. Preparation snapshots the runtime and
records its SHA-256; running a prepared manifest refuses a changed snapshot.
This tests whether the service endpoint is usable after that exec; preservation
of PID alone does not prove code identity, audit identity or entitlement
compatibility.

On this machine, the preserved `dc91b93f…` installed baseline passed the native
x86 control and ARM-client/x86-service case. The two directly launched ARM
service cases failed with NSCocoaErrorDomain 4097 before guest entry. Both
native-entry ARM-service cases returned the correct echo, with the same PID
from wrapper entry through guest listener and receipt. The translated services
remained alive afterward, and the runner recorded exact-path SIGTERM then
SIGKILL cleanup with no remaining owned processes. This is a synthetic echo
result, not validation of Xcode's real services or of service lifecycle support.
Client deadlines are 15 seconds; no automatic retries are performed. Logs and
cleanup actions are preserved in each manifest row.

The inventory is static only. `translated-candidate` means that the file has an
unencrypted ARM-only executable; it does not mean the app or XPC service was
launched or verified. The selected roots are explicit, not a claim that every
application installed on the computer was found.

The current probes do not establish full ARM memory-model correctness,
arbitrary Swift concurrency, sandbox and entitlement compatibility, direct
CAMetalLayer presentation, device I/O, audio, synchronous fault delivery,
debuggers, or full Xcode functionality. Preserve failed logs when improving a
runtime so that a later fix can be evaluated against the same workload.
