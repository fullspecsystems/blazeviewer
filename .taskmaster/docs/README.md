# PhotoBlaze — Design & Research Docs

> Prime directive for every decision in here: **"Will this make it faster, or
> have basically zero performance impact?"** If the answer is neither, it doesn't
> ship.

This folder holds the thinking behind PhotoBlaze: a chrome-less photo viewer
whose only obsession is how fast you can flick through thousands of images.

## Synthesis docs (read these first)

| Doc | What it is |
|-----|------------|
| [`architecture.md`](./architecture.md) | The system: threads, pipeline, caches, the keypress→photon path. |
| [`decisions.md`](./decisions.md) | Architecture Decision Records + the open questions awaiting the owner. |
| [`roadmap.md`](./roadmap.md) | Phased build plan; seed for `.taskmaster/tasks/tasks.json`. |

## Research (deep dives, with sources)

| Doc | Scope |
|-----|-------|
| [`research/gpu-decode-pipeline.md`](./research/gpu-decode-pipeline.md) | GPU decode (nvImageCodec/nvJPEG/NVDEC), CUDA↔graphics interop, the wgpu zero-copy limitation, VRAM budgeting, Metal/UMA portability. |
| [`research/cpu-decode-libraries.md`](./research/cpu-decode-libraries.md) | Per-format Rust decode crates: speed, SIMD, scaled-decode, Windows build friction. |
| [`research/windowing-gpu-presentation.md`](./research/windowing-gpu-presentation.md) | winit, refresh-rate detection, wgpu vs ash vs D3D12, low-latency present modes, HDR output. |
| [`research/svg-raw-color.md`](./research/svg-raw-color.md) | SVG (resvg/vello), RAW (embedded-preview fast path), color management (moxcms). |
| [`research/perf-instrumentation-testing.md`](./research/perf-instrumentation-testing.md) | Benchmarking, Tracy/wgpu-profiler, keypress→photon measurement, A/B harness, golden-image TDD, coverage. |
| [`research/prior-art-viewers.md`](./research/prior-art-viewers.md) | How the fastest existing viewers (Photo Mechanic, JPEGView, mpv, …) achieve their speed. |

## Code map

```
crates/
  pb-core    pure nav / precomputed-random / prefetch / cache-residency  (no I/O, no GPU, fully tested)
  pb-decode  decode abstraction: decode-to-fit + preview-first, swappable backends
  pb-render  fit-to-screen geometry today; wgpu presenter later
  pb-app     the binary: winit event loop wiring it all together
```
