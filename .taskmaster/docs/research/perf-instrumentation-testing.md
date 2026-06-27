# PhotoBlaze — Performance Instrumentation, Benchmarking & TDD for a GPU App

> Research doc. Scope: bench stack, profiling/tracing, GPU + end-to-end latency
> measurement, A/B harness design, and how to TDD a wgpu renderer to >80% coverage
> on Windows 11 (macOS port later). Core metric: **keypress → photon** within one
> 120 Hz refresh (~8.3 ms). Sources are cited inline and collected at the end.
> Compiled 2026-06; sources are 2024–2026 unless noted.

---

## 1. Overview

PhotoBlaze needs three measurement layers that must not be conflated:

1. **Microbenchmarks** (offline, deterministic) — "is `decode_jpeg` faster after this
   change?" Run on demand and in CI as a *regression gate*. Wall-clock noise is the
   enemy; instruction-count tooling solves it.
2. **Live profiling** (interactive, on a dev box) — "where did this frame's 12 ms go,
   on the CPU *and* GPU timeline?" Frame-by-frame, real-time, with GPU zones.
3. **End-to-end latency** (the product KPI) — "keypress → photon" p50/p95/p99,
   logged per-frame to a file so we can A/B architectural swaps offline.

The architecture decision that makes all three tractable is **separation of pure
logic from I/O and GPU** (ports-and-adapters / hexagonal). Pure modules
(playlist/nav, precomputed random walk, prefetch-window policy, cache eviction) become
fast unit + property tests with near-100% coverage; the thin GPU/present shell is
covered by golden-image tests and a small amount of `#[coverage(off)]` exclusion. The
same seam is what lets us hot-swap decode backends / cache policies / present modes
behind a trait (or cargo feature) and measure each variant with one harness.

**Headline recommendations** (detail in §7):

| Concern | Pick | Why |
|---|---|---|
| Microbench (local, rich stats) | **Criterion** (or Divan) | de-facto, p50/throughput, plots |
| CI regression gate | **CodSpeed** (Valgrind sim) *or* **iai-callgrind on a Linux job** | instruction-count = deterministic in noisy CI |
| Live frame profiling | **Tracy** via `profiling` crate + **wgpu-profiler** | real-time CPU+GPU zones, frame markers |
| GPU pass timing | **wgpu timestamp queries** (`wgpu-profiler`) | decode-upload vs draw on GPU timeline |
| Keypress→photon | **DXGI waitable swapchain + QPC** in-app; **PresentMon SDK** to validate | true display-time on Windows |
| Golden image | **headless wgpu → buffer → `nv-flip`** perceptual diff | matches how wgpu itself tests |
| Property tests | **proptest** | Strategy composition for nav/cache invariants |
| Fuzzing | **cargo-fuzz** (libFuzzer) | decoder robustness |
| Coverage (Windows) | **cargo-llvm-cov** | tarpaulin is Linux-only |

---

## 2. Benchmarking stack

### 2.1 The three contenders

**Criterion.rs** — statistics-driven, wall-clock. The de-facto standard: warms up,
takes many samples, fits a model, reports estimates with confidence intervals, draws
plots (gnuplot/plotters), and persists results to compare runs (`--save-baseline NAME`,
`--baseline NAME`). Its FAQ is explicit that **cloud-CI virtualization "introduces a
great deal of noise"** and statistics "can only do so much"; the default *noise
threshold* is `0.01` (changes < 1% ignored). Criterion's own FAQ recommends Iai for
CI because Cachegrind counts are immune to VM scheduling jitter. → Use Criterion for
**local, rich microbenchmarks of decode/upload/cache** where you want absolute ns and
throughput (MB/s, images/s).

**Divan** — newer, simpler API (`#[divan::bench]`, registered like `#[test]`),
*more* powerful in a few ways: benchmarks **generic functions / const-generic
variants** in one definition, has an `AllocProfiler` that counts **allocations and
bytes allocated** (e.g. it shows `Vec` allocs once vs `LinkedList` 100×), throughput
counters, and dynamic sample-size scaling to fight timer noise at the ns scale. Still
wall-clock, smaller ecosystem. → Good if we want allocation accounting per decode path
and terse benches; otherwise Criterion's tooling is richer.

**iai-callgrind** (recently renamed **gungraun**) — runs each bench **once** under
**Valgrind/Callgrind** and reports **instruction counts, estimated cycles, cache
metrics (Cachegrind), heap (DHAT)**. Because it counts instructions rather than timing
a VM, results are **deterministic and comparable across machines**, "completely
negating the noise of the environment" — purpose-built for CI regression gating with
configurable thresholds. **Critical constraint: it cannot run on Windows** (Valgrind is
Linux/Unix only) and **not on ARM macOS** either. So on this Windows-first project,
iai-callgrind only runs in a **Linux CI job**, on the platform-agnostic pure logic
(decode math, cache policy) — not on the Windows-specific GPU/present code.

