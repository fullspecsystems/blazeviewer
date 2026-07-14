# Task #90 P0.8 — text-shaping spike findings (2026-07-14)

Throwaway spike (`pb-hud/examples/spike_shaping.rs` + `spike_hudglyph.rs`, both deleted
after this record), run on macOS 15 / Apple Silicon, to answer the gate the plan put in
front of #90: **can we lay out real subtitle text at all?**

**Verdict: `cosmic-text` 0.19 clears the bar. #90 is unblocked** — with one correction to
the plan's design.

## Why the gate existed

The plan's original premise ("resvg/usvg/tiny-skia text") was wrong: HUD text is **fontdue**
(`pb-hud`), rasterized **per scalar** from **one hardcoded face**, with **no shaping and no
fallback**. Verified: `crates/pb-hud/Cargo.toml` has `fontdue = "0.9"`; resvg is there only
for the Font Awesome icons. That is fine for `IMG_0001.jpg` and unusable for subtitles,
which are arbitrary human language.

## Q1 — does it shape? **YES**

| case | chars | glyphs | rtl run | verdict |
|---|---|---|---|---|
| `السلام عليكم` (Arabic) | 12 | **11** | yes | contextual forms + RTL reordering |
| `हिन्दी` (Devanagari) | 6 | **4** | – | conjunct + reordered vowel sign |
| `שלום עולם` (Hebrew) | 9 | 9 | yes | RTL base direction detected |
| `Hello العربية world` | 19 | 19 | – | bidi: LTR base, Arabic reordered within |

`glyphs != chars` is the proof — fontdue cannot produce that at all.

## Q2 — does system-font fallback work? **YES — zero `.notdef` across six scripts**

Resolved automatically from the system db, with no configuration:

- Arabic → **Geeza Pro** · CJK → **PingFang SC** + **Hiragino Sans**
- Devanagari → **Devanagari Sangam MN** · Hebrew → **Arial** · Latin → **System Font**

Note CJK needed *two* faces for one string — the fallback chain is per-glyph, not per-run.

## Q3 — lazy-init cost: **261 ms / 1114 faces** ⚠ **CORRECTS THE PLAN**

The plan says "lazy-init on first subtitle use". **Do not do that on the event loop**: a
quarter-second freeze on the first cue is exactly the class of stall task 98.6 just removed
from the Details probe.

**Do instead:** build the `FontSystem` on a **worker**, the same shape as
`pb_app_core::media_details` — kick it when a subtitle track is first selected (or when a
video with a subtitle track opens), and render cues once it lands. Cues before it is ready
simply don't draw; a subtitle appearing ~200 ms late on the first cue of a session is
invisible next to a frozen window. `FontSystem::new()` is also a fine candidate to build
eagerly at video-open time, since a 261 ms worker is free.

## Q4 — per-cue cost: **0.15 ms mean / 0.40 ms worst** (warm), **1.3–1.5 ms** first-use-per-script

Measured over 50 *fresh* 2-line cues (text varied so nothing cached trivially), shape +
rasterize. One 120 Hz frame is 8.3 ms, so a cue costs ~2 % of a frame **on cue change
only** — never per frame. The first cue in a new script pays ~1.5 ms (font load + fallback
search); after that the `SwashCache` warms.

Implication: the plan's "shape/rasterize only on cue/style/viewport change, glyph+bitmap
cache" is comfortably right, and does not need to be clever.

## Q5 — RGBA8 output for the presenter contract: **YES**

`Buffer::draw(&mut SwashCache, Color, |x, y, w, h, color| …)` yields per-pixel spans on the
CPU with no GPU involved — exactly the shell-neutral bitmap P0.7 needs to hand to **both** a
wgpu overlay and a macOS `CALayer`.

## Dependency cost: acceptable, no C

cosmic-text 0.19 pulls ~15–20 crates, **all pure Rust** — `fontdb` 0.23, **`harfrust`** (the
HarfBuzz port; 0.19 uses it, *not* rustybuzz), `read-fonts`/`font-types`, `swash`,
`unicode-bidi`, `memmap2`, `slotmap`. **No native/C build**, so ADR-015 stays clean and it
ports to every target unchanged. `pb-hud`'s "deliberately UI-toolkit-free, pure-Rust
rasterization" charter survives.

## P0.7 presenter premise — **VERIFIED, the plan is right**

`mac/Sources/PhotoBlazeMac/NativeVideoPlayer.swift`: *"attaches its `AVPlayerLayer` as a
sublayer of the Metal canvas"*. So macOS native video really is an **opaque layer above
Metal**, and a wgpu subtitle quad would draw **underneath the movie, invisible**. macOS needs
a subtitle `CALayer`/`NSView` above `AVPlayerLayer`; the session/wgpu path (Linux, Windows,
and macOS MKV/WebM via FFmpeg) needs `Renderer::set_subtitle_overlay`. Both consume the same
RGBA bitmap — which Q5 says we can produce.

---

## Bonus finding (pre-existing bug, NOT #90) — the HUD cannot render non-Latin filenames

`pb-hud` loads exactly one face (`/System/Library/Fonts/SFNS.ttf` on macOS, Segoe UI on
Windows) and has no fallback. Tested against that face directly:

```
  latin: "Photo.jpg"  -> missing glyphs: []
    cjk: "写真.jpg"    -> missing glyphs: ['写', '真']
 arabic: "صورة.jpg"   -> missing glyphs: ['ص', 'و', 'ر', 'ة']
```

So a file named `写真.jpg` renders as `□□.jpg` wherever the HUD draws text — the winit
shell's info line and panels (Windows/Linux). macOS draws its panels natively in SwiftUI, so
it is unaffected there; **worth confirming on Windows** rather than assuming.

This is independent of subtitles and predates #98/#90. Worth its own task — and if #90 adds
cosmic-text to `pb-hud` anyway, fixing it may be nearly free (the HUD's text path would move
to the same shaper, gaining fallback for every script at once).
