# Blaze Viewer

Blaze Viewer is an image and video viewer with an obsession: Make viewing as fast and smooth as possible.
A key feature is "Blaze Mode": seeking through photos in a direction (forward, backwards, or 'randomly') as fast as a user wants, within the limits of the hardware.
The feel and acceleration of this is customizable, but on a powerful computer with a 120Hz display, that means we can literally display 120 images per second in a way that is actually useful. When a user stops and parks on an image, operations like rescaling, toggling fullscreen, and playing videos are all feel instant.

## Prime Directive

**Every decision is answered by one question: "Does this make the user's next
likely action feel closer to instant?"** If it serves that for no mode and isn't free,
it doesn't ship. Speed is the feature — and speed is a property of
*interactions*. What the next likely actions *are* depends on
mode: blazing, it's the next items in the travel direction; parked, it's zoom,
1:1, rotate, fullscreen, compare, play — operations on *this* item. On high-end,
modern hardware "instant" is almost always achievable for our purposes.

Three corollaries:

1. **Anticipate.** Do the work before it's asked for. The prefetch ring is
   the blaze-mode instance of a general rule: whatever the user plausibly
   does next should already be resident when they do it. Every new feature
   answers "what are its likely interactions, and what do we pre-arrange?"
2. **Never repeat likely work.** Deterministic derived results — decodes,
   scaled derivatives, posters — are retained within budget, so the second
   time is a rebind, not a recompute. Toggling fullscreen twice must never
   decode twice. (Greedy within enforced budgets, evicted by likelihood —
   see *spend the hardware* below.)
3. **Measure.** Perceived-speed claims require numbers from the corpus —
   per-interaction latency, p50/p95/p99, never means. Non-obvious hot-path
   choices go behind swappable seams and get A/B tested (*Instrumentation*).

> ⚠️ The original reading of this directive — "faster" meant navigation
> throughput — produced an app that blazed at 120 Hz and took seconds to
> toggle fullscreen, every single time. That is the failure this wording
> exists to prevent: optimizing one global metric while the user's actual
> next action goes cold. As scope grows (video, archives, editing), the
> question is never "is the app fast?" — it's "which interaction just became
> likely, and is it instant?"

## Second Directive: private by default

Blaze Viewer is fast **and** private: **it keeps no record of what you
viewed.** You should be able to look at anything and have nobody — later, on
this machine or anywhere else — be able to tell. Privacy ranks just after
performance, and it is why some "free" performance is deliberately left on
the table: a persistent central thumbnail/pixel cache would speed cold starts
*and* leak viewing history as an audit trace, so it doesn't exist. Every
derived result we retain (the *never repeat likely work* corollary) lives in
RAM/VRAM and dies with the process; any future on-disk cache must be
**explicit opt-in and user-clearable**, never a default.

Two hard rules follow:

- **No involuntary traces of viewing.** No thumbnail DB, no MRU of photo
  paths, no decoded-pixel temp files, no logs of viewed items. Explicit user
  edits (delete, save rotation) are a separate, allowed category. The full
  contract and its enforcement (the no-trace test, the static audit,
  ADR-018/022) live in *Privacy guarantee* below.
- **Nothing leaves the machine by default.** Any feature that sends content
  or metadata to another system — AI descriptions, cloud OCR, anything
  network-touching — is explicit opt-in, gated behind a clear warning that
  says what goes where. Never on silently.

> Licensing Note: A dev build linking `/opt/homebrew/opt/ffmpeg/...` (GPL) is 
> **expected and correct** — dev and release use different FFmpeg.

## The performance model (read this before optimizing anything)

A real session alternates between two modes, and the architecture serves both:

1. **Blazing** — flipping through items as fast as the brain can process them
   (rate user-tunable). The budget: **keypress → photon ≤ one refresh interval**
   (~8.3 ms @ 120 Hz). A direction-biased prefetch ring decodes right-sized
   pixels ahead of the user into resident GPU textures, so **a keypress is a
   rebind — never a decode, never an upload.** Holding a key self-paces to the
   newest *ready* frame each vsync: blaze when decode keeps up, degrade to
   previews when it can't. Throughput is capped at refresh — the job is "one
   fresh frame per vsync with an instant preview fallback," not "infinite fps."
   A video's poster frame is its blaze-mode face.
2. **Parked** — the user settles on one item. Every interaction — fullscreen,
   fit/fill/1:1, zoom, pan, rotate, compare, the metadata panel, starting
   video playback — gets the same ≤-one-refresh budget, and quality is
   **maximum**: original pixels, correct color, real metadata. The parked
   metrics are **interaction → photon** and **time-to-max-quality after
   parking**.

The modes are not two code paths. They are two ends of one **cancellable
refinement ladder**: embedded preview → decoded-to-fit → full quality. Blazing
shows whatever rung is ready; parking just lets the ladder finish. Any nav
input cancels in-flight refinement — cancellation is what makes generosity
affordable.

Two consequences that are easy to get wrong:

- **Decode-to-purpose.** Each rung decodes only what its job needs: a blaze
  frame needs display-fit pixels (`pb-decode::FitBox`; native scaled decode
  like JPEG DCT scaling is the lever, though measured inert for ≤24 MP photos
  on the 7680-wide target — full-decode throughput dominates there); a parked
  1:1 view needs the original. Neither over-decodes for its purpose.
- **The wall is decode throughput, not GPU draw.** Drawing a textured quad is
  microseconds; the naive "tune the rendering" instinct is wrong. The
  prefetch ring, preview-first, and the decode pool all exist to hide decode
  latency. (The GPU does real work now — Lanczos derive, HDR, video — but it
  is never the display bottleneck.)

**Spend the hardware, inside measured budgets.** RAM and VRAM exist to be
used: a 96 GB / RTX 5090 box should hold big resident rings and eager caches.
But every cache has an enforced budget derived from what the machine actually
has, and allocation is **refuse-before-reserve** — pre-flight the cost, then
decline or degrade rather than thrash. Big machines feast; small machines
degrade gracefully; nobody crashes or swaps.

## Architecture

```
crates/
  pb-core     pure nav: playlist, precomputed-random shuffle, prefetch window, ring
              residency, thumbs — no I/O, no GPU, deterministic, fully unit-testable
  pb-decode   the refinement ladder (preview → fit → full) behind swappable backends:
              stills (image / zune / jxl-oxide / resvg / raw / WIC / libheif / dav1d),
              video + posters + metadata (Media Foundation + FFmpeg demux), subtitle cues
  pb-source   ItemSource seam ("encoded bytes + name for item i"): FsSource or an
              archive — ZIP / 7z / tar family / RAR4 / RAR5, lazy random-access vs
              eager-to-RAM per kind; `archive_kind` is the one classifier. RAM-only,
              read-only on the view path
  pb-render   the wgpu presenter: swapchain + resident texture ring, view transforms
              (fit/fill/1:1, zoom/pan/rotate), staging-ring uploads, GPU Lanczos
              resample, fp16 scRGB color + HDR output, NV12/P010 video planes,
              headless golden-image rendering
  pb-hud      CPU overlay compositor (panels, toasts, pie, chips; fontdue text +
              resvg FA icons) — shell-neutral, split from pb-app in NS0
  pb-ui       the chrome design system: egui tokens + components + light/dark theme.
              egui-only, no app deps; powers the dialogs and the gallery example
  pb-app-core the orchestration core (NS0/ADR-021): AppCore + the engine turning
              CoreEvents into CoreEffects — decode pool, scan, archive opens,
              settings/keymap/config, slideshow + hold-to-blaze timing, the video
              session, subtitles, poster selection, panels/undo/delete/save-rotation.
              No UI toolkit (winit/egui/wgpu-free); filesystem allowed (config, scan)
              — the boundary is "no shell," unlike strictly-pure pb-core
  pb-cli      the clap flag surface as a library (→ LaunchOverrides), shared by every
              shell; never calls process::exit (FFI-safe)
  pb-app      the Windows/Linux winit shell binary: event loop, wgpu surface, egui
              dialogs + overlay, muda menus, WASAPI audio, clipboard, self-update
  pb-mac-ffi  swift-bridge staticlib exposing AppCore to the SwiftUI/AppKit host in
              mac/ (events in via AppCoreHandle, CoreEffects drained on the main
              actor). macOS ships the mac/ host and never links pb-app
```

The crate boundaries *are* the A/B seams. Anything whose "is this faster?" answer
is non-obvious goes behind a trait so alternatives can be benchmarked: decode
backend, cache/eviction policy, present mode, upload strategy.

