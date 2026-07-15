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

It is also the better path for **sharp**, not merely the tidier one. SwiftUI `Text` cannot
stroke at all (you would drop to `NSAttributedString` + negative stroke width and fake the
rest), and the HUD's own existing outline is an 8-way offset halo — good enough for a toast,
not for a subtitle.

*As built (2026-07-14):* the rasterizer composites from a single glyph coverage mask —
outline = an exact **circular dilate** (mathematically the outer half of a stroke, and
unlike the 8-way halo it has no diagonal gaps), shadow = a 3-pass separable box blur
(indistinguishable from Gaussian, O(n)), background = an antialiased rounded-rect SDF. All
of it CPU, all of it in `pb-hud`. (The earlier note here said "stroked glyph paths via
tiny-skia"; the dilate is equivalent for an outline *around* text and needs no path
extraction, so tiny-skia isn't used for this after all.)

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
    pub outline_pct: f32,             // 0 = off. An exact circular dilate of the glyph
                                      // coverage = the outer half of a stroke.
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

---

# As built (2026-07-14) — what shipped, and where reality corrected this doc

The plan above was frozen before anything was wired. It held up well: the one-rasterizer
decision, the signed-offset anchor, and the physical-px discipline all survived contact.
Four things it did **not** anticipate are recorded here, because each one cost real time and
none is guessable from the design.

## The sequencing mistake (read this before starting a slice)

The four pure modules (discovery, cues, style/placement, rasterizer — ~1500 lines, ~75 tests)
were all built and merged **before a single caller existed**. The owner twice tried to test
subtitle playback that did not exist. Tests passing on a module nothing calls is not progress.

The correction — and the rule for the rest of #90: **prove the pipe, then build through it.**
The thin slice (one real cue on screen, end to end) found five defects in an afternoon that
the 75 unit tests had not, because they were all *wiring* defects, invisible from inside a
module.

## Where the bugs actually were

Every one was in a seam, not in the logic the tests covered:

1. **The tick skipped `update()`.** `tick_subtitles` had an `if mode == Off { return }` fast
   path; `update()` is what hides the overlay. So `C` left the last cue frozen on screen
   forever. The unit test called `update()` directly and passed. **Rule now: every "nothing
   should be on screen" case leaves through ONE exit that clears.**
2. **The preference didn't apply at launch.** `new_host` builds the core from *default*
   settings and loads the real ones afterward, hand-copying derived state across. The engine
   was left off that list, so `subtitles = true` on disk launched with captions off.
   Anything deriving state from settings must be re-derived there.
3. **A coordinate-space mismatch (the zoom clipping).** The core places the block in the
   **canvas's** space (full window, physical px ÷ scale). Attached down the SwiftUI chrome
   chain, the overlay measured `(0, 32, 2651, 1754)` against the canvas's `(0, 0, 2651, 1786)`
   — SwiftUI re-applies the titlebar safe-area inset. Every subtitle rode ~52 pt low, which
   is **invisible until the block clamps flush to the bottom** (zoom / Crop-to-Fill) and then
   the last line is cut by exactly the inset. Fixed by attaching the overlay to the canvas,
   inside its existing `.ignoresSafeArea()`. A trailing `.ignoresSafeArea()` on the whole
   chain would have "worked" and silently moved the panels and scrim.
4. **The backend gate (the subtle one).** Subtitles gated on `video_session_active()`. When
   Phase 3F made the macOS sample-buffer presenter the default route for MKV/WebM — a
   `Native`-proxy backend — that check went false and subtitles silently switched off for
   exactly the files they were built for. No error, no failing test.
   **Rule: never gate a feature on a *backend*; gate it on the *thing* (`video_showing()` /
   `video_position()`).** The routing will change again.

## Corrections to the design above

- **The offset is a margin from the nearer edge, not just the video's.** The doc says a
  negative offset "clamps to the viewport bottom" when there's no letterbox — true, but the
  same clamp applied to a *positive* offset parked the text flush against the window edge on
  a zoomed clip. The offset is a legibility margin, so it now holds its gap from whichever
  bottom edge is nearer. A negative offset is exempt (it deliberately targets the letterbox).
- **Multi-line cues must be centered.** cosmic-text defaults to left, which rendered a short
  line over a long one as ragged-left. Every player centers. Pinned by a test that measures
  the short line's margins.
- **The rasterizer must be built on a worker.** `FontSystem::new()` is 261 ms. Built once and
  kept — per-video would be worse. The doc's "lazy-init on first use" would have stalled the
  event loop.
- **`PB_SUBTITLE_TRACE=1`** prints why nothing is on screen. The pipe has six gates (clock,
  placement, sidecar, cues, font system, bitmap) and a silent failure at any one looks
  identical from outside. It turned "I don't see any subtitles" into a diagnosis in one run.

## Shipped

Discovery (#90.1) · cues (#90.2, sidecar only) · style/placement/rasterizer (#90.3/.4 core) ·
the engine + macOS presenter · `C` / View ▸ Subtitles + persisted preference.

---

# As built, part 2 (2026-07-15) — the embedded tier

#90.2 is complete: cues *inside* the container render, and selection finally goes through
the catalog. Four things reality taught that the design above could not have known.

## The sequencing rule paid off again — use it

The reader was run against a **real MKV before anything was built on it**. That one step,
before any wiring, is why the two findings below were design-time facts rather than owner
bug reports. Do this on every slice.

## 1. Reading an embedded track costs a full pass over the container — so it streams

Subtitle blocks are scattered through every cluster; `av_read_frame` must walk every
interleaved packet to find them. **Measured: 39 s** on the corpus MKV (4.4 GB over SMB).
There is no index to seek — this is inherent to the format, not a tuning failure.

The resolution is a **ratio**, and it is the most important thing on this page: the reader
walks in *presentation order* at ~113 MB/s, while playback consumes at ~1.6 MB/s — **~70×
faster than playback needs**. So it hands cues over as it finds them instead of returning
a lump. **Measured: first cue at 1.06 s**, 177 batches. Cancelling stops a read in ~1 s.

> **The optimization NOT to take:** the playback demuxer already reads these exact packets
> and throws the subtitle ones away; forwarding them would make cues free. But it exists
> only on the routes that use *our* demuxer — and this task's own scar (bug 4 above) is
> that **a feature must never be gated on a backend**. Layer it *under* the reader if it is
> ever built. Never replace it.

## 2. `ffmpeg-next` has two landmines in the subtitle API (verified in 8.1.0)

- **No `Drop` for `Subtitle`, and it never calls `avsubtitle_free`.** The decoder
  heap-allocates rects on every successful decode. Free them yourself or leak per cue.
- **`Text::get()` / `Ass::get()` are `from_utf8_unchecked`** — UB on the non-UTF-8 payloads
  that genuinely exist (a CP1252 `.srt` muxed without `sub_charenc`). We read the C string
  through the raw pointer with `from_utf8_lossy`. We parse hostile bytes for a living.

One genuinely nice surprise: **FFmpeg normalizes every text decoder's output to one ASS
envelope** (`ff_ass_get_dialog` → `ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,
Effect,Text`). Eight commas, then text. So `subrip`, `webvtt`, `mov_text`, `text`, `ass`
and `ssa` all cost exactly one pure function to unwrap — count the separators, don't split
(dialogue is full of commas).

## 3. Subtitles are the worst-encoded text on a computer — and it is fixable

The corpus MKV's embedded track carries `â™ª` where `♪` belongs. **`ffprobe -show_data`
proves it is in the file** (`c3a2 e284 a2c2 aa`), while the `.eng.srt` beside it is clean
(`e299 aa`). The muxer double-encoded it. Nobody noticed because nothing rendered the
embedded track before.

`pb_app_core::mojibake` undoes it — and the reason this is safe is that **it is a proof,
not a guess**. Mojibake is lossless and reversible: re-encode the run to CP1252 and require
the bytes to be valid UTF-8. `café` → `63 61 66 E9` → `E9` is a lead with no continuation →
invalid → untouched. Applied per-**run** at the single choke point both tiers pass through.

Two things a future editor must not undo:

- **`SAFE_LEADS` excludes `à`/`á` on purpose.** French `voilà «` is `E0 A0 AB`, which *is*
  valid UTF-8 and round-trips into a Samaritan letter. Validity alone is not enough. The
  cost — mojibake'd Thai/Devanagari stays broken — is the right trade against corrupting
  correct French/Spanish/Portuguese. Pinned by a test.
- **The five WHATWG windows-1252 passthrough bytes** (0x81/8D/8F/90/9D) must be mapped even
  though IBM leaves them unassigned. `”` is `E2 80 9D`: without 0x9D, every *closing* curly
  quote stays broken while the opening one repairs. **Found by a test failing for a real
  reason** — the spec would have talked you out of it.

## 4. `Automatic` needed a fallback chain (⚠ owner review)

The frozen rule was forced-only-matching-the-audio. It is exactly right for its stated
purpose — stopping `Automatic` from enabling subtitles *by itself*. But `Off` is the
default, so the mode is only ever `Automatic` because the user pressed `C` and read a toast
saying "Subtitles on" — and strict forced-only then shows **nothing** for the most common
case in existence (English film, full English track, no forced track). That is the same
surprise pointing the other way, and it would have silently regressed the 2026-07-14
validated behaviour.

Now: forced+matching-audio → the container's default → anything renderable. Nothing can
turn subtitles on by itself; the fallback only fires once you have asked. Flagged in
`SubtitleMode`'s docs. **Owner: confirm or overrule.**

## Remaining — see `.taskmaster/tasks/tasks.json` #90 and #99
- **#90.3** — seek generations (no stale cue flash on scrub); wire `controls_h` so cues lift
  above the transport bar (`place()` supports it; nothing measures it).
- **#90.4** — the Settings UI. All eight axes are implemented, clamped, and tested, but
  reachable only from code. Owner's read on the defaults so far: *"a bit big for my taste."*
- **#90.5** — the winit shell has no presenter (`Renderer::set_subtitle_overlay`); Windows /
  Linux show nothing. Worth deferring until the style defaults settle.
- **#99** — the picker. `Shift+C` cycling + toast shipped 2026-07-15 (`cycle_choices` /
  `next_choice`, pure + tested); the engine now reads the catalog, so `resolve_track` is live.
  Remaining: `A`/`Shift+A` audio cycling, the CC button, the popover. ⚠ Audio must toast only
  on a *confirmed* switch — subtitles may toast optimistically. The asymmetry is deliberate.