### 2.2 CI regression gating — the actual recommendation

Two viable paths, both instruction-count based so they survive noisy runners:

- **CodSpeed** — provides drop-in compat layers (`codspeed-criterion-compat`,
  `codspeed-divan-compat`) so you keep writing Criterion/Divan benches but CodSpeed's
  **simulation mode runs them under Valgrind/cachegrind for < 1% variance regardless of
  system load**, hardware-agnostic, with auto flame-graphs; a newer **walltime mode**
  additionally samples HW perf counters (cycles, instructions, cache). Wired via
  `CodSpeedHQ/action` in GitHub Actions; it comments regressions on PRs. Lowest-effort
  way to get deterministic CI gating *while keeping the same bench source*.
- **iai-callgrind directly** in a Linux job + **Bencher.dev** to persist/track/graph
  and *fail the PR* on regression. Bencher runs the same benches on the **same bare
  metal locally and in CI**, stores history, and has a **self-hosted** option (no data
  leaves your infra). Bencher has adapters for Criterion, Divan, libtest-bench, and
  iai-callgrind output.

**Decision:** keep **Criterion** (rich local) + add **CodSpeed compat** for the CI gate
(simplest), or iai-callgrind+Bencher if we want self-hosted history. Either way the gate
runs on a Linux runner against **pure, platform-independent code**; Windows GPU timing is
gated separately by the end-to-end latency log (§4), not by microbench instruction counts.

### 2.3 Reproducible benchmark image corpus

There's no turnkey crate for this; build it as a fixture discipline:

- **Commit a fixed corpus** of representative inputs (JPEG/PNG/WebP/HEIC/AVIF at a
  spread of resolutions, chroma subsamplings, bit depths, EXIF orientations, plus a few
  pathological/truncated files). Store binary fixtures via **Git LFS** so the repo stays
  light and the bytes are pinned by content hash.
- **Pin exact bytes, not "an image"** — decoders are sensitive to encoder quirks;
  reproducibility requires byte-identical inputs. Record a **manifest** (`corpus.toml`
  with filename → sha256, dimensions, format, expected decoded checksum).
- Benchmarks should **decode from in-memory `&[u8]`** (read the file once outside the
  measured closure via `iter_batched`/`black_box`) so disk I/O isn't in the hot loop.
- Optionally **generate synthetic images deterministically** (seeded noise/gradients,
  re-encoded by a pinned encoder version) for sizes you can't ship, but checksum the
  output so a libjpeg-turbo/encoder upgrade can't silently change the corpus.
- Tag the corpus version in bench IDs so historical comparisons (Bencher/CodSpeed)
  don't cross corpus boundaries.

---

## 3. Profiling / tracing stack

### 3.1 The `tracing` crate as the spine

Use **`tracing`** for structured spans/events across the app (decode, upload, submit,
present). It's the ecosystem standard and, crucially, has **pluggable subscribers/layers**
so the *same* instrumentation can fan out to a console logger, a JSON file, or a
profiler — no separate instrumentation pass.

### 3.2 Profiler backends and the abstraction layer

- **`profiling`** (aclysma) — a *very thin* macro abstraction over **puffin, optick,
  tracy, superluminal-perf, and tracing**. You write `profiling::scope!("decode")` /
  `#[profiling::function]` once and pick the backend at compile time via features:
  `profile-with-tracy`, `profile-with-puffin`, `profile-with-optick`,
  `profile-with-superluminal`, `profile-with-tracing`. **When the consumer enables none,
  the macros emit no code → zero overhead.** This is the right top-level choice: it keeps
  us from marrying one profiler.
- **Tracy** (`wolfpld/tracy`) — a **real-time, frame-oriented** profiler with
  nanosecond zones, **frame markers**, statistics, and **GPU zones**. This is the live
  frame-by-frame tool. Rust access via **`tracing-tracy`** (feeds `tracing` spans into
  Tracy) and/or **`tracy_full`** (adds a **`wgpu` feature** to profile command
  encoders and render/compute passes, `tracy_frame_marker!()`, `profile_scope!()`).
- **puffin** — pure-Rust, in-process egui flamegraph; nice for an *in-app overlay* with
  no external app, lower fidelity than Tracy.
- **superluminal-perf / optick** — Windows-centric commercial/》native profilers,
  reachable through the same `profiling` features if we want them.

### 3.3 GPU profiling on the same timeline

