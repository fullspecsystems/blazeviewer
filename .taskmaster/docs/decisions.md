# PhotoBlaze — Decisions Log (ADRs) & Open Questions

Status legend: **Accepted** (decided), **Proposed** (default pending owner
confirmation), **Open** (needs owner input — see bottom).

---

## Accepted

### ADR-001 — Language: Rust
Bare-metal performance, fearless concurrency for the decode pool + shared caches,
and the heavy codecs are C either way (we bind to them). Rust makes the
*orchestration* memory-safe, which is where a C++ version would bleed.

### ADR-002 — GPU API: wgpu (D3D12 on Windows, Metal on macOS), behind a `Renderer` trait
*Revised again 2026-06-26 after the decode/upload spikes + codex review (see the
post-spike update below).* **wgpu is the v1 renderer** — DX12 backend on Windows,
Metal on macOS — behind a thin `Renderer` trait. The spikes showed CPU decode
(2.5×) and a persistent staging-buffer upload ring (3.4×) already clear 120 Hz for
the real corpus, so wgpu's portability costs nothing measurable here. **Native
D3D12 is retained as a measured *acceleration backend* behind the same trait** (see
ADR-012), not the default. Raw Vulkan stays rejected on Windows (no DXGI waitable);
within wgpu we prefer the DX12 backend for its frame-latency waitable object.

### ADR-002a — macOS is a cheap wgpu/Metal port, not a separate effort
With wgpu as the v1 renderer, the macOS (Apple Silicon) port is largely a recompile
to the Metal backend plus a hardware-decode/upload backend swap; the trait seams
isolate the platform bits. (Reverts the earlier native-D3D12-only stance — still
deferred to v2, but no longer a rewrite.)

### ADR-003 — Presentation: flip-model + waitable object, present mode `Mailbox`
Lowest latency without tearing. `SetMaximumFrameLatency(1)`, wait at the **top**
of the loop, then sample input → render newest → present. `Immediate` is a toggle
for absolute-minimum latency (accepting a faint tear). Never plain `Fifo`.

### ADR-004 — Decode-to-fit + preview-first are part of the decode contract
`pb-decode::ImageDecoder` takes a `FitBox` and may return an embedded preview
first. These are the two biggest decode-speed levers, so they're not optional
add-ons — they're the interface.

### ADR-005 — "Random" = a precomputed shuffle order
Rolling a die per keypress is unprefetchable. A precomputed permutation makes the
next random targets *known*, hence preloadable; walking it visits each photo once
before reshuffling. (`pb-core::ShuffleOrder`.)

### ADR-006 — The keypress path is a rebind, never a decode or upload
Steady state = cache lookup + rebind resident texture + draw. Decode and upload
happen during prefetch. A keypress that triggers either means the prefetch window
is wrong.

### ADR-007 — Workspace with trait-based A/B seams
`pb-core` / `pb-decode` / `pb-render` / `pb-app`. Every non-obvious hot-path
choice (decode backend, cache policy, prefetch policy, present mode, upload
strategy) is a trait with a benchmarked default.

### ADR-008 — Decode pool: priority queue + cancellation (not bare rayon)
Rayon's work-stealing reorders prefetch and has no priorities; a direction
reversal must be able to cancel now-stale jobs.

### ADR-009 — Color management in-shader (moxcms)
Extract 3×3 matrix + TRC for common profiles and apply on the GPU — effectively
free. `lcms2` behind a flag for exotic CLUT/CMYK. Identical on Windows and macOS.

### ADR-010 — TDD, >80% coverage (cargo-llvm-cov), golden-image rendering tests
Pure logic isolated in `pb-core`; headless **wgpu** render (WARP/lavapipe in CI)
→ readback → nv-flip diff vs reference PNGs for the GPU path; proptest for
invariants; cargo-fuzz for decoders. GPU shell marked `#[coverage(off)]` to keep
the number honest.

