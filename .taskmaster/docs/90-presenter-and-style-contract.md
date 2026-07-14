# Task #90 — subtitle presenter + style contract (owner spec, 2026-07-14)

Frozen before the cue engine exists, because it is cheap now and expensive later.
Companion to `.taskmaster/docs/90-p08-text-shaping-spike.md` (the shaping gate).

## Owner requirements (2026-07-14)

> "I'm not too particular about whether we render with SwiftUI or cosmic — but I want them
> to look good, sharp, and I want customization of: font · size · color/opacity · outline ·
> shadow · background · rounded corners (if there's a background) · **vertical position
> (including BELOW the video in the black borders, which almost no player gets right)**."

## Decision: ONE rasterizer, one bitmap, both shells composite

Rejected: native SwiftUI `Text` on macOS + cosmic-text elsewhere.

**Why.** Eight customization axes × two implementations = eight chances to drift, and
#90.4 (legibility) is the owner's *core* requirement — "2 px outline, 60 % background" must
mean the same thing on every machine. One rasterizer also makes the whole look
golden-image testable, which a SwiftUI path cannot be.

It is also the better path for **sharp**, not merely the tidier one: `pb-hud` already
carries **tiny-skia** (via resvg), so we get real stroked outlines from glyph paths, real
blurred shadows, and real rounded rects. SwiftUI `Text` cannot stroke at all (you would drop
to `NSAttributedString` + negative stroke width and fake the rest), and the HUD's own
existing outline is an 8-way offset halo — good enough for a toast, not for a subtitle.

**Rejected shortcut:** letting `AVPlayer` render in-container subtitles itself (enable the
legible `AVMediaSelectionOption`). Nearly free for native-played MP4/MOV, but it fails
sidecar SRTs (#90.1), fails MKV entirely (that is the FFmpeg path), ignores every setting
above in favour of the OS Media Accessibility prefs, and would make #99's picker drive two
different selection mechanisms.

### Layers

| | video | subtitle presenter |
|---|---|---|
| **macOS, native** (MP4/MOV) | `AVPlayerLayer`, a sublayer of the Metal canvas — **opaque, above wgpu** | a SwiftUI `.overlay` on the canvas |
| **macOS, session** (MKV/WebM, FFmpeg) | wgpu, in the Metal canvas | the same SwiftUI `.overlay` |
| **Windows / Linux** (winit) | wgpu | `Renderer::set_subtitle_overlay` |

macOS needs **no** wgpu subtitle path: SwiftUI composites `.overlay`s over the whole
`NSView`, so one overlay sits above *both* video paths. This is not a new mechanism —
`InfoLineView`, `PlayHintView`, and `ToastView` already live exactly there
(`PhotoBlazeMacApp.swift`). The wgpu overlay is drawn after the video scene, before
controls/toasts.

Both presenters consume the **same shell-neutral RGBA8 bitmap + placement rect**. Cleared on
nav/stop/new-session. **Zero per-frame work when off**, and zero when on: the bitmap is
rebuilt only on cue / style / viewport change, never per frame (spike: 0.15 ms mean per cue
against an 8.3 ms frame).

## `SubtitleStyle` (#90.4) — serde defaults + clamps

```rust
pub struct SubtitleStyle {
    pub font_family: Option<String>,  // None = system sans. Picker lists cosmic-text's
                                      // fontdb families — one list, every platform.
    pub size_pct: f32,                // % of VIEWPORT HEIGHT, not points: resolution- and
                                      // display-independent, so it reads the same on the
                                      // ultrawide and the Studio.
    pub color: [u8; 4],               // RGBA — opacity is the alpha
    pub outline_px: f32,              // 0 = off. A real tiny-skia stroke of the glyph path.
    pub outline_color: [u8; 4],
    pub shadow: Option<Shadow>,       // { dx, dy, blur, color }
    pub background: [u8; 4],          // alpha 0 = off
    pub background_radius: f32,       // rounded corners; only meaningful when background.a > 0
    pub background_pad: f32,
    pub vertical_offset_pct: f32,     // SIGNED — see below
    pub max_line_pct: f32,            // max line width as % of viewport width
    pub line_spacing: f32,
}
```

Appearance **persists** (config). Per-file track choice and cue state stay session-only
(privacy #2: no record of what was viewed).

## Vertical position — the thing almost no player gets right

**`vertical_offset_pct` is signed and measured from the *video's bottom edge*, in % of
viewport height.**

```
        ┌─────────────────────────┐  ← viewport
        │      ▓▓▓ letterbox ▓▓▓  │
        │  ┌───────────────────┐  │
        │  │                   │  │
        │  │      picture      │  │
        │  │                   │  │
        │  │   +  (into pic)   │  │
        │  └───────────────────┘  │  ← 0.0  = baseline on the video's bottom edge
        │      −  (into bar)      │  ← negative = DOWN INTO THE LETTERBOX
        └─────────────────────────┘
```

- `0.0` — classic: just inside the picture's bottom edge.
- `> 0` — up into the picture.
- `< 0` — **down into the black bar.** The owner's ask.
- Clamped to the viewport.

**Why players get this wrong:** they pick one anchor and lose. Anchor to the *video* and you
can't go below it (clamped at 0). Anchor to the *window* and the text drifts relative to the
picture every time the clip's aspect changes — fine for one film, wrong for the next.
Anchoring to the video edge but allowing a **signed** offset tracks the picture across clips
*and* reaches the bar.

**Degradation (must be tested):** Fill mode, zoom, and a clip matching the display aspect
have **no letterbox**. A negative offset then clamps to the viewport bottom — subtitles land
at the picture's bottom rather than off-screen. The setting never becomes invalid, it just
runs out of room.

**Interaction:** the playback controls occupy the bottom. Bottom subtitles lift above them
*while the controls are visible* — including a subtitle parked in the bottom bar. The
controls auto-hide, so the lift is transient.

Placement is pure math (viewport rect + video display rect + style → subtitle rect) in
`pb-app-core` — unit-testable with no GPU, and the same numbers feed both presenters.

## Sharpness — the discipline that decides whether this looks good

⚠ **Rasterize at PHYSICAL pixels, and re-rasterize on backing-scale change.**

`pb-hud` has no `scale_factor` of its own — it works in physical px and the *caller* scales
(`15.0 * viewport.scale_factor` in `app_core_impl`). Subtitles must do the same:
`size_px = size_pct * viewport_height_px`, where the viewport height is already physical.

This is the project's known sharp edge, not a hypothetical: the owner runs a 1× ultrawide
plus 2× Studio displays, and **backing-scale transitions already cause blurry/hit-test bugs**
(see the `owner-display-setup` note). Dragging the window between them **must** re-rasterize.
Treat scale as part of "viewport change" in the rebuild rule, and add a test that a
scale-factor change invalidates the cached bitmap.

Corollary: never rasterize once at a logical size and let the GPU/`CALayer` scale it up.
Set `contentsScale` on the macOS layer.

## Test plan

- **Pure (no GPU):** placement math — offset 0 / +ve / −ve; with and without letterbox; Fill;
  zoom; rotated; clamping; controls-visible lift. Style serde defaults + clamps.
- **Golden image** (the project already has headless wgpu + nv-flip): each style axis —
  outline on/off, shadow, background + radius, and the letterbox placement. Plus the spike's
  scripts (Arabic RTL, CJK fallback, mixed bidi) so shaping regressions are caught.
- **Scale:** the same cue at 1× and 2× produces different bitmap dimensions (proving physical
  px), not the same bitmap scaled.