- **`wgpu-profiler`** (Wumpf) — hierarchical **GPU timestamp** scopes:
  `profiler.scope("upload", &mut encoder)`, `scoped_render_pass(...)`,
  `scoped_compute_pass(...)`, arbitrary nesting, auto query-pool management; then
  `profiler.resolve_queries(&mut encoder)` + `profiler.end_frame()`. It **caches frames
  until results are ready (no device stall)**, supports parallel encoders, and exports
  to **Chrome trace** or integrates with **Tracy/puffin behind feature flags**. Requires
  device features `TIMESTAMP_QUERY`, `TIMESTAMP_QUERY_INSIDE_ENCODERS`, and
  `TIMESTAMP_QUERY_INSIDE_PASSES`.
- **Tracy GPU context** (if going lower-level): declare one `TracyGpuContext` per
  rendering context, mark zones with `TracyGpuZone(name)`, and call `TracyGpuCollect`
  **after present/swap**. Vulkan & D3D12 contexts can be **calibrated** so CPU and GPU
  zones line up on a single timeline (Vulkan uses `VK_EXT_host_query_reset`).

### 3.4 Recommended live-profiling stack (Windows)

Top of app: `tracing` + the **`profiling`** abstraction. Dev profiling builds enable
**`profile-with-tracy`** and run **wgpu-profiler with the Tracy feature** so a single
Tracy capture shows **CPU zones, GPU zones, and frame markers correlated**. Ship an
optional **puffin egui overlay** for a no-tooling, in-app FPS/zone view. All profiling
macros compile out of release builds.

---

## 4. GPU & end-to-end latency measurement

### 4.1 GPU-timeline timing (decode-upload vs draw)

Use **wgpu timestamp queries** (via `wgpu-profiler`, or hand-rolled):

1. Enable `Features::TIMESTAMP_QUERY` (+ `_INSIDE_ENCODERS` / `_INSIDE_PASSES` as needed).
2. Write timestamps: `RenderPassTimestampWrites` / `ComputePassTimestampWrites` on pass
   creation, or `CommandEncoder::write_timestamp` / `RenderPass::write_timestamp` between
   commands.
3. `CommandEncoder::resolve_query_set(...)` into a `QUERY_RESOLVE | COPY_SRC` buffer,
   copy to a `MAP_READ` buffer, map it next frame, and multiply deltas by
   **`Queue::get_timestamp_period()`** to get nanoseconds.