### ADR-011 — Instrumentation is first-class
`tracing` + Tracy with **wgpu timestamp queries** (+ `wgpu-profiler`), compiled out
of release; keypress→photon via DXGI `GetFrameStatistics`, validated against
PresentMon; Criterion locally + deterministic instruction-count gate (CodSpeed/iai)
in CI on platform-independent code. Report p50/p95/p99, never means.

### ADR-012 — GPU decode + zero-copy is a *gated acceleration backend*, not v1
*Revised 2026-06-26 (spikes + codex).* v1 ships the **CPU decode pool + persistent
staging-ring upload on wgpu** (ADR-017). `nvImageCodec` GPU decode + CUDA→D3D12
zero-copy is kept behind the `DecodeBackend` / `UploadStrategy` seams as an
acceleration path, pursued **only if** a measured bottleneck appears (chiefly
high-MP). **Kill criterion:** keep the zero-copy path only if it beats the tuned
CPU + staging path by a meaningful margin on the high-MP stress test (45/60/100 MP);
otherwise drop it. NVIDIA's current nvJPEG docs list hardware-JPEG acceleration on
Blackwell/Ada/Hopper/Ampere — the earlier "datacenter-only" caveat is likely stale
— but the RTX 5090's actual path must still be queried and benchmarked before any
design depends on it. (FFI cost stands: no Rust bindings; hand-rolled bindgen +
`cudarc`.)

### ADR-013 — Provisional library picks
Per the table in `CLAUDE.md` / `architecture.md`. All provisional and
benchmark-justified; the seams exist so any can be replaced with data.

### ADR-014 — SVG via resvg; RAW via embedded-preview extraction
resvg/usvg rasterizes to a tiny-skia pixmap at on-screen resolution (uploads
directly). RAW browses via its embedded full-size JPEG preview (cheap); full
demosaic is deferred to a later zoom feature.

### ADR-015 — HEIC and AVIF are first-class in v1 (CPU-decoded)
*Per Q-2.* Included from v1, but **CPU-decoded** (`libheif-rs` / `dav1d` +
`avif-parse`) into the resident ring — there is no turnkey GPU decoder. NVDEC
tile-decoding is a much larger, later optimization (gated by measurement). We
accept the `libheif` Windows build cost: pin vcpkg ports or ship prebuilt DLLs,
and isolate it behind a cargo feature so a broken build never blocks the core.