### Threading
- **Event-loop thread (winit):** input, swapchain, draw. Never blocks on I/O or
  decode. On keypress: advance index → rebind resident texture → present.
- **Decode pool (`pb-app-core::decode_pool`):** a dedicated worker pool with
  **priorities + cancellation** (not bare rayon — work-stealing reorders prefetch
  and there are no priorities). Pulls jobs from the prefetch scheduler,
  decodes-to-fit, hands off for upload. Non-image work kinds (posters, poster
  selection) ride the same pool under the same priority rules.
- **Upload (`UploadStrategy` seam):** v1 uses a **persistent staging-buffer ring**
  (`copy_buffer_to_texture`) — measured ~48 GB/s, 3.4× the 120 Hz budget. Never
  `write_texture` (the trap: 60–75 fps on large frames). Uploads land in the
  resident ring during prefetch — **never on the keypress frame**. A zero-copy CUDA
  alias is the gated escalation behind the same seam.
- **Other workers, same rule:** video playback runs a demand-driven MF reader
  thread + a WASAPI audio engine (the audio clock is the master); archive opens,
  container probes, and RAW demosaic (256 MB stack) get their own threads. All of
  them report back as effects/messages — nothing ever blocks the event loop.

## Test-Driven Development (required)

- **Write the test first.** Especially for `pb-core` logic (nav, random walk,
  prefetch window, eviction) — it's pure, so there is no excuse.
- **Coverage target: >80%**, measured with **`cargo-llvm-cov`** (Windows-native;
  tarpaulin is Linux-only). The hard-to-test GPU/present shell is marked
  `#[coverage(off)]` so the number stays honest rather than gamed.
- **Property tests** (`proptest`) for nav/cache invariants (e.g. a random cycle
  visits each item exactly once; a residency plan never exceeds capacity).
- **Golden-image tests** for rendering: a headless **wgpu** render reads back to
  a CPU buffer, compared to reference PNGs with a perceptual diff (**nv-flip**) and
  a tolerance. Run in CI on a software adapter (**WARP**/lavapipe, no GPU required).
- **Fuzz** the decoders (`cargo-fuzz`) — they parse hostile bytes.
- Keep logic testable by isolating it from I/O and GPU (the whole point of
  `pb-core`). If something is hard to test, that's usually a design smell.

## Instrumentation & A/B methodology (first-class, not an afterthought)

Wired today (all opt-in, RAM-only, privacy-clean — durations, never paths or
pixels):

- **Per-stage timing** (`pb-app-core::metrics`, the `--metrics` flag):
  scan / read / decode / upload / render durations summarized as
  **p50/p95/p99 — never means**.
- **Episodic latency timers** (`pb-app-core::perf`, gated by `PB_PERF`):
  whole user-visible operations — open → first photo on screen, open → every
  photo cached, a Fit↔1:1 or resize switch → back on screen. These measure
  the Prime Directive directly; add a new episode when a new interaction is
  supposed to feel instant.
