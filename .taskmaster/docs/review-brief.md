# PhotoBlaze — Architecture Review Brief

> **RESOLVED 2026-06-26 — codex review complete.** Outcome: **A for the engine, C
> for the architecture** — wgpu + CPU decode + staging-ring upload is the v1
> foundation; native D3D12 + zero-copy is a *gated acceleration backend* (kill
> criterion on the high-MP stress test). Plans updated: `decisions.md` (post-spike
> update + ADR-002/012/016/017), `architecture.md`, `roadmap.md`. This brief is kept
> as the point-in-time input that was reviewed.

**For:** an external/adversarial review (codex), before we write any rendering code.
**Scope:** one decision — the GPU stack. Everything else (pure core, trait seams,
format picks, TDD/instrumentation) is low-risk and reversible; don't spend the
review there.

## The decision under review

Build the Windows renderer as **native D3D12** + **nvImageCodec GPU decode** +
**CUDA→D3D12 zero-copy upload**, instead of **portable wgpu + a CPU decode pool +
staged upload**.

This is the one bake-in: it shapes the entire `pb-render` crate, Windows-locks the
renderer (macOS becomes a separate Metal backend, not a recompile), and is
expensive to reverse mid-build. It was chosen (owner decision Q-1) to maximize
speed over portability.

## The three load-bearing claims behind it

The decision is only correct if all three hold. Status:

1. **"wgpu can't import an external/CUDA texture."** *(VERIFY — may be stale.)*
   This is the sole reason to abandon wgpu. It came from a research agent; wgpu
   moves fast. If a stable external-memory/shared-texture path exists or is
   imminent, the native-D3D12 rationale collapses and we keep portability.
2. **"The RTX 5090 hardware-decodes JPEG fast."** *(UNCONFIRMED.)* The dedicated
   NVJPG engine is documented only on datacenter GPUs; consumer Blackwell may fall
   back to GPU-hybrid. If so, the GPU-decode win shrinks.
3. **"A CPU decode pool can't sustain 120 Hz."** *(MEASURED — and it's FALSE for
   this user's library.)* See data below.

## New evidence: the CPU-decode spike (measured, not assumed)

Pure-Rust decode (zune-jpeg) over **200 of 15,874 real photos** from the owner's
library, on the target machine (**32 cores**), fit target 7680×3840 @ 120 Hz:

| Metric | Result |
|---|---|
| All-core full-decode throughput | **295 img/s = 2.5× the 120 Hz budget** |
| Single-thread latency | 32 ms p50 / 64 ms p95 |
| Decode-to-fit triggered | **0 / 200 images (0%)** |
| Corpus | 0.1–16 MP (no >24 MP present) |

Implications:
- The CPU path **already flies past 120 Hz** with the *slowest* reasonable
  decoder; turbojpeg would be faster. Holding a key consumes 120/s; the pool
  delivers 295/s. The prefetch ring stays warm **without GPU decode**.
- **Decode-to-fit is inert on this display.** On a 7680-wide screen, power-of-2
  DCT scaling never fires for ≤16 MP photos — so the architecture's "biggest
  lever" delivers ~nothing here. (turbojpeg's 1/8-granular scaling would help a
  little; GPU decode doesn't change this.)

Caveats: corpus tops out at 16 MP (high-MP 45–60 MP cameras not directly
measured; extrapolation ≈150 img/s on 32 cores — tighter but likely fine);
upload+present not yet measured (the real open question may be upload, not decode).

Full report: `.taskmaster/reports/decode-spike.md`.

## New evidence: the upload spike (measured)

Headless wgpu texture upload on the RTX 5090, fit-sized frames into a resident
ring, vs the same 120 Hz budget:

- **Naive `queue.write_texture` is the trap** — ~60–75 fps for ≥25 MP frames
  (below budget); it allocates fresh staging per call.
- **Persistent staging-buffer `copy_buffer_to_texture` ≈ 48 GB/s (near the PCIe
  Gen5 ceiling) = 414 fps even for a 118 MB full-screen frame = 3.4× budget** — and
  pure wgpu. The staging ring is the fix, and it's already in the architecture.
- For the ≤16 MP library, fit frames are ≤64 MB and even the naive path clears
  budget (313 fps / 2.6× at 12.6 MP).
- Honesty note: the DX12 `copy_buffer` figures came back above the PCIe ceiling
  (Resizable BAR put staging in VRAM → a VRAM↔VRAM copy, not a real upload) and are
  discarded; the credible number is the Vulkan run.

So **upload is not the wall either.** Full report:
`.taskmaster/reports/upload-spike.md`.

## Questions for the reviewer

1. **Is claim #1 still true today?** Does any shipped/imminent wgpu (or
   wgpu-hal) path import external/CUDA/D3D12-shared textures? This single fact
   gates the whole decision.
2. Given the spike, **is GPU decode justified at all** for a ≤24 MP library at
   120 Hz, or is it complexity chasing a bottleneck that isn't there?
3. **Upload is now measured:** the staging-ring path hits 414 fps (3.4× budget)
   for a 118 MB frame; only naive `write_texture` falls short. Does that close the
   upload-based case for native D3D12, or is there a pipelining/overlap concern the
   throughput test misses (e.g. upload contending with draw on wgpu's single queue)?
4. Is there a **hybrid** that keeps portability: wgpu + CPU decode pool + a
   tuned staging-ring upload, dropping to native D3D12 *only* if upload is proven
   to be the bottleneck?
5. What's the right **high-MP** stress test before committing?

## The three ways forward

- **A — Reconsider toward portable wgpu + CPU decode.** The spike says decode
  isn't the bottleneck; keep the cheap Mac port and far less complexity. Measure
  upload before deciding it's a problem.
- **B — Proceed with native D3D12 + zero-copy as committed.** Justified only if
  the reviewer confirms upload is the wall and/or high-MP decode falls short.
- **C — Hybrid / staged.** Build on wgpu + CPU now; keep the `Renderer`/decode
  traits so native-D3D12 zero-copy can slot in *iff* measurement demands it.

Recommendation going in: **A.** Both spikes say decode (2.5×) and upload (3.4× via
the staging ring) clear 120 Hz on portable wgpu + CPU for this ≤16 MP library, so
the native-D3D12 / nvImageCodec / zero-copy complexity buys no user-visible speed
here. Keep the `Renderer`/decode trait seams so that path can slot in *iff* a
high-MP (45–60 MP) or higher-refresh future ever proves it. The two things still
worth the reviewer's eyes: claim #1 (is wgpu external-texture import really
impossible/stale?) and the high-MP question.