### ADR-016 — Color: wide-gamut SDR (Display-P3) for v1; HDR when wgpu surfaces it
*Per Q-3; updated 2026-06-26.* In-shader color management (`moxcms`, matrix + TRC)
targeting Display-P3 now. True-HDR *surface* output follows wgpu's HDR surface
support when it ships (the in-shader pipeline is already ready for it), or via the
native-D3D12 acceleration backend if that path is ever pursued. (No longer "free via
D3D12" now that wgpu is the default renderer.)

### ADR-017 — Upload via a persistent staging-buffer ring, never `write_texture`
*Measured 2026-06-26 (upload spike).* `queue.write_texture` collapses to ~60–75 fps
on large frames (fresh staging allocated per call); a persistent mapped staging
buffer + `copy_buffer_to_texture` hits ~48 GB/s ≈ 414 fps for a 118 MB frame (3.4×
budget) — pure wgpu. v1 uploads through a staging-buffer ring behind an
`UploadStrategy` seam. Still to measure: a faithful end-to-end run (per-frame CPU
write into mapped staging → copy → draw → present while holding nav at 120 Hz).

### ADR-018 — Windows desktop integration & distribution: a signed WiX/MSI, not MSIX
*Decided 2026-06-27.* PhotoBlaze ships as a **code-signed WiX → MSI**, built and
signed (**Azure Trusted Signing**, which the owner has) in **GitHub Actions** and
attached to Releases. **MSIX/Store is deferred until there is a reason to charge
money** — its only real draw here is Store *discovery*, which we don't need yet,
and it actively costs us: a folder right-click verb is a one-line registry key in
an MSI but requires a packaged `IExplorerCommand` COM handler under MSIX, plus
container/iteration friction. Signing — not packaging — is what removes the
SmartScreen prompt, so a classic installer is signed and warning-free all the same.
- **Associations:** the MSI registers PhotoBlaze as a *candidate* handler (a
  `PhotoBlaze.Image` ProgID + per-extension `OpenWithProgids` for the common raster
  set: jpg/jpeg/png/gif/webp/bmp/tiff/heic/avif/jxl — **not** RAW/SVG by default)
  plus an "Open with PhotoBlaze" `Directory\shell` verb, a Start-menu shortcut, the
  app icon, and clean uninstall. **No app or installer can silently seize the
  Windows default** (the SID-salted `UserChoice` hash is OS-protected); an in-app
  "Set as default" deep-links `ms-settings:defaultapps`.
- **Open behavior (owner):** a **photo** opens its folder *flat*; a **folder**
  opens *recursively*; **Ctrl+R** toggles recursion at runtime; **O** = file
  picker. Moving the recursion toggle to Ctrl+R frees **R/Shift+R** for rotate,
  resolving the Task-1 ↔ Task-9 key conflict.
- **Privacy boundary (owner, re-scoping Task 2):** install/registry/associations
  are explicitly fine — the guarantee is "no persistent record of *viewed photos or
  their metadata*," not "the app leaves no footprint at all."

### ADR-019 — Launch handed to a pure open-request seam (`pb-core::open`)
*Decided 2026-06-27.* Every entry point — CLI path, double-click via association,
drag-and-drop, file picker, and later the macOS `openFiles` Apple Event — is
normalized by a thin app-layer I/O shim into a `LaunchInput { Empty | Files |
Directory }` (the one step that reads the disk: an `fs::metadata` file-or-folder
check). A **pure** `pb-core::open::plan` then yields an `OpenPlan` (`Source::Scan {
roots, recursive }` or `Source::Explicit(files)`, plus a `Cursor`). The filesystem
scan + extension filter stay in the app (`pb-core` keeps its no-I/O rule); ordering
and `resolve_cursor` are pure and unit-tested. This is the seam that makes the
**macOS port a delivery-layer change only** (a different shim builds the same
`LaunchInput`); it also keeps Task-9's recursive ordering reusable by folder opens.

### ADR-020 — HEIC/AVIF decode: preview-first now; CPU `libheif` next; NVDEC deferred
*Decided 2026-06-27.* HEIC is the one format that can't keep up with the prefetch
engine. **Measured root cause:** the Windows WIC HEVC decoder **serializes** —
~1.7× across 8 threads on a 32-core box (vs JPEG 4.3×); STA-vs-MTA made no
difference, so it's the decoder/DXVA session, not COM. Shipped now: **preview-first**
(WIC `GetThumbnail`, 320×240, ~18 ms) for instant scrolling + **on-land sharpen**
(full decode of only the on-screen photo, upgrade the slot in place). The full
decode is still WIC-bound (~250 ms–1 s).
- **Next: route HEIC/AVIF to CPU `libheif`** (behind the `ImageDecoder` seam, A/B
  vs `WicDecoder`, cargo-feature-gated per ADR-015). The decode pool already runs 8
  concurrent workers; libheif decodes have no shared GPU session, so they run truly
  in parallel → ~8× throughput → prefetch full-res *ahead* of the user. Cost: the
  libheif Windows C-dep (vcpkg + ship DLLs).
- **NVDEC deferred** (ADR-012 stays the GPU escalation): an iPhone HEIC is a
  **48-tile 512×512 HEVC grid**, so NVDEC means hand-writing grid demux + 48-decode
  orchestration + stitch + CUDA↔D3D12 interop — the hard 80% that libheif does for
  free. Pursue only if libheif can't keep up at 48 MP.
- Full plan, phasing, toolchain blocker, the higher-res-preview spike, and the
  code-review follow-ups: [`heic-decode-plan.md`](heic-decode-plan.md).

### ADR-021 — macOS chrome: native AppKit/SwiftUI shell over an extracted `AppCore`; egui retained on Windows
*Decided 2026-06-30. Execution plan: [`macos-native-ui-plan.md`](macos-native-ui-plan.md).*
*Status (2026-07-01): **NS0 (`AppCore` extraction) COMPLETE**, on `main` — the winit shell drives
the engine entirely through `AppCore::handle(CoreEvent)` + a `CoreEffect` drain. **NS1 FOUNDATION
laid** — the FFI boundary is **`swift-bridge`** (chosen over UniFFI and a hand-rolled C-ABI for the
ergonomic marshaling of the enum-heavy effect drain); a macOS-only `staticlib` crate
`crates/pb-mac-ffi` bridges `AppCore` with the KeyDown→effect round-trip proven. Remaining NS1 work
(CAMetalLayer surface, real construction, input adapter, event/effect expansion, frame pump, menu,
CI, exit criteria) is tracked in `current-status.md` ▶ Resume.*
On the **macOS target only**, the egui `DialogWindow` chrome is replaced by a native
AppKit/SwiftUI shell that owns the `NSWindow` + run loop and hosts the wgpu/Metal
canvas in an `MTKView`. This requires extracting a platform-neutral **`AppCore`**
(commands-in / effects-out) from the winit `ApplicationHandler` in
`pb-app/src/main.rs`; winit stays the Windows/Linux driver. The renderer remains
wgpu/Metal, but a native `MTKView.layer` is not the current safe winit-window surface
path: NS1 must add/prove a macOS-only `CAMetalLayer` surface adapter with explicit
main-thread and lifetime rules. Rationale: the Mac market expects and pays for
native polish ("Mac-assed"); egui's immediate mode has a hard ceiling (no native
controls, text editing, or accessibility); the engine seams (ADR-002/007/019) already
make the chrome swappable. Windows keeps egui (lower native bar + willingness-to-pay;
native WinUI would be the worst effort/return quadrant). Sequenced strangler-fig
(NS0–NS3) with a shippable build at every step; the egui-on-Mac beta stays the
default Mac artifact until the NS3 cutover. Risk posture: feasible and worthwhile,
but not low-risk polish — NS0 (`AppCore` extraction) and NS1 (`CAMetalLayer`/FFI/frame
pump) are proof gates, and cutover happens only after both are stable. Supersedes the
implicit "egui everywhere" of `macos-port-plan` for the Mac chrome only. **Parameters
(owner, 2026-06-30):**
- **arm64-only** (no universal2). Intel Mac is sunsetting (last Intel Mac discontinued
  2023; Tahoe/macOS 26 the final Intel-supporting release; Rosetta 2 winding down);
  PhotoBlaze's value prop is Apple-Silicon-shaped (UMA upload short-circuit, EDR/P3,
  Image I/O HEVC); and a universal binary would ship an arch that can't be validated
  without Intel hardware. Reversible — add `x86_64` on demand if a real Intel user
  ever asks.