- **A/B at the seams:** `Box<dyn Trait>` at the swap seam (decode backend,
  cache policy, present mode), generics in the hot inner loop, cargo features
  only for whole-program toggles. `pb-render`'s `ab_report` (an `--ignored`
  test run with `--nocapture`) prints the full A/B/X resample-quality matrix —
  the GPU-Lanczos decision (#110) came from it.
- **Targeted diag env vars** (`PB_TRACE`, `PB_DOOR_DIAG`, …): cheap stderr
  diagnostics scoped to one subsystem; the house pattern for a new
  investigation.

Designed but **not yet wired** (don't cite these as existing):

- **keypress → photon:** QPC input timestamps + DXGI
  `GetFrameStatistics().SyncQPCTime` (flip-model + waitable object +
  max-frame-latency 1), validated against Intel **PresentMon**; macOS via
  `CAMetalDrawable.presentedTime` behind the same seam.
- **Tracy** (`tracing-tracy` + wgpu timestamp queries / `wgpu-profiler`) for
  unified CPU+GPU zones, behind a feature that compiles out of release.
- **Criterion microbenches over the pinned corpus** (`benches/` is empty
  today) and a **CI regression gate** (CodSpeed / iai-callgrind on
  platform-independent code, on Linux — never gate on Windows wall-clock
  noise).

## The UI model — minimal when you want it, everything when you ask

The POC was "a chrome-less window and five keys." That is no longer the truth
and shouldn't be defended: today there are menus, a toolbar, a thumbnail
strip, info/EXIF/details panels, overlays, HUD toasts, and a full Settings
window. What survived the growth is the philosophy:

- **Easily made minimal — not always minimal.** The viewing surface is a
  borderless window (borderless-fullscreen at native res → DXGI Independent
  Flip), and every piece of chrome is dismissible: the app collapses to
  "just the image," and nothing forces itself back on screen.
- **All the information they want, when they ask.** Info panel, full-EXIF
  panel, media details, text-in-image, help overlay — each one keypress away,
  each dismissible.
- **Keyboard-first, fully remappable** (`keymap.toml`), and every surface —
  menu, toolbar, mouse — dispatches the same `Action` vocabulary
  (`pb-app-core::action`), so the keymap, menus, and toolbar can't drift
  apart. The canonical spine: `space`/`backspace` next/prev, `enter` random
  (precomputed shuffle, reversible), `P` play / enter a door, `Esc` quit.
  Bindings are contextual where that reads naturally (arrows pan; over a
  playing video with no horizontal overflow `←`/`→` seek). **`keymap.rs` is
  the source of truth — don't restate the full map here; it drifts.**
- Image fit to screen, centered, never cropped **by default**
  (`pb-render::fit_rect`); fill and 1:1 are a keypress away, all prefetched.
- Hold any nav key to blaze — advance as fast as frames become ready.
- Key handling tracks **held physical keys** (`Pressed`/`Released`), ignoring
  OS key-repeat events, plus a focus-loss release net (avoids known winit
  repeat/lost key-up bugs).

### HUD / toast icons — Font Awesome solid (codified workflow)

Overlay panels (the `pb-hud` crate — shell-neutral since NS0) composite white
outlined text **and optional icons** into
one software RGBA8 pill, drawn as a single alpha-blended quad — rebuilt only on
change, never per frame (off the photo hot path). Icons are **Font Awesome `solid`**
SVGs (the house style): a single `currentColor` path tinted white, with the text's
black-outline pass for legibility. They're vendored into the repo and rasterized via
the same `resvg`/`usvg`/`tiny-skia` stack `pb-decode` uses (`icon::rasterize`).

> **Style decision (2026-06-28, owner):** we tried `duotone` first but switched to
> **solid** — "boring but reliable and effective." Duotone's 40%-opacity secondary
> layer muddied at toast size. Use **solid** for any new icon; don't reintroduce
> duotone without a reason.

**To add an icon:**
1. Find it in the local FA library:
   `D:\Media\fontawesome-pro-plus-7.3.0-web\svgs\solid\<name>.svg`
   (`ls svgs/solid | grep <kw>` to search; always the `solid` weight).
2. Copy it **verbatim** into `crates/pb-hud/icons/<name>.svg`.
3. Add `pub const <NAME>: &str = include_str!("../icons/<name>.svg");` to
   `icon::assets` (`crates/pb-hud/src/icon.rs`).
4. Show it: `show_toast_icon(msg, Some(icon::assets::<NAME>), ..)` for icon+text, or
   `show_toast_icon("", Some(..), ..)` for an icon-only square pill (e.g. rotate).

**Licensing:** FA **Pro** assets are licensed to the owner but **not redistributable**.
The repo is **private**, so vendoring the SVGs is in-bounds. If it ever goes public:
git-ignore `icons/` and load from the local FA path at build, or swap to the free-tier
solid set (most of these icons, including the ones used here, are in FA Free).
(Privacy task #2 is unaffected — the SVGs are compile-time assets, not a viewing trace.)

## Chrome design system — the `pb-ui` crate (don't reinvent components)

The viewer hot path is custom wgpu (above). The **chrome** — the tabbed Settings
window (General / Display / Subtitles / AI / Shortcuts; the Shortcuts tab *is* the
keymap editor, and edits auto-save live), About, the Confirm / Message / Password
dialogs, the Loading / Scanning progress views (determinate bar + Cancel; a
Password dialog turns into Loading in place once accepted), and Ask About Image —
is **egui**, in a second winit window
(`pb-app/src/dialog.rs`), and it is **component-based on `crates/pb-ui`**. The rule:
**when you need a button / field / toggle / card / section, reach for the `pb-ui`
component — never hand-roll one in a dialog.** That's the whole point of the crate; a
one-off `egui::Button` in `dialog.rs` is a bug (it will drift in size/color/radius).

`pb-ui` is egui-only (no app deps), so it stays reusable and testable, and it powers
the **gallery** — the egui equivalent of a Storybook page:

```sh
cargo run -p pb-ui --example gallery   # every token + component, Light/Dark/Both
```

The gallery is dev-only (eframe dev-dependency; never shipped) and is the place to
**preview and A/B the system** — including light vs dark side by side, which a real
single-theme dialog can't show.

**What's there (`pb-ui/src/lib.rs`):**
- **Tokens:** `SPACE_1..6` (4px scale), `GAP` (**the** standard gap — between rows, between
  cards, and the dialog button gap/inset; one knob), `RADIUS_CONTROL`/`RADIUS_CARD`,
  `CONTROL_H` (32px — set once, kills "every control a different size"), `FIELD_MARGIN`,
  `CARD_WRAP_WIDTH`, and `Palette` (named color roles, light + dark).
- **Theme:** `install_fonts` (native Segoe UI) once per dialog ctx; `apply_style(ctx,
  dark)` each frame (cheap; survives egui's own theme bookkeeping); `apply_to_ui(ui,
  dark)` to scope one region (e.g. a gallery column, or a **combo popup** — egui draws
  popup *contents* with the global ctx style, so re-assert it inside `show_ui`).
- **Components:** `group_card` (the **grouped-settings** card: a semibold heading inside
  the card + `card_row`s that **auto-space by `GAP`** — no dividers; related settings share
  one card, so a page is a few cards not one-per-setting), `card` (single), `card_row`
  (responsive: control on the right when wide, stacked under the header below
  `CARD_WRAP_WIDTH`), `toggle` / `toggle_with_label`, `page_title` / `section_label` (type ramp: page title
  30 / section 17 — both semibold via the bundled Segoe UI Semibold face / card title 14.5
  / description 12.5), `primary_button` / `secondary_button` / `danger_button`,
  `text_field`, `slider` / `slider_stepped` (stable-width value box + solid-accent
  fill — no jitter), `tab_bar` (the Settings tabs), `progress_bar` (the Loading /
  Scanning views), `icon_sized`. Section headings are Title Case; setting labels
  stay sentence case. `lib.rs` is the full token/component list — trust it over
  this paragraph.
- **Icons (`pb-ui/src/icon.rs`):** Font Awesome SVGs **vendored per family**
  (`icons/<family>/<name>.svg`), rasterized to a **white square sprite** and **tinted at
  draw time** — one texture serves every tone and theme (cached in the egui ctx). A
  semantic `Icon` enum (`Lock`, `Warning`, `Trash`, …) names *meaning* not glyph; `Tone`
  (`Neutral`/`Accent`/`Warning`/`Danger`/`Success`) resolves through the `Palette` so it's
  light/dark-correct. Placement helpers — `lead_row` (gutter icon centered on the first
  content line: the dialog body shape) and `inline` — bake the alignment, so there is
  **no per-call nudging or top-clipping** (the old pain). The square render is our own
  `fa-fw` — FA glyphs aren't all square (lock is 384×512), so we center every glyph in a
  square box. **Switch families** by flipping `icon::ACTIVE_FAMILY` (vendor that family
  first). The HUD toasts keep their own CPU-composite rasterizer (`pb-hud/src/icon.rs`);
  only the egui chrome uses `pb-ui::icon`.

**Conventions:** primary = accent default action (Save/OK/Unlock); secondary = neutral
(Cancel); danger = red (Delete). Dialog status icons: Password = `Lock`/`Neutral`,
Message = `Warning`/`Warning`, Confirm-delete = `Trash`/`Danger`. `dialog.rs` keeps only
the *scaffold* (the second window + `button_bar` / `dialog_frame` panels) and **composes
pb-ui atoms** inside it. Theme is locked to the OS-resolved light/dark at open (explicit
`ThemePreference`, not `System`, so `apply_style` isn't re-clobbered).

**To add a component:** put it in `pb-ui` (drive it from tokens/`Palette`, take a
`&mut egui::Ui`, return the `Response`), add it to the gallery `catalog`, then use it.
Don't add UI primitives to `pb-app`. **To add an icon:** copy the glyph for each vendored
family from the FA library (`D:\Media\fontawesome-pro-plus-7.3.0-web\svgs\<family>\`) into
`pb-ui/icons/<family>/`, add a variant to `icon::Icon` + a `glyph!` arm, show it in the
gallery's Icons row.

> **Accent (resolved — brand-first):** the accent defaults to the logo orange
> (`BRAND_ACCENT`, `#FF4915`), with an OS/custom override chosen in Settings.
> Every candidate passes a legibility guard (`ensure_legible`) that falls back
> to the brand color rather than ship unreadable chrome. Process-wide
> (`set_accent`, lock-free), read live by every `Palette::new`, so overlay
> panels and dialogs track an accent change with no plumbing. The face is
> still native Segoe UI and the visual target remains the Windows 11
> settings-card look.

## Privacy guarantee (no record of viewed photos) — tasks.json #2

**The line is content, not footprint** (owner, ADR-018): PhotoBlaze keeps **no
persistent record of which photos were viewed, or any pixel/metadata derived from
them.** The app's own *existence* on disk is fine — the installer's registry writes
(the per-user ProgID, file associations, folder verb the Velopack install hook writes),
a Start-menu shortcut, and read-only config (the future task #8) are all explicitly in-bounds. What's forbidden is a
trace of the *viewing*: no thumbnail DB or pixel cache, no recent-files/MRU of
photo paths, no decoded-pixel temp files, no log of viewed paths. **One deliberate
exception (ADR-022, owner 2026-07-02):** `settings::last_folder` remembers the single
most-recent *folder* (never file names) as the Open dialog's default start on a fresh
launch — it never auto-opens anything (owner call 2026-07-03 reversed the brief
reopen-on-launch behavior); it's written on the explicit open action, and unit tests
can't write it (`AppCore::persist_prefs`).

**Explicit user edits are a separate, allowed category** — not an exception to be
minimized. Deleting a photo, saving a rotation (an EXIF Orientation write back to
the file or a sidecar), and similar metadata updates *do* touch disk, but only ever
on a **user-initiated command** — never as a passive byproduct of viewing. The line
is *involuntary traces of viewing*, not *the user editing their own files*; nothing
is persisted unless the user deliberately invokes the action.

- **Every runtime cache is RAM-only and dropped on exit.** Inventory: the resident
  GPU texture ring + its `pb-core::ResidentRing` mirror, the decode pool's in-flight
  buffers + `pending_uploads`, `meta_cache` (per-photo panel data), per-image
  `rotations`, the `failed` set, the transient `toast`, on-demand EXIF reads
  (`Shift+I`), and the **session archive-password cache** (`AppCore::archive_passwords`)
  — all in memory, never serialized. `pb-core` is pure (no `std::fs`).
  - The password cache (session-archive-password-cache) auto-tries passwords the user has
    successfully used this session on later encrypted archives, so a same-password folder
    asks once. It holds `SecretString`s — **zeroized on drop, redacted `Debug`, never
    `Display`ed or serialized** — so it is not a `Settings` field (`settings.save()` can't
    write it) and threads redacted through `DialogResult`/`CoreEffect`. It is wiped
    (zeroizing) at teardown (`clear_session_state` / the macOS quit intercept), explicitly so
    it holds even if the process `exit()`s without unwinding. Scope note (honest, not
    overclaimed): this protects against *app-level* leaks (a stray `{:?}`, a settings write),
    **not** OS capture of live process RAM (a kernel crash dump, swap, hibernation) — the
    same exposure as the password while the user types it.
- **On-disk I/O is read-only on every view/cache hot path.** The only files
  PhotoBlaze opens *while viewing* are the photos themselves, and only to *read*:
  directory scan (`read_dir`), decode (`fs::read`), and the panel's
  `fs::read`/`fs::metadata`.
- **Writes happen only on an explicit user command, never on the view path.** The
  Edit-menu file operations — delete a photo, save a rotation (write the EXIF
  Orientation), future metadata edits — modify disk, but they are deliberate,
  user-triggered actions on the user's own files: gated behind a command (with
  confirmation for destructive ones), never automatic, never reached by scrolling or
  decoding. Per-image `rotations` stay RAM-only until the user chooses *Save*.
- **Esc teardown writes nothing** (task #6): hide the window, drop the RAM caches
  (`clear_session_state`), exit — no flush-to-disk step exists.
- **Enforced two ways:** a no-trace integration test
  (`viewing_a_folder_writes_nothing_to_disk`) diffs a sandbox before/after a real
  scan+decode+EXIF session and asserts zero files created or modified — it exercises
  only *viewing*, so it still holds once the Edit commands land (a scan/decode never
  triggers them); and a static audit that no `fs::write`/`File::create` sits on the
  *passive view/decode path* (today the only ones in the tree are `#[cfg(test)]` code
  and the `offscreen_png` example). The Edit-menu commands (delete, save-rotation) are
  the one place app code writes — reachable only from an explicit user action. Re-run
  the audit before adding any *passive* disk write; any on-disk scratch must be opt-in
  + cleared.

## Cross-platform discipline (Windows now, Apple Silicon later)

Windows 11 is the target, but the spikes + codex review (see `decisions.md`,
post-spike update) put us on the **portable path**: **wgpu is the v1 renderer**
(DX12 backend on Windows, Metal on macOS), because CPU decode (2.5×) and the
staging-ring upload (3.4×) already clear 120 Hz — wgpu's portability costs nothing
measurable here.
- **`winit` owns windowing + input**; the wgpu surface is created on its window
  handle. Rendering/upload/decode sit behind the `Renderer`, `DecodeBackend`, and
  `UploadStrategy` traits.
- **macOS is a cheap port** (wgpu Metal backend + a hardware-decode/upload backend
  swap), not a rewrite — deferred to v2.
- **GPU decode + zero-copy is a gated acceleration backend** (native D3D12 +
  nvImageCodec), pursued only if the high-MP stress test proves a need (ADR-012
  kill criterion). The CPU pool is the permanent baseline and handles formats GPU
  decode can't.
- Do color management **in-shader** (matrix + TRC) so it ports unchanged.
- Isolate every platform-specific call (refresh-rate query, swapchain latency
  hook, photon timestamp) behind a single helper so the eventual port is a small
  surface.

## Licensing — LGPL discipline (read before touching FFmpeg, libheif, or dist)

Blaze Viewer is **source-available and sold** — **FSL-1.1-ALv2** (`LICENSE.md`), which
converts to Apache-2.0 after two years. (It was a closed proprietary EULA until
2026-07-14; the superseded `LICENSE` file is gone.) That still makes third-party native
deps a hard ship gate, not a chore. The rule, in one line:

> **We only ever DECODE. Ship LGPL; never ship GPL.** GPL-only encoders and
> filters (x264, x265, GPL avfilter) are irrelevant to a viewer — we don't encode
> anything, so we give up nothing by excluding them.

### ⚠️ The Homebrew trap — the #1 false alarm in this repo

**Dev and release link different FFmpeg. This is deliberate.**

| Build | FFmpeg | License | Shippable? |
|---|---|---|---|
| `build-swift-host.sh --ffvideo` (**dev**) | Homebrew `/opt/homebrew/opt/ffmpeg` | **GPL-3.0** (`--enable-gpl`, x264/x265) | ❌ never |
| `build-swift-host.sh --bundle-ffmpeg` (**release**) | pinned source → `third_party/ffmpeg/<arch>` | **LGPL-2.1+** | ✅ yes |

So: **`otool -L` on `target/swift-host/**` showing Homebrew GPL dylibs is not a
bug.** It is the dev path working as designed. Before concluding the *product*
has a licensing problem, check the artifact that actually ships:

```sh
# Inspect the SHIPPED app, not the dev build:
hdiutil attach dist/BlazeViewer-*.dmg -nobrowse -readonly -mountpoint /tmp/pb
otool -L "/tmp/pb/Blaze Viewer.app/Contents/MacOS/Blaze Viewer" | grep -i homebrew   # must be EMPTY
ls  "/tmp/pb/Blaze Viewer.app/Contents/Frameworks/"                                # bundled LGPL dylibs live here
hdiutil detach /tmp/pb
```

A release .app must reference FFmpeg **only** via `@rpath/…` out of
`Contents/Frameworks`. Zero absolute `/opt/homebrew` paths. If you see one in a
`dist/` artifact, *that* is a real bug — the dev path leaked into a release.

### How each platform stays clean

- **macOS** — `scripts/build-ffmpeg-macos.sh` builds a pinned FFmpeg 8.1.1 from
  source: no `--enable-gpl`, no `--enable-nonfree`, `--disable-encoders
  --disable-muxers`, `--install-name-dir=@rpath`. It **asserts** LGPL at the end
  (belt-and-suspenders against a flag leaking in). `bundle-ffmpeg-macos.sh` copies
  the dylibs into `Contents/Frameworks`; `release-macos.sh` re-signs them with our
  Developer ID. HEIC on macOS uses **Apple Image I/O** — libheif is deliberately
  *not* linked here (see `crates/pb-decode/build.rs`), so macOS has no libheif
  exposure at all.
- **Linux** — system/AppImage-bundled shared libheif + FFmpeg via `linuxdeploy`;
  dynamic linkage, so LGPL §4 is satisfied by construction.
- **Windows** — covers **three** LGPL libraries as of task #100: libheif + libde265
  (LGPL-3.0, §4) and **FFmpeg** (LGPL-2.1, §6 — demux/metadata only, but LGPL attaches to
  *linkage*, not to how much of the library you call). **`pb-decode/build.rs` now links all
  three as DLLs** (the `x64-windows` / `arm64-windows` vcpkg triplets, *not* `-static-md`),
  which is what satisfies the relink obligation — attribution alone never would. Measured
  cost of the switch: **+0.8 MB**. `PB_VCPKG_STATIC=1` forces static linkage but is an A/B
  measurement escape hatch **only — never ship it**. (`dav1d` is BSD-2-Clause — static
  would be fine there, attribution only.)
  > Task #77 is still open at the time of writing; the *linkage* half is done. Check it
  > before shipping a paid Windows build, and trust `pb-decode/build.rs` over this
  > paragraph — it was stale once already.

### Distribution model

Direct sale + **Sparkle** auto-update. This sidesteps the App Store's long-running
incompatibility with LGPL relink requirements. If an App Store channel is ever
added, the LGPL bundling question must be re-opened *first* — it is not a
packaging detail.

### Non-code assets

**Font Awesome Pro** is licensed to the owner and **not redistributable** — see
*HUD / toast icons* above. Any FA Pro glyph baked into a shipped artifact is a
compliance defect regardless of platform.

### Where the truth lives (in precedence order)

1. `scripts/build-ffmpeg-macos.sh` — the authoritative configure flags + the *why*
2. `THIRD-PARTY-NOTICES.md` — the shipped compliance manifest
3. `crates/pb-decode/build.rs` — per-target linkage (static vs shared, which cfgs)

If those three disagree with this section, **they win** — and fix this section.

## Current library picks

These are the starting points from the research in `.taskmaster/docs/`. Each is
**provisional and benchmark-justified** — the A/B seams exist precisely so we can
replace any of them with data.

| Concern | Primary | A/B alternative / notes |
|---|---|---|
| JPEG | `turbojpeg` (libjpeg-turbo, **native scaled decode**) | `zune-jpeg` (pure Rust, SIMD; pair with `fast_image_resize`) |
| PNG/APNG | `png` (image-rs) + `zlib-rs` backend (pure Rust, fastest now) | — (no scaled decode exists for PNG) |
| WebP | `libwebp-sys` (`use_scaling` = true downscale-on-decode) | `image-webp` (pure Rust, no SIMD/scaling) |
| AVIF | stills: Windows WIC / macOS ImageIO; **animated (`avis`): vcpkg dav1d + own demuxer** (task #76) | Linux: FFmpeg (`livephoto`) for animated |
| HEIC | `libheif-rs` | ⚠ **highest** Windows build risk — pin vcpkg ports or ship DLLs |
| JXL | `jxl-oxide` (pure Rust) | `jpegxl-rs` only if native DC downscale needed |
| TIFF / BMP / QOI | `tiff` / `image` / `qoi` (all pure Rust) | — |
| SVG | `resvg`/`usvg` → `tiny-skia` pixmap → texture | rasterize at on-screen res; watch `vello_hybrid` for live-zoom |
| RAW | `kamadak-exif` → extract embedded JPEG preview → JPEG path | full demosaic deferred (100×+ cost); `rawler` optional (LGPL) |
| Color | `moxcms` (pure Rust; 3×3 matrix + TRC in-shader) | `lcms2` behind a flag for exotic CLUT/CMYK profiles |
| Windowing | `winit` (window + input; `refresh_rate_millihertz` for the advance cap) | portable; the wgpu surface is created on its window handle |
| GPU API | **wgpu** (DX12 backend on Windows, Metal on macOS), present **Mailbox** | native D3D12 retained as a gated acceleration backend behind `Renderer` |
| GPU decode | **CPU decode pool** (`zune-jpeg`; `turbojpeg` as A/B) — measured 2.5× @ 120 Hz | nvImageCodec/CUDA zero-copy is a gated escalation (ADR-012 kill criterion); benchmark 5090 HW-JPEG first |

### Decode-to-fit value ranking
JPEG ≫ WebP > JXL(C) ≫ everything else. Prioritize the scaled-decode path where
it pays.

### What's actually wired (2026-06-27) — deviations from the provisional table
Multi-codec dispatch is implemented in `pb-decode` behind the `ImageDecoder` seam
(`decode_bytes` sniff-registry + extension routing for the ambiguous ones). The
pragmatic crate choices differ from the table above and are the current baseline:
- **`image` crate** (one dep, `default-features` curated) covers PNG/GIF/BMP/TIFF/
  **WebP**/TGA/QOI/ICO/PNM/HDR/EXR — chosen over the per-format crates (libwebp-sys,
  png+zlib-rs, …) for zero build risk; swap individual formats back behind the seam if
  benchmarks justify it.
- **JXL** `jxl-oxide`; **SVG** `resvg/usvg/tiny-skia` (rasterized at display res).
- **RAW** (ARW/NEF/CR2/DNG/…, `raw.rs`): hybrid. If the embedded preview is full-size
  (long edge ≥ `PREVIEW_FULL_MIN`=4000, e.g. Nikon's ≈7360) use it (fast); else **demosaic
  to true sensor res** via `rawloader` + `imagepipe` (e.g. Sony's 1616 thumbnail → 6048).
  Demosaic runs on a **256 MB-stack thread** — some RAW decoders recurse deep enough to
  overflow the default stack (a Nikon D800 NEF does; it's why the preview path exists for
  full-preview cameras). Slower than preview-only; "preview-first then refine" is the
  future optimization. JPEG-segment-parse finds embedded previews (`jpeg_spans`).
- **AVIF + HEIC stills**: **Windows WIC** (`wic.rs`, `cfg(windows)`, `windows` crate) using the
  OS codec extensions — first platform-specific decode backend (macOS mirrors with ImageIO).
  Needs the AV1/HEVC/HEIF Store extensions; absent → graceful decode error. **JPEG-2000 not
  added** (no pure-Rust decoder; OpenJPEG/C, rare).
- **Animated AVIF (`avis`) on Windows** (task #76): the vcpkg static **dav1d** behind
  `--features dav1d` (ship config, like libheif; same pinned tree via `setup-libheif.ps1`).
  WIC exposes only frame 0 and MF can't demux `.avif`, so `pb-decode/src/avis.rs` demuxes the
  sample tables itself (pure Rust, fuzzed) and feeds dav1d through a **C accessor shim**
  (`csrc/dav1d_shim.c`, compiled by build.rs against the pinned headers — dav1d structs never
  cross the FFI by hand). `probe_avis` is the shared detect/decode decision: avis-only (msf1 =
  HEVC stays still), HDR → the fp16 WIC still path, fragmented/encrypted/stz2 → still; no dead
  play hints. YUV→RGB in `yuv.rs` (identity/601/709/2020, limited+full, 8/10/12-bit); the trak
  `colr` rides as the display `ColorTransform`. Cancellable via `decode_animation_cancellable`
  (the whole animation path now checks the flag). Loops like a GIF; corpus 26-frame 1280×531 ≈
  139 ms release decode.
- **Orientation** (subtle, was buggy): `common::read_orientation` scans **all** EXIF IFDs
  (HEIC/RAW put Orientation outside the primary IFD; a PRIMARY-only lookup wrongly read 1).
  WIC **already applies** the container rotation, so the WIC path passes orientation=1 (re-
  applying double-rotated). imagepipe also self-orients (demosaic path → 1). The RAW
  *preview* path applies the container's orientation to the sensor-order preview.
- Every backend returns full-res RGBA8 → shared `common::finalize`/`finalize_oriented`
  (orientation + Lanczos decode-to-fit). Native scaled-decode (JPEG DCT, WebP) still a TODO.
- **Panic-safety**: `decode_bytes`/`decode_image_file` wrap the decoders in `catch_panics`
  (`catch_unwind`) — a third-party decoder panicking on a hostile file becomes a
  `DecodeError`, not an app crash. **Release profile is `panic = "unwind"`** (changed from
  abort) so this works; the GPU present hot path never panics, so unwind tables don't cost
  it. A hard *stack overflow* (some NEFs) is still uncatchable — mitigated by the demosaic
  big-stack thread, not `catch_unwind`.
- **TGA is routed by extension** (`.tga`), not content — Targa has no magic number, so
  `image::guess_format` can't sniff it; `decode_image_file` hands it an explicit format hint.
- **Color management + wide-gamut + HDR output are wired** (tasks.json #11). Three layers:
  1. **In-shader ICC CMS.** `pb_decode::color` parses the source profile (`moxcms`) to a
     3×3 (source primaries → BT.709) + a 7-param TRC, carried on `DecodedImage::color`.
     Read per backend: JPEG APP2 (zune `icc_profile`), PNG/TIFF/WebP (`image` `icc_profile`),
     JXL `rendered_icc`, and — because the Windows HEIF decoder returns **no** WIC color
     contexts — the ISOBMFF `colr` box parsed directly (`prof`/`rICC` ICC or `nclx` CICP) in
     `wic.rs`. sRGB / ~2.2-gamma-sRGB-primaries → passthrough. Fixes oversaturated P3 HEICs.
  2. **fp16 scRGB render path.** `pb-render` renders to an `Rgba16Float` scRGB-linear
     intermediate (no gamut clamp), then a present pass → the surface: SDR 8-bit gets an
     extended-Reinhard tone-map (per-image `peak`) + sRGB-encode; HDR fp16 copies straight.
  3. **Wide-gamut + HDR output (pure wgpu, no native D3D12).** A DXGI **fp16 flip swapchain
     is always scRGB**, so `pb_render::display::primary_hdr()` (DXGI `GetDesc1`) detects an
     HDR desktop and configures an `Rgba16Float` surface. HDR AVIF/HEIC (PQ/HLG) decode to
     fp16 scene-linear via WIC `128bppRGBAFloat` (`PixelFormat::Rgba16F`); brightness is baked
     in the scene pass (SDR×SDR-white-scale, HDR×1.0 absolute). P3 shows wider and HDR gets
     real headroom on a capable panel.
- **Video playback (tier 2, task #79 — Windows shipped; Linux/macOS = parity work).**
  Filesystem videos (one cross-platform container recognition list; per-file playability is
  a runtime property of the OS codecs) are **typed items** — `LibraryItemKind`, dispatched
  in `decode_item` *before* any `bytes()` read; path-only, never indexed inside archives;
  Live-Photo companion `.mov`s are hidden by same-stem-per-directory dedup at scan.
  Poster = the clip's first non-black frame via an MF reader configured identically to
  playback (rotation/color parity by construction); panel facts come from a ~20 ms
  header-only probe. Playback = a forward-only `VideoSession` (pb-app-core, injected-time,
  fully unit-tested on fake producers) fed by a demand-driven MF reader thread (pb-decode):
  one credit = one frame over a **single merged command channel** (Stop/SeekTo can never be
  deafened by backpressure), byte/frame-budgeted queue (constant memory at any clip
  length), rebuffer-don't-drift, audio = a shell WinRT `MediaPlayer` (video tracks
  deselected — audio only) whose position is the master clock via ~4 Hz
  `AudioClockSample`s. Seek recreates the reader positioned at the target (repositioning a
  warm HEVC reader blocks ~1 s — spike-measured); the producer parks after EOS so `P`
  replays via seek-to-0. 4K30 software decode is comfortable; 4K60 is borderline — NV12 +
  in-shader YUV or hardware decode is the reserved escalation (ADR-012).
  **macOS routing (2026-07-16, macos-video-smoothness plan):** MP4/MOV → `AVPlayer`;
  **everything else (MKV/WebM included) → the Session route** (FFmpeg → wgpu → Metal). The
  `AVSampleBufferDisplayLayer` "sample-buffer" presenter is **parked, opt-in only**
  (`PB_SAMPLE_BUFFER=1`) — it dropped ~3 frames/sec that both other routes play flawlessly;
  it is kept as the on-device Dolby-Vision **reference renderer** (DoVi itself is deferred;
  detection ships — a DoVi Profile-5 file on the Session route toasts an honest
  colors-can't-be-shown warning and the Details panel names every DoVi profile).
  `PB_TRACE=1` prints a per-2s Session dropped-frames diag (the `sb-play diag` analog).
- **Archives are "doors" in the deck** (tasks.json #104, 2026-07-16): an archive **on disk** is a
  typed item — `LibraryItemKind::Archive(ArchiveKind)`, the third arm beside `Image`/`Video` — so
  a folder's `.zip`/`.7z`/`.cbz`/… are *visible while browsing* instead of reachable only via Open
  File. It decodes to the owner's **folder artwork** (`pb-app-core/assets/folder-zip-*.webp`,
  `cfg(windows)` = manila / else blue, matching each OS's own folder colour; decoded + composited
  onto an opaque backdrop **once** per process), and **`P` enters it** (routed through
  `open_plan(Source::Archive)`, so it is the same operation as the picker's open — password
  prompt, RAM pre-flight and progress dialog all included). `Alt+Up` climbs back out to the
  folder (`open_parent_cmd` anchors on `source.container()`), which is how you reach the next
  archive. Doors also make a folder of *only* archives openable: it now yields scan items, where
  before it hit the keep-deck rule and reported no images.
  - **The affordance is the play-hint pill, not the tile** — `play_hint_kind` = `3` +
    `play_hint_persistent` (the pill reads *Open*, and unlike an animation's flash the shells
    hold it open, because a door's picture alone never says "press P"). The tile briefly carried
    a Font Awesome glyph instead: an icon drawn for 16 px stretched to the height of a 7680-wide
    display. Don't go back.
  - **The guarantee:** `decode_item_cancellable` returns the tile **above** the `source.bytes()`
    request, so browsing past a door *never* decompresses. That — **not** the texture size — is
    why doors are safe where blending archive contents into the deck was not (the prefetch ring
    would have decompressed archives nobody clicked). Pinned by a panicking-`bytes()` source
    driven through **every** decode entry point. ⚠️ The tile's *size* has been argued from three
    times and been wrong three times; it only has to clear a comfort bar
    (`a_full_ring_of_doors_fits_the_byte_budget`).
  - ⚠️ **A new `LibraryItemKind` must opt *out* of byte reads, not into them.** The tree encodes a
    two-kind world (video vs "everything else, therefore an image, therefore safe to read"). Guards
    written `!matches!(…, Video(_))` or `if let Video(_)` silently drop a new kind in the *image*
    bucket — which is how the thumbs strip and the `Shift+I` panel would each have `fs::read` every
    archive in a folder. Read guards are **positive** (`Image` reads bytes) and kind matches are
    **exhaustive** so the compiler lists the sites; note it only lists them *per platform* —
    `macos_native_route`/`macos_sample_buffer_route` are `cfg(macos)` and invisible on Windows.
  - Doors are typed off the item's **path**, not its name (unlike video, deliberately): an archive
    entry has no path, so a `.zip` inside a `.zip` is unrepresentable rather than merely refused,
    and `archive_kind` gets `.tar.gz` right where a name-based split sees `gz`.
- **Archive viewing (ZIP + 7z + the tar family + RAR5 + RAR4)** (tasks.json #30, #102, #103) is
  wired in the `pb-source` crate behind the `ItemSource` seam, decoded via
  `pb_decode::decode_named_bytes` (bytes + extension hint). One classifier —
  `pb_source::archive_kind` — answers every "is this an archive, and which kind?"
  question (shell `is_archive` predicates, `scan::open_archive` dispatch, the `LibraryItemKind`
  door arm, double extensions like `.tar.gz`, `.cbr`/`.cbz` comics). Two access models: **lazy** (ZIP via handle pool; plain
  `.tar` via a seek-over-data header index) and **eager decode-to-RAM** (7z; `.tar.gz` /
  `.tar.bz2` / `.tar.zst` / `.tar.xz` — solid streams have no cheap random access). 7z
  pre-flights its RAM budget from the header; a compressed tar has no size table, so its
  budget is enforced **mid-stream** (`OpenError::TooLarge`, still refuse-before-reserve).
  Every kind but ZIP opens **off-thread** (`ArchiveKind::background_open`) through the
  one worker entry `scan::load_archive`, with determinate progress + Cancel. Tar opens
  are hardened against hostile bytes (metered PAX/GNU metadata quota, entry/name-table
  caps, expanded-work cap, zstd window pre-check + frame-checksum verify, xz dict-size
  pre-check; `fuzz/` has `tar_open` + `rar_open` targets) — see the #102 plan rev2.
  **RAR5 + RAR4** (#103): two of our own container parsers (`pb-source/src/rar.rs` =
  RAR5, `pb-source/src/rar4.rs` = RAR4 — completely different container shapes) over the
  `compcol` codecs (`rar5` + `rar3`; exact-pinned fork rev on the `rar3-standard-filters`
  branch until it merges + releases on crates.io). Both share one `RarSource`/`ItemSource`
  via an `EntryData::Lazy { codec: RarCodec::{Rar5,Rar3} }` tag: non-solid lazy / solid
  eager, header-CRC + entry-CRC32 verified, corpus-validated byte-identical to `unrar` (44
  archives / 218 entries in the differential). RAR5 Delta/x86 filters decode (compcol's
  `add_file_boundary` makes position-dependent filters file-relative in a solid group);
  RAR4 LZ/PPMd + Delta/x86/audio filters + solid all decode. Multi-volume, encrypted RAR4
  headers (`-hp`), and unsupported encryption versions refuse with honest messages
  (`ArchiveOpenError::Unsupported`); a codec-refused member degrades per-entry (its
  solid-group tail goes unavailable), the rest of the archive serves. **Encryption is
  supported for RAR5** (`pb-source/src/rar_crypt.rs`): per-file (`-p`) and full-header
  (`-hp`) RAR5 use standard PBKDF2-HMAC-SHA256 + AES-256-CBC (the tractable scheme, unlike
  RAR4's bespoke SHA-1 KDF — RAR4 `-p` refuses per-entry, `-hp` refuses at open), so a
  missing/wrong password returns `PasswordRequired` (prompts, like ZIP/7z) and a correct
  one decrypts — validated byte-identical to `unrar` over the corpus and a committed
  encrypted-solid fixture. RAR5 encrypted solid runs are padded to 16 bytes *between* files,
  so each run is decrypted then stripped to its real block length (`rar5_stream_len`) before
  the LZ decoder, which reads block framing eagerly and would choke on the padding. RAM-only
  — never extracted to disk, so the no-trace guarantee holds
  (`viewing_a_{zip,7z,tar,tar_gz,rar}_writes_nothing_to_disk`, the RAR one covering a
  decrypt). Errors surface in the egui `Message` dialog. Passwords: ZIP/7z/RAR5 all prompt
  in-app; RAR4 and the tar family have no in-app decryption. Crates: `zip` + `sevenz-rust2` +
  `tar`/`flate2`/`bzip2`/`ruzstd`/`lzma-rust2` + `compcol` + `aes`/`cbc`/`hmac`/`sha2`
  (all pure Rust, no C build risk).
- **Known v1 limitations** (deliberate): Radiance-HDR / OpenEXR (image-crate, not WIC) still
  clamped to SDR; CMYK JPEG mis-colored; first frame only (GIF/animated-WebP/Live-Photo/
  multipage-TIFF). LUT/CLUT & gray/CMYK ICC profiles → sRGB passthrough (the `lcms2`-behind-a-
  flag escalation). SDR-white level is a 200-nit default (real value via DisplayConfig = TODO).
  ⚠ On an **HDR desktop**, GDI screen capture of the flip swapchain returns all-white — a
  Windows limit, not a render bug (use the `offscreen_png` example to verify rendering).

## Build, test, bench

> Rust toolchain required (`rustup`); `rust-toolchain.toml` pins stable + the
> components for coverage.

```sh
cargo test                 # unit + property + golden tests
cargo llvm-cov --workspace # coverage (target >80%)
cargo clippy --all-targets -- -D warnings
cargo fmt --all
cargo bench                # criterion microbenchmarks over the corpus
```

**Running the viewer on Windows — use the build script, not a bare `cargo run`.**
`pwsh scripts/build-windows.ps1 -Run` builds with the **ship feature set**
(`libheif,dav1d,ffprobe`) and enters the VS Developer shell FFmpeg's bindgen needs.
A plain `cargo run -p pb-app` omits `ffprobe`, so **FFmpeg isn't linked and every film
with AC-3 / E-AC-3 / DTS audio plays SILENT** (Media Foundation can't decode them —
`0xC00D36B4`; it is the #1 "no sound" trap). `-NoFfmpeg` drops FFmpeg for a quick
build (films silent, no Developer shell); `-NoNative` skips every native lib. On
macOS/Linux a bare `cargo run -p pb-app --features …` is fine — this is a Windows
build-config footgun specifically.

## Working norms

- **Test first.** New `pb-core` logic without a failing-then-passing test is not
  done.
- **No per-frame heap allocations on the hot path.** Pre-allocate pools; reuse.
- **Never block the event loop.** Decode and I/O are always off-thread.
- **Never decode or upload on the keypress frame.** If you're tempted to, the
  prefetch window is wrong — fix that instead.
- **Quarantine platform-specific code** behind the established helper seams.
- When a decision touches the hot path and the answer isn't obvious, **put it
  behind a seam and benchmark both** rather than arguing.
- **Update `CHANGELOG.md`.** When you land a user-facing bug fix or feature, add a
  line under `## [Unreleased]` (Keep a Changelog format — group under `Added` /
  `Changed` / `Fixed` / `Removed`; write for users, not commits). Skip purely
  internal churn (refactors, test-only changes, CI/workflow tweaks, formatting).
  On release, the `[Unreleased]` block moves under the new version's heading + date
  and a fresh empty `[Unreleased]` is left at the top.

## Cutting a release

**Windows** ships **Velopack** (per-user installer + auto-update), built + signed **locally**
(GitHub Actions credits are finite): `pwsh scripts/release-windows.ps1 -Upload` builds with
libheif, signs the exe + `Setup.exe` + `Update.exe` via Azure Trusted Signing, `vpk pack`s a full
release, and rsyncs the flat feed to `downloads.blazeviewer.app/win`. The app reads that
feed over HTTP (`update.rs` `FEED_URL`) and self-updates — downloads in the background, installs on
quit. Version comes from `crates/pb-app/Cargo.toml`, so it always matches the app; there is **no
tag / GitHub Release for Windows**.

*Architecture:* the script defaults to the host arch and takes `-Arch x64|arm64`. **x64** ships as the
historical `win` Velopack channel; **ARM64** as `win-arm64` — both land in the same flat feed dir, and
an install only ever auto-updates within its own channel (Velopack tracks the channel the app was
installed from, so `update.rs` needs no arch logic and the two never cross). Each arch is built on its
own **native** box (no cross toolchain wired up), after building that arch's native decode libs
(libheif, dav1d, **and FFmpeg** — tasks #76 / #100) once with `scripts/setup-libheif.ps1 -Triplet
<arch>-windows` — the **DLL** triplet, *not* `-static-md` (task #77: LGPL relink; a static build
cannot ship, and `release-windows.ps1` throws if it can't find `installed\<triplet>\bin\heif.dll`).
The script pins the vcpkg tree to a recorded commit (`-VcpkgRef`) and installs all three ports;
`pb-decode/build.rs` picks the vcpkg triplet from the target arch. The ship
feature set is `--features libheif,dav1d,ffprobe`. ARM64 uses the `vcredist143-arm64` redist framework.

> **`ffprobe` needs a VS Developer shell — it's the first feature that does.** FFmpeg's
> `bindgen` runs its own clang, which reads `INCLUDE` to find `stdint.h`; a plain `cargo build`
> never needed that, because rustc finds the MSVC linker itself. `scripts/vs-dev-env.ps1` handles
> it (release script + both CI lanes call it; it no-ops if you're already in a dev shell), and VS
> already ships the required libclang at `VC\Tools\Llvm\{x64,ARM64}\bin` — nothing extra to install.
> It also needs `VCPKG_ROOT` **exported**: the `vcpkg` crate `ffmpeg-sys-next` uses has no `~/vcpkg`
> fallback, unlike our own build.rs. FFmpeg here is **demux/metadata only** (MF still decodes
> everything) — the setup script patches the port to a trimmed build, which is the difference
> between **+3.06 MB** and +16.42 MB on the exe.

**macOS** is **built locally on the owner's Mac** via `scripts/release-macos.sh` (Developer ID +
notarization), then published to `downloads.blazeviewer.app/mac` with
`scripts/release-mac-upload.sh` — which scp's the DMG + appcast **straight from the Mac** to
jdlien.com and repoints the `BlazeViewer-latest.dmg` symlink (the remote `mac/` dir is
jdlien-owned, no sudo). No Windows detour: `scripts/release-mac-upload.ps1` is the equivalent for
running the upload from the Windows box, but the whole Mac release now stays on the Mac. Hosted
GitHub Actions is too expensive to use, so `.github/workflows/release.yml` — which builds the DMG
on a hosted `macos-15` runner — is **`workflow_dispatch`-only (dormant)**; a `v*` tag no longer
auto-triggers it. A GitHub Release for the DMG, if wanted, is created manually. Signing setup is
in `.taskmaster/docs/release-signing.md`.

macOS **auto-updates via Sparkle** (task #65) — the in-app equivalent of Windows' Velopack. The
`.app` embeds `Sparkle.framework` (assembled by `build-swift-host.sh`, since a SwiftPM executable
has no Xcode "Embed Frameworks" phase) and reads an EdDSA-signed `appcast.xml` next to the DMG
(`SUFeedURL` in `Info-swift-host.plist`). `release-macos.sh` re-signs Sparkle's nested helpers with
the Developer ID (inside-out, before the app) and, after notarizing, EdDSA-signs the DMG and writes
`dist/appcast.xml` (`scripts/generate-mac-appcast.sh`); `release-mac-upload.ps1` publishes that
appcast alongside the DMG. The **private EdDSA signing key lives only in the release Mac's login
keychain** (generated once via Sparkle's `generate_keys`; the public `SUPublicEDKey` is committed in
the plist) — **back it up** (`generate_keys -x`); losing it means no future build can be signed for
auto-update without shipping a new public key via a stopgap manual update.

**Linux** ships a self-contained **AppImage** — one executable the user downloads, `chmod +x`es, and
runs; **no `apt install`, no dependency hunt.** Built locally with `scripts/release-linux.sh` (→
`dist/BlazeViewer-<version>-<arch>.AppImage`). It builds the full-feature release binary
(`--features livephoto,pb-decode/libheif`) and uses **linuxdeploy** (fetched to `dist/appimage-tools`)
to bundle the *specialized* decode libraries — libheif, FFmpeg, and the AV1/HEVC codecs — while
leaving the ~universal system stack (glibc, GTK, Mesa/GL, X11, Wayland) to the host, per the AppImage
excludelist. Two things linuxdeploy/`ldd` can't see are handled by the script: **libheif's dlopen'd
plugins** (`libheif-libde265.so` etc.) are copied into `usr/lib/libheif/plugins` with their own deps
(libde265/libaom/…), and a **custom `AppRun`** exports `LIBHEIF_PLUGIN_PATH` + `LD_LIBRARY_PATH` so
they resolve inside the bundle. Live Photo *audio* still needs `pw-cat` (PipeWire) on the user's PATH
— present on any modern desktop, degrades to silent motion if absent, so it's intentionally **not**
bundled. **Unsigned** (no Developer-ID/GPG equivalent yet), but it **does self-update** now (below).
`dist/` is git-ignored, so the artifacts never get committed.

`release-linux.sh` builds for the **host arch**, so from a Mac/Windows box (no Linux VM needed) use
**`scripts/release-linux-docker.sh [amd64|arm64|both] [--upload]`** — it builds an **Ubuntu 26.04**
container (`scripts/appimage.Dockerfile`, matching the FFmpeg 8 / libheif 1.21 the code targets) and
runs `release-linux.sh` inside it. `both` builds x86_64 then aarch64; `--upload` publishes the
result afterwards (see below). On **Apple Silicon + OrbStack** `linux/amd64` runs under **Rosetta**,
so the **x86_64** artifact (what most Linux users need) builds at near-native speed; `arm64` is
native. It uses a container-only `CARGO_TARGET_DIR` (a cached volume) so it never clashes with the
host's macOS `target/`, and `APPIMAGE_EXTRACT_AND_RUN=1` so no FUSE/`--privileged` is required. The
build distro sets the glibc floor (2.43 here → recent-distro runtime); dropping it means building
FFmpeg/libheif from source on an older base. **AppImages can only be built on Linux** (the container
*is* that Linux) — there's no native macOS/Windows AppImage build. ⚠ The container build image
pre-installs the Rust toolchain via `rustup-init`, whose `--component` takes a **comma-separated**
list (`rustfmt,clippy,…`) — a space-separated list makes it reject the second component.

**Publishing + auto-update (Linux) — the JSON-feed self-replace model** (the Velopack/Sparkle analog
for AppImages). `scripts/release-linux-upload.sh` (or `release-linux-docker.sh … --upload`) scp's the
versioned AppImage(s) + a `.sha256` sidecar each to `downloads.blazeviewer.app/linux`, writes a
shared `latest.json` manifest (version + per-arch url/sha256/size), and repoints the
`BlazeViewer-latest-<arch>.AppImage` symlinks; Caddy redirects `/latest/linux` (x86_64) and
`/latest/linux-arm64` (aarch64) at them. The app's `update.rs` `linux` module reads
`latest.json` in a background thread, and if it advertises a newer build for this arch it downloads
the AppImage, **verifies the sha256**, and swaps `$APPIMAGE` in place on quit (atomic rename — the
next launch is the new version). Self-gates when `$APPIMAGE` is unset (a `cargo run` / extracted
binary) or the AppImage's directory isn't writable (installed read-only) — then it just stays put.
`PB_UPDATE_FEED` overrides the feed base URL for offline testing.

> **Release only from a clean, committed workspace — now enforced, not remembered.**
> `crates/pb-app/build.rs` stamps the build id `-dirty` on **any** `git status --porcelain`
> output — **untracked files included** — and that ships in the About dialog. Every release
> script refuses to run from a dirty tree; `scripts/release-preflight.sh` is the shared bash
> gate (release-windows.ps1 mirrors it inline — PowerShell can't source bash).
>
> ⚠️ **Why the gate is two-sided — a pre-flight `git status` is NOT enough.** Bumping
> `crates/pb-app/Cargo.toml` changes `pb-app`'s entry in `Cargo.lock`, but *nothing rewrites
> the lockfile until a cargo command runs* — which is the release build itself. So a tree that
> is genuinely clean when checked goes dirty **mid-build**, and the DMG ships
> `0.2.1 (abc1234-dirty)` having been verified clean minutes earlier (hit on 0.2.1,
> 2026-07-14). Hence:
> 1. **`release_preflight`** runs `cargo metadata` *first* to settle the lockfile, *then*
>    checks — turning that mid-build rewrite into an up-front, actionable failure.
> 2. **`assert_build_id_clean` / `assert_tree_clean_after_build`** run *after* the build and
>    assert what was actually stamped, so causes we haven't thought of still get caught.
>    Placed **before** codesign/notarize, so a doomed build never costs an Apple round-trip.
>
> Both honour an escape hatch — `--allow-dirty` (mac), `PB_ALLOW_DIRTY=1` (linux),
> `-AllowDirty` (windows) — for a deliberate throwaway build. **Never for a real release:** it
> only downgrades the abort to a warning; the artifact is still stamped `-dirty`.

> **Never let a tool auto-invoke a paid CI run.** Hosted runners cost real money (a macOS run is
> billed at 10×), so releases are scripted and run locally — the `v*`-tag trigger was removed from
> release.yml precisely so a tag push (or `gh release create`) can't quietly start a hosted build.
> Don't re-add an automatic hosted trigger; script the build instead.

To cut a release:

1. **Roll the `CHANGELOG.md`.** Move `## [Unreleased]` into `## [<version>] - <YYYY-MM-DD>`,
   leave a fresh empty `[Unreleased]`, and update the compare links at the bottom. The crate
   version (`crates/pb-app/Cargo.toml`) must match the tag's numeric core (a `-beta.N` suffix
   lives only on the tag). **Write a `### Highlights` block** (a ~7-line, plain-English "what's
   new" — regular users, not contributors) as the first subsection of the version, above
   `### Added` — the macOS **Sparkle** update dialog shows *only* that block
   (`generate-mac-appcast.sh` extracts it; it falls back to the whole section if absent), while the
   full `Added/Changed/Fixed` detail stays in the file for the curious and the GitHub release body.
2. **Windows:** `pwsh scripts/release-windows.ps1 -Upload` from this machine (with `.env.release`
   signing creds). **Run it from native PowerShell, not the Bash tool / Git Bash** — the ssh
   config's YubiKey `Match exec` hook has a Windows path that Git Bash mangles, so the upload fails
   `Permission denied (publickey)`. The build + sign + pack still succeed there; only the `-Upload`
   scp/rsync needs native PowerShell.
   > ⚠️ **A re-run is NOT upload-only.** `-Upload` is the last step of the *whole* pipeline, and
   > `vpk pack` **hard-fails** on a second run — *"There is a release in channel win which is equal
   > or greater to the current version"* — because the version it just packed is sitting in
   > `dist\feed`. So if the pack succeeded and only the upload failed, do **not** re-run the script:
   > `scp` the already-signed feed yourself (`cd dist\feed; scp * jdlien.com:/var/www/downloads.blazeviewer.app/win/`).
   > Re-running means clearing `dist\feed` first, which re-signs everything for no gain (hit on 0.2.1).

   > ⚠️ **`dist\feed` is a *cumulative* feed, and `vpk` merges whatever it finds there — including a
   > different product.** On 0.2.1 the dir still held the PhotoBlaze packages, so `releases.win.json`
   > advertised PhotoBlaze 0.1.0/0.1.1/0.2.0 *beside* BlazeViewer 0.2.1 and vpk built a delta **across
   > the packId rename** (PhotoBlaze 0.2.0 → BlazeViewer 0.2.1). Upload sends the whole directory, so
   > that would have published the old product to the new feed. `vpk` keys deltas on channel+version,
   > **not packId**. Check `dist\feed` holds only this product before packing.

   Prune superseded packages on the server periodically.
3. **macOS (all on your Mac):** `./scripts/release-macos.sh --release` builds the signed +
   notarized DMG **and** EdDSA-signs it into `dist/appcast.xml` (Sparkle auto-update, task #65),
   then `./scripts/release-mac-upload.sh` scp's the DMG **and the appcast** to jdlien.com and
   repoints `BlazeViewer-latest.dmg` — no Windows box needed. (Optionally verify the seed's updater
   first with `./scripts/test-sparkle-update.sh dist/BlazeViewer-<version>.dmg`.) A GitHub Release is
   **optional and manual** — nothing auto-builds from a tag:
   `gh release create v<version> dist/BlazeViewer-<version>.dmg* --notes-file <(bash
   scripts/changelog-section.sh <version>)`. Write **real, curated, user-facing** CHANGELOG notes
   before tagging so `changelog-section.sh` has a body. **Never** enable `generate_release_notes`.
4. **Linux (from your Mac via OrbStack):** `./scripts/release-linux-docker.sh both --upload` builds
   both AppImages and publishes them + `latest.json` to the feed (repointing the `latest-<arch>`
   symlinks). Needs your ssh keys for the scp step (it runs host-side, after the container work). A
   launched older AppImage then self-updates on next quit.
5. **Tag for posterity** (optional): `git tag -a v<version> -m "…" && git push origin v<version>`.
   Windows never needs it (Velopack reads the version from `Cargo.toml`); it's a record + the anchor
   for a manual macOS GitHub Release. Safe to push now that release.yml doesn't auto-build on tags.
6. **Verify:** the Windows feed serves the new `releases.win.json` + `.nupkg` (and a launched build
   self-updates); the macOS DMG is genuinely notarized — `xcrun stapler validate <dmg>` and
   `spctl -a -t open --context context:primary-signature -vv <dmg>` → `source=Notarized Developer
   ID`; the macOS feed serves the new `appcast.xml` (curl it) and a launched older build detects →
   downloads → installs-on-quit the update; the Linux feed serves the new `latest.json` +
   `latest-<arch>` symlinks (curl `…/latest/linux`). A `-` in a tag marks a pre-release; a
   clean `vX.Y.Z` is a full release.


## Project Task Tracking

This project uses [taskmaster](https://github.com/eyaltoledano/claude-task-master) conventions for tracking tasks, with a `.taskmaster/tasks/tasks.json` using the following structure:

### Directory Structure

```
.taskmaster/
├── tasks/
│   └── tasks.json    # Active tasks
├── docs/             # Documentation or notes
└── archive.json      # Completed tasks (optional)
```

### Schema

**Important:** Task and subtask IDs must be numbers, not strings. Task Studio cannot look up individual tasks if IDs are quoted strings like `"25"` instead of `25`.

```json
{
  "master": {
    "tasks": [
      {
        "id": 1,
        "title": "Brief task title",
        "description": "What needs to be done",
        "status": "pending|in-progress|done|review|deferred|cancelled",
        "priority": "high|medium|low",
        "dependencies": [],
        "subtasks": [
          {
            "id": 1,
            "title": "Subtask title",
            "description": "Subtask details",
            "status": "pending"
          }
        ]
      }
    ]
  }
}
```

---
