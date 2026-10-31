# Metal pipeline reflection and real GPU regression

Pipeline creation can return both a pipeline reflection object and an NSError through separate
Objective-C object pointers. The previous bridge treated every non-const `^@` parameter as the
same NSError output. A successful ARM render-pipeline request with reflection consequently
aborted with `Invalid domain=nil in -[NSError initWithDomain:code:userInfo:]` on the test host.

The legacy RPC now carries one object output slot per argument. The host invokes the method
with independent autoreleasing slots, encodes each output separately, and the guest decodes
reflection proxies and NSError values into their original caller slots. NULL output pointers
remain NULL. Successful NSError-only requests retain memo caching; a request that returns an
actual output object is not memoised as a pipeline-only result. Reflection APIs continue to use
the synchronous forwarding path; command encoding and batching are unchanged.

The real GPU probe in `tests/gpu/metal_render_reflection.m` performs:

- Two identical render-pipeline reflection requests and inspection of fragment buffer 0.
- Pipeline creation with NULL reflection and error outputs.
- Vertex and fragment shaders rendering a complete 16 × 16 target into a private texture.
- A texture-to-buffer GPU blit and verification of all 256 RGBA pixels.
- Compute-pipeline reflection and a GPU kernel with all 256 results verified.
- Invalid pipeline creation with a separate NSError result and nil reflection.

The same source is compiled for native x86_64 and ARM64. The runner also tests the normal bridge,
legacy forwarding with plist descriptors, and disabled batching. Process groups have a deadline
of at most 60 seconds. No GUI app is opened or closed. Run with a separately built GPU directory:

```sh
python3 tests/check_gpu_reflection.py \
  --runtime /absolute/path/to/maclator \
  --sysroot /absolute/path/to/arm64-sysroot \
  --gpu-dir /absolute/path/to/gpu \
  --output /absolute/path/to/results
```

An optional `--baseline-gpu-dir` reproduces the old bridge crash before checking the fixed one.
It requires the precise `Invalid domain=nil` failure, so an unrelated timeout or crash cannot
count as a reproduced regression. Logs, return codes, environment switches and SHA256 hashes
are saved alongside the result JSON.

On 2026-10-13, native x86_64 and all three fixed-bridge ARM modes passed on the NVIDIA GeForce
RTX 5070 Ti Laptop GPU exposed by NVMTL/NVK GB205-A. The old installed bridge aborted at the
first reflection request; a separate ARM 4096-byte Metal fill/copy/readback regression also
passed with the fixed bridge. The historical run was recorded as `Maclator-gpu-reflection`; the reproducible driver is `tests/check_gpu_reflection.py`.
This verifies these shader, render, compute and blit paths; it is not an application performance
benchmark or a proof of support for every Metal API.

The next display gap is direct `CAMetalLayer` / `MTKView` presentation. Only device factories
are interposed, so an ARM app still constructs a guest-side layer rather than a host layer with
host drawable objects. Treating the layer as a descriptor does not attach it to the guest's
Core Animation tree or provide drawable lifetime and presentation semantics. This needs an
explicit layer/drawable bridge and a displayed-window test; offscreen texture rendering does
not establish that support. Existing IOSurface texture transport is useful infrastructure but
does not supply the missing layer integration.

Apple documents the [pipeline reflection object](https://developer.apple.com/documentation/metal/mtlrenderpipelinereflection?language=objc)
and [pipeline creation APIs](https://developer.apple.com/documentation/metal/pipeline-state-creation?language=objc).