- **Min macOS 14 (Sonoma).** Gated by the Observation framework (`@Observable`) for the
  Rust↔SwiftUI state bridge; excludes zero Apple Silicon hardware (every M1+ Mac runs
  Sonoma+). The egui-on-Mac beta keeps its 11.0 floor; the floor rises to 14 only at
  the NS3 cutover.
- **About:** standard `NSApplication` about panel + a `credits` attributed string
  (tagline + GitHub link) — maps the current egui About 1:1, native, ~no custom UI.
  A bespoke SwiftUI About is a deferred NS3 nicety.

### ADR-022 — Remembered `last_folder` seeds the Open dialog; bare launch stays empty
*Decided 2026-07-02 (owner); amended 2026-07-03 (owner).* The app remembers the most
recent folder open as `settings::last_folder`. **As originally decided, a bare launch
auto-reopened that folder; the owner reversed that on 2026-07-03 after living with it**
— auto-opening a whole folder nobody asked for felt wrong. Now a bare launch (no CLI
path, no opened document) always lands on the empty "Press O" state, and `last_folder`
only decides where the Open dialog *starts* when nothing is open yet (priority: pinned
`picker_dir` → current photo's folder → `last_folder` → Pictures; see
`engine::picker_start_dir`). The recording mechanics are unchanged: written in
`AppCore::rebuild_playlist` for folder-backed opens only (archives don't update it),
only when the folder *changes*, on the explicit open action — save-on-open rather than
save-on-exit, because opens funnel through one shared choke point, exit has many
teardown paths, "Esc teardown writes nothing" (task #6) stays true, and it survives a
crash. **Privacy (amends the ADR-018 re-scope):** this is a deliberate, owner-approved
exception to "no record of viewed paths" — one folder path, never file names, and still
nothing derived from photo content. Unit tests are kept honest by
`AppCore::persist_prefs` (false in `headless`, true only on live hosts), so opening a
deck in a test never writes the real `settings.toml`.

### ADR-023 — HUD split: keep the CPU compositor for ephemeral overlays; move rich panels to real UI presenters
*Decided 2026-07-04 (owner); revised 2026-07-05 after plan review. Execution plan: [`hud-panels-plan.md`](hud-panels-plan.md).*
`pb-hud` is a CPU software compositor — it rasterizes text/panels into one RGBA8 bitmap
drawn as a single alpha-blended quad. That is exactly right for **ephemeral, non-interactive**
overlays (toasts, the basic info line, play/rotate hints, scan chip, tooltips, empty-state
CTA) but it has grown into a **mini UI toolkit** for panels it was never meant to host —
EXIF/details (cut off, no real scroll), the folder tree (bad scroll, hand-rolled hit rects),
keyboard help (long, unscrollable), recognized text, and AI descriptions/answers. These are
content the user may want to read, scroll, select, or copy. **Decision:** split the HUD into
two layers. (1) The CPU-quad HUD **stays** for the ephemeral layer. (2) Rich panels move to
real retained UI presenters over a shared, shell-neutral panel model in `pb-app-core`.

The load-bearing boundary is **semantic panel data + actions**, not a generic widget schema:
`pb-app-core` owns row order, labels, values, copy payloads, Markdown/plain source, folder
targets, loading/error states, and `PanelAction`s; presenters own layout, scroll, selection,
focus, styling, and native accessibility. Windows/winit gets an in-viewport **egui** presenter
because Windows has no native toolkit in this app and `pb-ui` already covers the egui design
system. This work is not throwaway. macOS should instead consume the same models through FFI
and render them with **SwiftUI/AppKit** by default. The earlier idea of a macOS egui
`RawInput` bridge is demoted to a fallback for a concrete short-term parity need, not a
speculative hedge.

Rationale: a Markdown library alone would still not give the HUD scroll, selection, copy, or
hit testing; building our own layout/scroll/selection toolkit reinvents egui/AppKit badly;
and a cross-toolkit widget abstraction would become a third UI framework. Dual presenters are
acceptable only because they are thin: egui on Windows, native SwiftUI/AppKit on macOS, both
driven by the same `pb-app-core` models. The viewport was never a native control — it is a
custom wgpu photo renderer — so this is a chrome-layer correction, not a rewrite of the photo
path.

**Prime directive safe:** with no rich panel visible, no egui/SwiftUI panel render runs, no
hidden-panel repaint wakes the frame pump, and the resident-ring present path is untouched.
Verification must include the scripted keypress workload with every rich panel hidden and
report p50/p95/p99 before/after. **Pilot sequence:** extract panel models first while keeping
`pb-hud` as the initial consumer; stand up the Windows egui overlay seam; migrate the Windows
folder tree; build the macOS folder presenter natively (SwiftUI `List(..., children:)` /
`OutlineGroup` first, AppKit `NSOutlineView` only if needed); then move EXIF/details, Help,
Text/OCR, and Description/Ask one at a time, retiring each rich `pb-hud render_*` as it lands.
Task #44 keeps an interim pure `markdown_to_plain` stopgap until Description reaches a rich
presenter.

**Update — 2026-07-05 (owner design review; plan doc revised in step):** panels get real
chrome and a placement paradigm. **Esc stays unconditional quit** (owner: insta-quit is a
priority feature) — panels close via an in-band ✕, their hotkey, the menu, or Tab-hide;
corollary: no inline text inputs in panels (Ask stays a dialog/sheet where Esc means
dismiss). **Details/Text/Describe become tabs of one floating Inspector panel** — the
existing single-content-slot semantics made visible instead of silently replacing each
other; the basic `i` info line stays on the ephemeral HUD. At most three floating panels
(Inspector, folder tree, Help), each with a title bar, ✕, drag-to-move, and copy-all where
useful; free overlap, last-interacted-on-top; no resize/snap/dock in v1 (docked mode is
deferred and shares the photo-sub-rect concept with task #43's demoted split view).
**`Tab` toggles global panel visibility** (Photoshop idiom): hidden ≠ closed, and any panel
action while hidden reveals first. Panel **positions** persist (footprint, not trace —
ADR-018); open state is session-only. Render seam committed: egui draws to an offscreen
`Rgba8Unorm` texture composited by the existing overlay pipeline into the fp16 intermediate
(color-pipeline-correct on HDR, texture retained across nav frames, shared device/queue).
Also recorded in the plan: key releases always reach the held-key tracker (stuck-fly
guard), the wheel-routing reversal vs the folder-tree plan, the Markdown
no-remote-fetch rule (ADR-018), the AccessKit correction (Windows egui accessibility is
nearly free), and the verification fix — the scripted-workload runner does not exist yet,
so Phase 1 builds a minimal headless `CoreEvent` replay dumping `StageTimes` percentiles.

**Update — 2026-07-04 (Phase 0 done; two owner course-corrections):** (1) The basic `i`
info line is now **fully independent**, not a shared-slot occupant — its own permanent
`pb-render` layer (bottom-right), so `i` and `⇧I`/`T`/`D` are orthogonal and the line coexists
below whatever rich panel is open (the panel reserves a bottom strip and lifts above it).
This supersedes the earlier "basic line shares the overlay slot" note above; it fixes a real
`i`/`⇧I`/`i` dead-input bug and matches the idiom that the panels are their own things. A
line-**alignment** preference (left/center/right) is a recorded later garnish (centered will
need a toast-stacks-above rule). (2) The remaining phases are **re-sequenced macOS-first**:
native SwiftUI presenters (Help pilot → folder tree → Inspector tabs) before the Windows egui
seam, because the Mac path needs no render-seam work and the owner smokes on macOS. This
surfaces the **per-shell "present panels natively" seam** now (host capability flag → core
suppresses that panel's HUD rasterization + emits a state-changed marker → Swift pulls the
flattened model over FFI); the ephemeral layer is never suppressed. Windows keeps today's HUD
panels until the egui phase. Phase 0 + the info-line decouple are implemented and green (542
tests); see [`hud-panels-plan.md`](hud-panels-plan.md).

---

### ADR-024 — Two viewing modes, one residency invariant: previews are blazing-only; interaction serves from a resident, display-capped Original pyramid
*Decided 2026-07-18 (owner). Execution: task #110 ([`110-gpu-lanczos-from-original.md`](../plans/110-gpu-lanczos-from-original.md)) and item-6 ([`106.7-item6-retain-remap-SPEC.md`](../plans/106.7-item6-retain-remap-SPEC.md)). Root-caused from the stuck-preview / scale-swap / 1s-re-decode bug class.*

The viewer has **two modes with opposite priorities**, and a whole bug class — a photo stuck on a
blurry preview, switching scale modes flashing a ~256 px thumbnail, the ~1 s re-decode after a
fullscreen toggle — all trace to **one root**: a single preview-first pipeline served *both* modes, so
the speed shortcut (an embedded ~256 px preview) leaked into the tier where quality is the entire point,
and downstream logic then trusted that thumbnail as the real image. **Decision:** make the mode split
explicit and give each mode its own residency rule.

- **Blazing** (a nav key is held, `held_nav().is_some()`): decode-to-fit **previews**, throwaway, in the
  resident ring at the current display size. Speed is the only currency; a preview is *correct* here.
  The keypress→photon hot path is untouched.
- **Interacting / parked** (`held_nav().is_none()`): the current image — plus a small neighbour window
  for instant prev/next — holds a **mipmapped full-res `Original`**, and **every** display (Fit at any
  window size, Fill, 1:1, zoom) is a **pure GPU derivation from that pyramid**. A preview must **never**
  appear here. RAM is spent freely, bounded (below).

**Invariant:** *a preview is a blazing-only asset; the interaction display is a pure function of the
resident Original pyramid.* First enforcement is shipped — the async prefetch requests a preview only
when decoding a Fit (`allow_preview = decode_fit().is_some()`), so the native tier is never fed a
thumbnail.

**Residency — bounded, and machine-adaptive by construction.** The key that carries this from a 32 GB
RTX 5090 down to a 4 GB laptop: **cap the resident pyramid's L0 to ~display resolution (× a small zoom
headroom), not the image's native size.** A fit-to-screen view never needs more source pixels than the
screen shows, so a 24 MP and a 100 MP photo cost roughly the same resident footprint for *viewing*
(display-bound, not image-bound), and the budget self-scales because weak machines have small screens
(a 1080p pyramid ≈ 21 MB; a 29 MP-display pyramid ≈ 160 MB). Total budget = a fraction of detected
RAM/VRAM; the neighbour radius (default 1 = current ± prev/next) auto-drops to 0 under tight memory. The
blazing Fit ring is separate and unaffected.

**Gigapixel is the one deliberate seam.** The capped pyramid serves every fit-to-screen view and true
1:1 / zoom on any normal photo (≤ ~30–60 MP) instantly. It degrades only for **true 1:1 pixel-peeping of
an above-cap file** (a 100 MP Hasselblad shows a native crop that needs those pixels). Today that stays
**clamped** (`clamp_to_max`, current behaviour — no regression); holding it whole is fine on a strong
machine; and **decode-just-the-visible-region on demand** is a *named, deferred* escalation — built only
if someone actually pixel-peeps gigapixel neighbours, never speculatively.

**How the roadmap serves this (it validates the plans, it does not replace them).** #110 (GPU-derive an
exact-size Lanczos Fit from the mipped Original) is the pyramid → any-size-quality **sampler**, the
linchpin. item-6 (retain the Original across geometry changes + "a Fit display may be satisfied by a
resident Original") keeps the pyramid resident and authoritative on nav. #110's `full_res_eligible` +
VRAM-accounting work (Phase 1b) is where the display-capped budget lives.

**Prime-directive safe:** the split rides the pre-existing `held_nav()` seam, so the blazing
keypress→photon path is untouched (no full-res, no derive on the keypress frame); the interaction tier
activates only when parked. The gut-check: the moment someone stops to scrutinise a 60 MP macro —
eyelashes, pore stipple, a capillary in the sclera — they get every pixel immediately, or the app has
failed at the one thing it exists to do.

**Enforcement (queued — the invariant must be level-triggered, not left to edge triggers + a correct
`held_nav`).** A rare stress-test bug (outrun the ring, flip fullscreen → stuck on a preview until a
resize) traced to `held_nav` sticking `Some` after a lost key-up, which suppresses the sharpen (both the
tick's re-issue and `sharpen_now` gate on `held_nav().is_none()`). Because that race is unreproducible,
the fix is a **safety net that enforces the invariant regardless of cause**: a displayed image that has
stayed a resident *preview* past ~0.5 s gets its full requested even if `held_nav` claims blazing (a real
blaze never lingers that long, so the hot path is untouched). "Converge to full or self-correct" is the
enforceable form of this ADR; the bug is impossible by construction rather than chased per-race.

---

## Owner decisions (resolved 2026-06-26)

| Q | Decision | Effect |
|---|----------|--------|
| Q-1 Zero-copy vs portability | **Pursue zero-copy now** | Native D3D12 renderer (ADR-002); GPU decode is a primary v1 goal (ADR-012); Mac port is a separate later backend (ADR-002a). |
| Q-2 HEIC/AVIF in v1 | **Both first-class** | Included v1, CPU-decoded (ADR-015); libheif behind a feature flag. |
| Q-3 Color/HDR | **Wide-gamut SDR (P3)** | In-shader moxcms now; HDR via the D3D12 swapchain later (ADR-016). |
| Q-4 Toolchain | **Owner installs it** | Owner sets up `rustup`; plan assumes the toolchain will be present. |
| Q-5 RAW depth | **Embedded-preview only** (default) | Browse via embedded JPEG preview; full demosaic deferred to v2. `rawler` is LGPL-2.1 — revisit if broad RAW coverage is needed. |

> The owner is optimizing for maximum speed over portability. These decisions
> intentionally trade the cheap wgpu Mac port and a simpler build for the fastest
> possible Windows pipeline. The trait seams keep the deferred options open.

### Update — 2026-06-26 (post-spike + codex review): Q-1 reversed
The decode spike (CPU **2.5×**) and upload spike (staging ring **3.4×**) showed the
portable path already clears 120 Hz for the real ≤16 MP corpus, and the codex review
concurred: **A for the v1 engine, C for the architecture.** wgpu + CPU decode +
staging-ring upload is the v1 foundation; native D3D12 + zero-copy becomes a *gated
acceleration backend* (ADR-012 kill criterion), not the default. ADR-002, ADR-002a,
ADR-012, ADR-016 revised; ADR-017 added. This **supersedes** the Q-1 "pursue
zero-copy now" decision above. Codex also asked for: previews sequenced before
zero-copy, a high-MP stress test (45/60/100 MP + progressive/odd-chroma/large-ICC/
orientation) measuring p50/p95/p99 decode·upload·ready-miss·keypress→photon, and a
faithful end-to-end upload run — all reflected in the roadmap.

### Parked — 2026-07-03: MSIX / Microsoft Store packaging
Moved out of the task list (was #14.3). Re-open **only if discovery or paid
distribution ever matters** — the signed MSI covers direct distribution fully.
Same date, single-instance decision: **Windows adopts single-instance window
reuse + `MultiSelectModel=Player`** (Explorer multi-select → one invocation →
one playlist), matching the macOS LaunchServices behavior; compare-two-photos
is task #43's split-view (or a future explicit File ▸ New Window), never
accidental process stacking (task #14, re-scoped).

---

## Naming & domain (checked 2026-06-26)

The **PhotoBlaze** name is clear for our purposes: no established photo-viewer
software owns it, and `photoblaze` is free on crates.io. The only same-name
collisions are a small e-commerce photo-editing service and a dormant hobby
GitHub repo (`kerryhatcher/PhotoBlaze`, a photo-management app) — neither in our
space.

**Prospective domain: `photoblaze.app`** — confirmed available (RDAP) on
2026-06-26 and the natural fit for an app. Also open: `.io`, `.dev`, `.net`,
`.photo`. Taken: `.com` (registered 2004, a parked for-sale premium) and `.xyz`
(parked). Grab `photoblaze.app` if/when the project warrants a public presence.