4. **Gotcha:** on Vulkan, timestamps can **read 0 if resolved too soon** (wgpu issue
   #6406) — resolve/read on a later frame, not the same submit. This gives separate GPU
   costs for **texture upload** and **draw**, the two things we A/B when swapping decode
   libs or upload strategies.

### 4.2 Keypress → photon (the product KPI) on Windows

True latency = `T_photon_onscreen − T_key_pressed`. Build it from three timestamps, all
in **`QueryPerformanceCounter` (QPC)** units:

- **`T_key`** — capture QPC at the input event. **Raw Input messages carry a device
  timestamp**; at minimum take QPC in the `WM_KEYDOWN`/winit key handler. (Winit exposes
  the event; record `Instant`/QPC immediately.)
- **`T_present`** — QPC at `queue.submit` + `surface.present()`.
- **`T_photon`** — the hard one; two complementary mechanisms:
  - **DXGI waitable swapchain**: create the swapchain with
    `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT`, get
    `GetFrameLatencyWaitableObject()`, `WaitForSingleObjectEx` **before** rendering each
    frame, and `SetMaximumFrameLatency(1)` for minimum latency (use 2 only if you need
    CPU-GPU parallelism to hold 120 Hz). This both *reduces* latency and gives a precise
    "ready to present" signal. (wgpu uses DXGI under the hood; for fine control we may
    need a thin DXGI shim or the `raw-window-handle`/`dxgi` interop.)
  - **`IDXGISwapChain::GetFrameStatistics`** → `PresentRefreshCount` + `SyncQPCTime`,
    i.e. the **QPC time the frame actually hit the display scanout**. **Restriction:** only
    valid for **flip-model** swapchains or **full-screen**, *not* bitblt windowed — so use
    flip-model (`DXGI_SWAP_EFFECT_FLIP_DISCARD`), which is what we want anyway.

**Validation / ground truth:** Intel **PresentMon** (open source, ETW-based) reports a
**`Click-to-Photon`** (mouse) and **`AllInputToPhoton`** metric, plus CPU/GPU/Display
frame durations, across DX/GL/Vulkan. Since 2.2 it reports in **near real-time (~30 ms
latency, down from 1 s)**. Two ways to use it: (a) run the PresentMon capture app
alongside PhotoBlaze and log CSV for offline analysis; (b) **integrate the PresentMon
SDK** — load `PresentMonAPI2Loader.dll`, include `PresentMonAPI.h`, talk to the
PresentMon **Service** which aggregates ETW frame data + hardware telemetry and exposes
metrics via the API — to read frame/latency metrics from inside our own harness. Treat
PresentMon as the **trusted oracle** to validate our in-app QPC math, then rely on the
in-app numbers for per-frame A/B logging.

> Note: the cleanest "input→photon" instrument is **DXGI GetFrameStatistics SyncQPCTime
> minus the input QPC**, because it pins the actual scanout. PresentMon's click-to-photon
> is the cross-check.

### 4.3 macOS (later port)

- **GPU timing:** `MTLCounterSampleBuffer` (resolve with `resolveCounterRange`, convert
  to ns via `mach_timebase_info` + a gpu/cpu timestamp factor) — or just keep using wgpu
  timestamp queries on the Metal backend.
- **Photon time:** `CAMetalDrawable.presentedTime` / `addPresentedHandler` gives the
  actual on-screen time; `MTLCommandBuffer.addCompletedHandler` gives GPU completion.
  Pair `presentedTime` with the input timestamp for keypress→photon. `CADisplayLink`
  for refresh cadence. So the *measurement seam is identical* to Windows: input QPC/mach
  time, present, photon time — only the photon-time source differs (GetFrameStatistics vs
  presentedTime), which is exactly what the A/B trait abstraction (§5) should hide.

---

## 5. A/B harness design (concrete Rust patterns)

### 5.1 Swappability mechanism — pick per axis

| Mechanism | Dispatch | Overhead | Swap at | Use for |
|---|---|---|---|---|
| **Cargo feature** | compile-time | **zero** | build | mutually-exclusive whole-program variant; profiler backend on/off |
| **Generics `<D: Decoder>`** | static (monomorphized) | none, inlinable | build/type | hot inner path where inlining matters (decode) |
| **Trait object `Box<dyn Decoder>`** | dynamic (vtable) | one indirect call | **runtime** | runtime-selectable backend; the A/B runner that flips configs in one process |
| **Enum dispatch** | match | branch | runtime | small fixed set, want no vtable |

For an **A/B runner that flips configs and emits comparable distributions in one run**,
**trait objects (or enum dispatch) win** because you can iterate over a `Vec<Box<dyn
DecodeBackend>>` at runtime. Keep the *measured hot loop* generic/inlined where it
matters, but select the variant via trait object at the seam. The per-call vtable cost
is negligible next to decode/upload, and it buys a single binary that runs every variant.

```rust
// One trait per swappable axis. Keep them object-safe.
pub trait DecodeBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn decode(&self, bytes: &[u8]) -> anyhow::Result<DecodedImage>;
}
pub trait CachePolicy: Send + Sync {
    fn name(&self) -> &'static str;
    fn on_access(&mut self, idx: usize);
    fn evict(&mut self) -> Option<usize>;        // pure, unit-testable
}
pub trait PresentMode: Send + Sync {
    fn name(&self) -> &'static str;
    fn present(&mut self, frame: Frame) -> PhotonTimestamp;  // hides DXGI vs Metal
}

pub struct Variant {
    pub decode: Box<dyn DecodeBackend>,
    pub cache:  Box<dyn CachePolicy>,
    pub present: Box<dyn PresentMode>,
}
```

`PresentMode` is where the Windows/macOS photon-time difference (GetFrameStatistics vs
`presentedTime`) hides behind one interface — so the harness, tests, and macOS port all
share the same seam.

### 5.2 The runner + per-frame metrics log

Drive a fixed, scripted workload (a recorded sequence of "next image" keypresses over the
pinned corpus) through each `Variant`, and **append one record per frame** to a file for
offline analysis. **NDJSON** (one JSON object per line) or **Parquet/Arrow** are both good;
NDJSON is trivial and grep-able, Parquet is compact for millions of frames.

```rust
#[derive(serde::Serialize)]
struct FrameRecord<'a> {
    variant: &'a str,         // "turbojpeg+lru+flip1"
    frame: u64,
    key_qpc_ns: u64,
    present_qpc_ns: u64,
    photon_qpc_ns: u64,       // GetFrameStatistics SyncQPCTime (or Metal presentedTime)
    gpu_upload_ns: u64,       // wgpu timestamp delta
    gpu_draw_ns: u64,
    decode_ns: u64,           // CPU
    cache_hit: bool,
    keypress_to_photon_ns: u64,
}
```

**Report distributions, not means.** Compute **p50/p95/p99 (and p99.9, max)** per variant
— a mean hides the tail that the 8.3 ms budget cares about. A tiny analysis step
(`polars`/`ndjson` reader, or an external notebook) loads the log and prints a table:

```
variant                p50      p95      p99      max     n
turbojpeg+lru+flip1   5.9ms    7.8ms    9.1ms   14ms   5000
zune+lru+flip1        6.4ms    9.0ms   11.2ms   19ms   5000
turbojpeg+arc+flip2   6.1ms    8.4ms   10.0ms   22ms   5000
```

Keep the workload, corpus version, machine, GPU, and driver in the log header so runs are
comparable. This same log doubles as the **Windows CI latency gate** (run the harness,
assert p99 ≤ budget on a known machine) since microbench instruction counts can't capture
present latency.

### 5.3 Wiring backend swaps without code changes

For coarse GPU-backend A/B (Vulkan vs DX12, or forcing the **WARP software adapter**), wgpu
honors env vars: **`WGPU_BACKEND`** (`vulkan,metal,dx12,gl`), **`WGPU_ADAPTER_NAME`**
(substring match), **`WGPU_DX12_COMPILER`** (`dxc`/`fxc`). The harness can fork itself
across these env settings to A/B the backend without recompiling, and the same vars force a
software adapter for CI.

---

## 6. TDD strategy for a renderer/GPU app

The renderer is "untestable" only if logic is tangled with the GPU. The whole strategy is
to keep that from happening.

### 6.1 Isolate pure logic (the high-coverage core)

Carve the genuinely pure, deterministic decisions into their own modules with **no I/O,
no GPU, no clock** — pass time/randomness in as parameters:

- **Playlist / navigation** — next/prev/wrap, filtered views, multi-selection.
- **Precomputed random walk** — seeded permutation; same seed ⇒ same order (so it's a
  pure function of `(seed, len)` and trivially testable + property-checkable).
- **Prefetch-window policy** — given current index + direction + window size, which
  indices to prefetch/decode-ahead.
- **Cache eviction** — LRU/2Q/ARC as a pure state machine: `on_access`, `evict`, capacity
  invariants. No bytes, just indices/keys.

These get **example-based unit tests + property tests** and should hit **~100% region
coverage**. Inject dependencies (`trait Clock`, `trait Rng`, `trait ImageSource`) so the
sequencing logic that *orchestrates* I/O is testable with fakes too. This is the
ports-and-adapters seam that also enables the A/B traits in §5.

### 6.2 Golden / snapshot image testing (the GPU shell)

Mirror exactly **how wgpu tests itself**:

1. **Headless render** — create a texture with `RENDER_ATTACHMENT | COPY_SRC`, render the
   frame, copy to a `MAP_READ | COPY_DST` buffer (mind the **256-byte
   `COPY_BYTES_PER_ROW_ALIGNMENT`** row padding — compute padded `bytes_per_row` and strip
   padding when reading back), map, and read pixels. No window needed.
2. **Compare to a reference PNG** with **`nv-flip`** (Rust bindings **`nv-flip-rs`**, by
   gfx-rs). FLIP is a **perceptual** difference evaluator: it produces a per-pixel **error
   map (0.0 = identical … 1.0 = max)** approximating what a human notices when flipping
   between the two images, then a **weighted histogram** decides pass/fail against a
   **tolerance**. This is what wgpu's example tests use (`cargo xtask test --bin
   wgpu-examples` renders one frame, captures, and FLIP-compares to a checked-in
   reference). Perceptual diff (not exact `==`) is essential: software adapters and drivers
   produce sub-ULP color differences that exact comparison would flag spuriously.
3. **Harness** — wgpu uses a `#[gpu_test]` macro + `cargo-nextest`. We can adopt a similar
   per-test fixture that boots a device, renders, and FLIP-compares; store references under
   `tests/golden/` and regenerate with an explicit `--bless`/env-gated path.

**Running golden tests in CI without a real GPU** — use software adapters, exactly like
wgpu's CI matrix: **Windows/DX12 → WARP**, **Linux/Vulkan → lavapipe**, **OpenGL →
llvmpipe**, Mac/Metal on a hardware runner. Force them with `WGPU_BACKEND`/
`WGPU_ADAPTER_NAME`. Caveat: software-rasterizer output can differ slightly from the
GPU, so either (a) keep references *per adapter* or (b) set FLIP tolerance generously and
keep golden scenes simple/flat-shaded so rasterization differences stay under threshold.
Tight pixel-exact checks belong in unit tests of pure pixel math (color conversion,
resize kernels), not full-frame goldens.

### 6.3 Decode correctness & regression

- **Decoded-output checksums** against the pinned corpus (§2.3): each fixture has an
  expected post-decode hash (or a small reference PNG via FLIP for lossy formats). A
  decode-lib swap or version bump that changes output trips the test.
- **Cross-decoder differential testing**: decode the same input with two backends and
  assert they agree within tolerance — catches backend-specific bugs.

### 6.4 Property-based testing

Use **proptest** over quickcheck. Both shrink to minimal failing cases, but proptest uses
**explicit `Strategy` objects** (per-value, composable, constraint-aware) rather than
quickcheck's per-type generation — far better for generating *valid* nav/cache sequences
without rejection churn. (Quickcheck's stateless shrinking is simpler/faster but less
expressive; proptest is in passive maintenance but feature-complete and the ecosystem
default.) Properties to assert:

- **Nav**: `next` then `prev` returns to start (away from wrap edges); random-walk visits
  every index exactly once before repeating (it's a permutation); index always in range.
- **Cache**: never exceeds capacity; an item accessed every step is never evicted (LRU);
  hit-rate ≥ a naive baseline on a generated access pattern; no panics on any sequence.
- **Prefetch**: window stays within bounds; never schedules out-of-range indices.

### 6.5 Fuzzing the decoders

**cargo-fuzz** (Cargo subcommand over **libFuzzer**, coverage-guided via LLVM since rustc
*is* LLVM). Write a target that feeds arbitrary bytes to each decode entry point and
asserts **no panic / no UB / bounded memory** (it should return `Err`, not crash, on
garbage or truncated/malicious files). Seed the corpus with the §2.3 fixtures. Run
nightly in CI (or OSS-Fuzz-style) since fuzzing is open-ended. (Recent cargo-fuzz no
longer puts the fuzz crate in a separate workspace by default.) Any crash becomes a
regression test fixture.

### 6.6 Coverage on Windows — realistically hitting >80%

- **Use `cargo-llvm-cov`** (LLVM source-based, **region-level**, Windows/macOS/Linux). **Do
  not use tarpaulin** — it's ptrace-based, **Linux x86_64 only**, useless on our primary
  platform.
- **Exclude what's genuinely untestable** so the 80% denominator is honest:
  - GPU/present glue, swapchain creation, DXGI calls, `main`/event-loop wiring →
    `#[cfg_attr(coverage_nightly, coverage(off))]` (the stable `#[coverage(off)]` was
    briefly stabilized then reverted, so use the cfg form on nightly). cargo-llvm-cov also
    **auto-excludes `tests/` and `*_tests.rs`**.
  - Mark error-path/unreachable arms judiciously rather than contorting tests to hit them.
- **Strategy to actually exceed 80%:** because pure logic (§6.1) is the bulk of meaningful
  code and is ~100% covered, and golden tests exercise the render path end-to-end, the
  *measured* figure lands well above 80% once the thin, excluded GPU shell is removed from
  the denominator. Track coverage in CI (`cargo llvm-cov --lcov` → Codecov/Coveralls) and
  fail under threshold, but on the **pure crates**, not the GPU shell.

---

## 7. Recommendations (the stack to adopt)

1. **Benchmarking:** Criterion for local microbenchmarks (decode, upload, cache ops) with
   a Git-LFS-pinned, checksummed image corpus decoded from memory. Consider Divan where
   allocation accounting helps.
2. **CI regression gate:** Add **CodSpeed** (criterion/divan compat layer, Valgrind sim, <1%
   variance) on a Linux runner against the **pure/platform-independent** code — instruction
   counts, not wall-clock. (Alternative: iai-callgrind + Bencher.dev self-hosted.) Do **not**
   gate on Windows wall-clock microbenches.
3. **Live profiling:** `tracing` + the **`profiling`** macro abstraction; dev builds enable
   **Tracy** (`tracing-tracy`/`tracy_full`) + **wgpu-profiler** (Tracy feature) for unified
   CPU+GPU zones and frame markers; optional **puffin** egui overlay. All compile out of
   release.
4. **GPU timing:** wgpu **timestamp queries** via wgpu-profiler for decode-upload vs draw on
   the GPU timeline (watch the resolve-too-soon zero bug).
5. **Keypress→photon:** in-app **QPC at input → present → DXGI `GetFrameStatistics`
   `SyncQPCTime`** (flip-model + waitable swapchain, max-frame-latency 1); validate against
   **PresentMon Click-to-Photon** (CSV or SDK). Log **per-frame NDJSON/Parquet**, report
   **p50/p95/p99**. macOS later: same seam, `presentedTime` as the photon source.
6. **A/B harness:** trait objects (`Box<dyn DecodeBackend/CachePolicy/PresentMode>`) at the
   seam, generics in the hot inner loop, cargo features only for whole-program/profiler
   toggles. One runner iterates variants over a scripted workload and emits comparable
   latency distributions; `WGPU_BACKEND`/`WGPU_ADAPTER_NAME` for backend/adapter A/B and CI
   software adapters.
7. **TDD:** isolate pure nav/random-walk/prefetch/cache logic for ~100% unit+proptest
   coverage; **golden-image tests** = headless wgpu → buffer → **nv-flip** perceptual diff,
   run on **WARP/lavapipe/llvmpipe** in CI like wgpu does; **cargo-fuzz** the decoders;
   **cargo-llvm-cov** for coverage with `#[coverage(off)]` on the GPU shell to keep >80%
   honest.

---

## 8. Open questions

1. **wgpu ↔ raw DXGI interop for `GetFrameStatistics`/waitable swapchain.** wgpu manages the
   swapchain; do we need a custom surface/DXGI shim (unsafe HAL access) to read
   `SyncQPCTime` and use the frame-latency waitable, or is in-app QPC-at-present + PresentMon
   SDK sufficient ground truth? Needs a spike.
2. **Input timestamp fidelity.** Raw Input carries device timestamps; does winit surface them,
   or do we take QPC in the handler (adding a few hundred µs of OS/queue latency we can't
   see)? How much does that bias the metric vs PresentMon's ETW input timing?
3. **CI determinism for the latency gate.** Present latency needs a *real-ish* present path;
   WARP has no true scanout/`SyncQPCTime`. Do we need a dedicated bare-metal Windows runner
   (Bencher-style) for the keypress→photon gate, or only gate GPU-pass timestamp deltas in CI
   and measure true photon latency on a known dev machine?
4. **Golden-image stability across adapters.** Will WARP vs real-GPU (and lavapipe/llvmpipe)
   rasterization differences stay under a single FLIP tolerance, or must we keep per-adapter
   reference sets? Affects test maintenance cost.
5. **iai-callgrind's no-Windows constraint.** Is gating purely on Linux-run instruction counts
   of platform-independent code enough confidence, given the real perf risk lives in
   Windows-specific decode-upload-present? (Leaning: yes for logic regressions, no for the KPI
   — hence the separate latency log gate.)
6. **`#[coverage(off)]` instability.** Stabilization was reverted; we're tied to nightly +
   `cfg_attr(coverage_nightly, …)`. Acceptable, or do we structure modules so the GPU shell is
   a separate crate simply excluded from the coverage run?
7. **Corpus licensing/size.** Shipping real-world JPEG/HEIC/AVIF samples (and their decoded
   references) via Git LFS — license-clean sources, and LFS budget for a corpus big enough to
   be representative.

---

## 9. Sources

**Benchmarking**
- Criterion.rs (repo + FAQ on CI noise, save-baseline, noise threshold): https://github.com/bheisler/criterion.rs , https://bheisler.github.io/criterion.rs/book/faq.html , https://github.com/bheisler/criterion.rs/blob/master/book/src/faq.md
- "Problems with Unstable Benchmarks" (criterion CI noise): https://github.com/bheisler/criterion.rs/issues/485
- Divan announcement (Nikolai Vazquez): https://nikolaivazquez.com/blog/divan/ ; HN: https://news.ycombinator.com/item?id=37773599
- iai-callgrind / gungraun (repo, docs, crate): https://github.com/iai-callgrind/iai-callgrind , https://docs.rs/iai-callgrind , https://crates.io/crates/iai-callgrind , https://lib.rs/crates/iai-callgrind
- Bencher: Iai guide + prior art: https://bencher.dev/learn/benchmarking/rust/iai/ , https://bencher.dev/docs/reference/prior-art/ , https://bencher.dev/rust/iai-callgrind/ , https://github.com/bencherdev/bencher
- Lambdaclass, criterion+iai: https://blog.lambdaclass.com/benchmarking-and-analyzing-rust-performance-with-criterion-and-iai/
- CodSpeed: divan guide + repo + action: https://codspeed.io/docs/guides/how-to-benchmark-rust-with-divan , https://github.com/CodSpeedHQ/codspeed-rust , https://github.com/marketplace/actions/codspeed-performance-analysis , https://docs.rs/codspeed-divan-compat
- The Rust Performance Book — benchmarking (recommends Iai for CI): https://nnethercote.github.io/perf-book/benchmarking.html

**Profiling / tracing**
- `profiling` abstraction crate (puffin/optick/tracy/superluminal/tracing features): https://github.com/aclysma/profiling , https://crates.io/crates/profiling , https://lib.rs/crates/profiling
- Tracy profiler (frame markers, GPU zones, GpuContext, GpuCollect, Vulkan/D3D12 calibration): https://github.com/wolfpld/tracy
- tracing-tracy: https://docs.rs/tracing-tracy
- tracy_full (wgpu encoder/pass profiling, frame markers): https://lib.rs/crates/tracy_full , https://crates.io/crates/tracy_full
- wgpu-profiler (GPU timestamp scopes, Chrome trace, Tracy/puffin): https://github.com/Wumpf/wgpu-profiler , https://lib.rs/crates/wgpu-profiler

**GPU timing & end-to-end latency**
- wgpu timestamp queries example + Features docs: https://wgpu.rs/doc/wgpu_examples/timestamp_queries/index.html , https://docs.rs/wgpu/latest/wgpu/struct.Features.html , https://wgpu.rs/doc/wgpu/enum.QueryType.html
- "How to use WebGPU Timestamp Query" (Omar Shehata): https://omar-shehata.medium.com/how-to-use-webgpu-timestamp-query-9bf81fb5344a
- LearnWebGPU benchmarking/time: https://eliemichel.github.io/LearnWebGPU/advanced-techniques/benchmarking/time.html
- wgpu issue: Vulkan timestamps return 0 if resolved too soon: https://github.com/gfx-rs/wgpu/issues/6406
- PresentMon (repo, releases, service/SDK readme, click-to-photon): https://github.com/GameTechDev/PresentMon , https://github.com/GameTechDev/PresentMon/blob/main/README-Service.md , https://github.com/GameTechDev/PresentMon/blob/main/README-CaptureApplication.md , https://presentmon.com/
- PresentMon 2.2 real-time / click-to-photon: https://www.wepc.com/news/intel-presentmon-version-22-significantly-reduces-event-latency-metrics-now-reported-in-real-time/ , https://videocardz.com/newz/intel-presentmon-2-2-0-offers-significantly-lowered-event-latency
- DXGI waitable swapchain / frame latency (MS Learn): https://learn.microsoft.com/en-us/windows/uwp/gaming/reduce-latency-with-dxgi-1-3-swap-chains , https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_3/nf-dxgi1_3-idxgiswapchain2-getframelatencywaitableobject
- IDXGISwapChain::GetFrameStatistics (SyncQPCTime, flip/fullscreen restriction): https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nf-dxgi-idxgiswapchain-getframestatistics
- Raph Levien, swapchains & frame pacing: https://raphlinus.github.io/ui/graphics/gpu/2021/10/22/swapchain-frame-pacing.html
- macOS Metal: GPU counters / MTLCounterSampleBuffer: https://developer.apple.com/documentation/metal/gpu-counters-and-counter-sample-buffers , https://developer.apple.com/documentation/metal/mtlcountersamplebuffer ; resolving Metal GPU timers: https://feresignum.com/resolving-metal-gpu-timers/ ; drawables/presentedTime: https://developer.apple.com/library/archive/documentation/3DDrawing/Conceptual/MTLBestPracticesGuide/Drawables.html

**A/B / swappability**
- Trait objects vs generics (Effective Rust): https://www.lurklurk.org/effective-rust/generics.html ; The Book ch. trait objects: https://doc.rust-lang.org/book/ch18-02-trait-objects.html ; polymorphism (generics/trait objects/enums): https://medium.com/@kaly.salas.7/3-ways-to-use-polymorphism-in-rust-when-to-use-generics-trait-objects-and-enums-94a451765e7d
- wgpu env vars (WGPU_BACKEND / WGPU_ADAPTER_NAME / WGPU_DX12_COMPILER): https://github.com/gfx-rs/wgpu , https://github.com/bevyengine/bevy/discussions/8667

**TDD / golden image / property / fuzz / coverage**
- wgpu testing doc (#[gpu_test], xtask, nv-flip, CI software adapters): https://github.com/gfx-rs/wgpu/blob/trunk/docs/testing.md
- wgpu CI software adapters (WARP/lavapipe/llvmpipe), nextest: https://github.com/gfx-rs/wgpu , https://gfx-rs.github.io/2021/09/16/deno-webgpu.html
- nv-flip-rs bindings + FLIP: https://github.com/gfx-rs/nv-flip-rs , https://developer.nvidia.com/blog/flip-a-difference-evaluator-for-alternating-images/ , https://github.com/NVlabs/flip
- wgpu_test::image: https://wgpu.rs/doc/wgpu_test/image/index.html
- Headless wgpu render-to-buffer (256-byte row align): https://sotrh.github.io/learn-wgpu/showcase/windowless/ , https://deepwiki.com/gfx-rs/wgpu-native/5.4-capture-example
- proptest vs quickcheck: https://proptest-rs.github.io/proptest/proptest/vs-quickcheck.html , https://github.com/proptest-rs/proptest , https://github.com/BurntSushi/quickcheck , https://www.lpalmieri.com/posts/an-introduction-to-property-based-testing-in-rust/
- cargo-fuzz (book, repo, changelog, testing handbook): https://rust-fuzz.github.io/book/cargo-fuzz.html , https://github.com/rust-fuzz/cargo-fuzz , https://github.com/rust-fuzz/cargo-fuzz/blob/main/CHANGELOG.md , https://appsec.guide/docs/fuzzing/rust/cargo-fuzz/
- cargo-llvm-cov vs tarpaulin (platform support, regions, exclusion): https://github.com/taiki-e/cargo-llvm-cov , https://rustprojectprimer.com/measure/coverage.html , https://github.com/taiki-e/cargo-llvm-cov/issues/123 ; rustc instrument-coverage: https://doc.rust-lang.org/rustc/instrument-coverage.html
