# Blaze Viewer

Blaze Viewer is an image and video viewer with an obsession: Make viewing as fast and smooth as possible.

A key feature is "Blaze Mode": seeking through photos in a direction (forward, backwards, or 'randomly') as fast as a user wants, within the limits of the hardware.
The feel and acceleration of this is customizable, but on a powerful computer with a 120Hz display, that means we can literally display 120 images per second in a way that is actually useful. When a user stops and parks on an image, operations like rescaling, toggling fullscreen, and playing videos are all feel instant.

This project was started as a POC on 2026-06-26 and has moved quickly since then, adding video playback, archive support, a Mac-native UI, and many more features.

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

**Platform scope:** this section and the two below (HUD icons, the `pb-ui`
chrome) describe the **winit shell — Windows and Linux**. They apply to macOS
almost not at all: the Mac ships a **native SwiftUI/AppKit chrome**
(`mac/Sources/BlazeViewerMac/` — dialogs, inspector, folder tree, help are
native views, not egui) held to the "Mac-assed Mac app" bar — see
*Cross-platform discipline* below. The *behavior* (actions, keymap, session
logic) is still shared via `pb-app-core`; only the presentation is
per-platform.

### HUD / toast icons — Font Awesome solid

HUD overlays (the `pb-hud` crate — shell-neutral since NS0) composite text +
icons into one software RGBA8 pill, drawn as a single quad, rebuilt only on
change — off the photo hot path. Icons are **Font Awesome `solid`** (the house
style — duotone was tried and rejected 2026-06-28: muddy at toast size; don't
reintroduce it without a reason). The add-an-icon workflow lives in
`crates/pb-hud/CLAUDE.md`.

**Licensing:** FA **Pro** assets are licensed to the owner but **not
redistributable** — fine while the repo is private; any FA Pro glyph baked into
a *shipped artifact* is a compliance defect (see *Licensing*).

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

**What's there (summary):** tokens (4px spacing scale, `GAP` as *the* one gap
knob, radii, `CONTROL_H` = one 32px control height, `Palette` light+dark), the
theme functions (`install_fonts` once, `apply_style` per frame, `apply_to_ui`
to scope a region — re-assert inside combo popups), the component set (cards +
rows, toggles, buttons, fields, sliders, tabs, progress), and the semantic
`Icon`/`Tone` sprite system (white square sprites tinted at draw;
`lead_row`/`inline` bake alignment so there's no per-call nudging). Section
headings are Title Case; setting labels stay sentence case. **The full
token/component/icon inventory + the add-a-component and add-an-icon workflows
live in `crates/pb-ui/CLAUDE.md`; `lib.rs`/`icon.rs` are the source of truth.**

**Conventions:** primary = accent default action (Save/OK/Unlock); secondary = neutral
(Cancel); danger = red (Delete). Dialog status icons: Password = `Lock`/`Neutral`,
Message = `Warning`/`Warning`, Confirm-delete = `Trash`/`Danger`. `dialog.rs` keeps only
the *scaffold* (the second window + `button_bar` / `dialog_frame` panels) and **composes
pb-ui atoms** inside it. Theme is locked to the OS-resolved light/dark at open (explicit
`ThemePreference`, not `System`, so `apply_style` isn't re-clobbered).

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
- **AI describe/Ask (task #44) is the one network-touching feature, and it
  follows the Second Directive exactly.** macOS uses the **on-device** Apple
  Foundation Models backend. Private Cloud Compute isn't *precluded* — but
  it's cloud, so if/when it's integrated (a macOS/iOS 27 feature; those OSes
  are still in beta) it becomes **opt-in behind a warning** like any other
  egress, never a default. Everywhere else the image goes **only to the
  user-configured OpenAI-compatible endpoint URL** (LM Studio / Ollama —
  `pb-app-core::describe`), only on an explicit command or the opt-in
  auto-describe toggle, downscaled + JPEG-re-encoded first (the original
  HEIC/RAW bytes never leave). Results are RAM-only like every other cache,
  and the AI settings tab carries the privacy blurb (a custom endpoint means
  your images upload to that URL — keep it local/trusted).
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

## Cross-platform discipline (the platform priority, honestly stated)

**Design cross-platform first; make it idiomatic on the Mac; ensure it works
on Windows; keep Linux cheap.** In practice:

1. **Cross-platform first.** Behavior — actions, keymap, session logic, timing
   — lands in the shell-neutral core (`pb-app-core` + the trait seams), never
   in a shell. A feature designed inside one shell is a design bug even if it
   works.
2. **Idiomatic on the Mac.** macOS is not a port of the winit shell — it's a
   **native SwiftUI/AppKit host** (`mac/`, over `pb-mac-ffi`) held to John
   Siracusa's **"Mac-assed Mac app"** bar: platform conventions, native
   controls and feel, wherever possible. The house definition + concrete
   checklist is `.taskmaster/docs/macos-native-ui-plan.md` (§ *North star —
   what "Mac-assed" means here*). The full skill is **vendored at
   `.claude/skills/mac-arsed-mac-app/`** (SKILL.md + reference docs on
   detailed rules, SwiftUI/AppKit choices, and review/QA — MIT, from
   <https://github.com/bartreardon/skills>; note the English-variant
   spelling), so it's invocable as `/mac-arsed-mac-app` when doing Mac UI
   work — load it before designing or reviewing anything user-facing on
   macOS.
3. **Works on Windows.** Windows 11 is the primary dev/target hardware and
   ships the winit shell; every feature must hold the performance bar there.
4. **Linux is deliberately cheap — but never designed out.** It rides the
   winit shell + wgpu Vulkan with no polish pass; the rule is only that we
   never architect something in a way that would make Linux *hard* later.

The technical base that makes this affordable:

- **wgpu is the renderer everywhere** (DX12 on Windows, Metal on macOS, Vulkan
  on Linux) — chosen post-spike because CPU decode (2.5×) and the staging-ring
  upload (3.4×) already clear 120 Hz, so wgpu's portability costs nothing
  measurable (`decisions.md`).
- **`winit` owns windowing + input on Windows/Linux**; on macOS the Swift host
  owns them. Rendering/upload/decode sit behind the `Renderer`,
  `DecodeBackend`, and `UploadStrategy` traits.
- **GPU decode + zero-copy is a gated acceleration backend** (native D3D12 +
  nvImageCodec), pursued only if the high-MP stress test proves a need
  (ADR-012 kill criterion). The CPU pool is the permanent baseline and handles
  formats GPU decode can't.
- Do color management **in-shader** (matrix + TRC) so it ports unchanged.
- Isolate every platform-specific call (refresh-rate query, swapchain latency
  hook, media decode) behind a single helper seam — that's what made the Mac
  host a bounded surface instead of a rewrite.

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
  > Task #77 status (2026-07-19): the DLL linkage is done and **validated in shipped
  > 0.2.1 packages on both x64 and ARM64** (dumpbin-verified import tables; licenses/ +
  > the written offer ship in the payload). The one remaining item is the
  > `cargo license`/`cargo about` sweep of the Rust dependency tree. Trust
  > `pb-decode/build.rs` over this paragraph — it was stale once already.

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

## The decode & source stack (summary)

The deep detail lives with the code and auto-loads when you work there:
`crates/pb-decode/CLAUDE.md` (library picks, per-format backends, color/HDR,
video decode), `crates/pb-source/CLAUDE.md` (archive internals),
`crates/pb-app-core/CLAUDE.md` (item kinds / doors). The always-relevant map:

- **Dispatch** (`pb-decode`): sniff-registry + extension routing behind the
  `ImageDecoder` seam; every backend funnels through shared
  `finalize`/`finalize_oriented` (orientation + Lanczos decode-to-fit).
  Decoders are wrapped in `catch_panics` — a hostile file becomes a
  `DecodeError`, not a crash (release profile is `panic = "unwind"` for this).
- **Backends:** the `image` crate covers commodity formats (PNG/GIF/BMP/TIFF/
  WebP/TGA/QOI/…); specialty paths: `jxl-oxide`, `resvg` (SVG at display res),
  hybrid RAW (full-size embedded preview, else demosaic on a 256 MB-stack
  thread), Windows WIC / macOS ImageIO for AVIF+HEIC stills (`libheif` as the
  parallel-decode ship feature), and our own fuzzed `avis` demuxer + vcpkg
  dav1d for animated AVIF.
- **Color + HDR are wired end-to-end:** per-backend ICC/CICP parse (`moxcms`)
  → in-shader 3×3+TRC → fp16 scRGB intermediate → HDR `Rgba16Float` surface or
  tone-mapped SDR present. P3 HEICs show correctly; HDR gets real headroom.
- **Video** (Windows shipped; macOS routes MP4/MOV → AVPlayer, rest → the
  FFmpeg→wgpu Session route): typed `LibraryItemKind::Video` items, MF poster
  + ~20 ms header probe, demand-driven reader thread + unit-tested
  `VideoSession`, WASAPI/FFmpeg-first audio (MF can't do AC-3/E-AC-3/DTS).
- **Archives:** *doors* in the deck — `LibraryItemKind::Archive`; `P` enters
  (same path as the picker: password prompt, RAM pre-flight, progress),
  `Alt+Up` climbs out; **browsing past a door never decompresses** (the tile
  returns above the `bytes()` request; pinned by a panicking-source test).
  Behind the `ItemSource` seam: ZIP / 7z / tar family / RAR4 / RAR5, lazy vs
  eager-to-RAM per kind, refuse-before-reserve budgets, RAM-only (no-trace
  holds). ⚠️ **A new `LibraryItemKind` must opt *out* of byte reads, not into
  them** — read guards are positive (`Image` reads), kind matches exhaustive.
- **Known v1 limits** (deliberate): Radiance-HDR/EXR clamp to SDR; CMYK JPEG
  mis-colored; GIF/animated-WebP first-frame only; exotic ICC → sRGB
  passthrough. ⚠ On an HDR desktop, GDI capture of the flip swapchain is
  all-white — a Windows limit, not a render bug (`offscreen_png` verifies).
## Build, test, bench

> Rust toolchain required (`rustup`); `rust-toolchain.toml` pins stable + the
> components for coverage.

```sh
cargo test                 # unit + property + golden tests
cargo llvm-cov --workspace # coverage (target >80%)
cargo clippy --all-targets -- -D warnings
cargo fmt --all
# NOTE: no criterion bench harness is wired yet (benches/ is empty — see
# Instrumentation); measure with --metrics, PB_PERF, and ab_report instead.
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

### Working across two machines (the cross-machine handoff)

Most non-trivial work here spans **two machines**: `pb-mac-ffi` + `mac/` compile and run
only on the Mac, and `pb-app` only on Windows/Linux. Neither box can type-check the other's
shell, so a change to shared code is **half-verified by construction** until the other
machine sees it. Sessions cannot talk to each other — there is no cross-machine agent
channel — so **the repo is the message bus**, and it works well as long as everyone uses the
same three conventions:

1. **Read the plan's ledger first; write it last.** Each task plan carries one section
   titled **`## Handoff`** — the *only* place cross-machine state lives, so nobody has to
   guess whether it's §11 this time or §12.8 the next. It holds exactly three things:
   *verified* (what was actually run, on which platform), *not verified* (what the next
   machine must check, specifically enough to act on), and *decisions/corrections* (including
   corrections to earlier sessions — those are welcome, and have caught real errors).
2. **Never mark something verified that you could not run.** "Compiles" is not "works", and
   `cargo check --target x86_64-pc-windows-msvc` from a Mac is not a Windows run. Say which
   it was. A shell UX change is *behaviour-unverified* until someone launches the app.
3. **Leave a revert lever for anything unverified.** If you ship a UX change the other
   machine must confirm, keep the old path alive behind `#[allow(dead_code)]` and a one-line
   gate, and say in the Handoff what flipping it restores and when it may be deleted.

**⚠ The trap that has actually bitten (2026-07-20):** `pb-app` builds `AppCore` as a **struct
literal** while `pb-mac-ffi` goes through `AppCore::new_host`. So **adding a field to
`AppCore` breaks the winit build and not the Mac one** — and a Mac session gets no warning.
Adding shared state is exactly the case where the cross-check below is not optional.

**Cross-check before pushing shared-code changes**, so the other machine never pulls a red
tree:

```sh
# From the Mac, for the winit shell (needs the temporary Cargo.toml edits, then revert them):
cargo clippy -p pb-app --all-targets --target x86_64-pc-windows-msvc -- -D warnings
# ureq must be default-features = false PLUS features = ["json"], or pb-app-core
# fails on `send_json` before pb-app is even reached.
```

There is no equivalent from Windows: `pb-mac-ffi` is `#![cfg(target_os = "macos")]`, so on a
non-Mac it compiles to an **empty staticlib** and a syntax error in it produces *zero*
errors. Windows sessions must therefore treat every mac-shell edit as unverifiable and leave
it to the Mac — not attempt it blind.

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

All three platforms build + sign **locally** (hosted CI is billed money; the
`v*`-tag auto-build was deliberately removed): Windows ships **Velopack**
(channels `win` = x64, `win-arm64`), macOS a notarized DMG + **Sparkle**
auto-update, Linux a self-updating **AppImage** — all served from
`downloads.blazeviewer.app`. Every release script enforces the clean-tree gate
(a `-dirty` build id refuses to ship, checked before *and* after the build).

**The full procedure — build commands, signing, feeds, verification, and every
known trap (vpk re-run / cumulative feed / packId merge, YubiKey ssh from Git
Bash, mid-build lockfile dirt, the EdDSA key backup) — is the
`cutting-a-release` skill** (`.claude/skills/cutting-a-release/SKILL.md`).
Load it before any release work; never work from memory.

**Never let a tool auto-invoke a paid CI run** — no tag triggers, no hosted
builds from `gh release create`. Script it and run it locally.


## Project Task Tracking

Tasks live in `.taskmaster/tasks/tasks.json`, following
[taskmaster](https://github.com/eyaltoledano/claude-task-master) conventions:
`master.tasks[]`, each with numeric `id`, `title`, `description`,
`status` (`pending|in-progress|done|review|deferred|cancelled`), `priority`,
`dependencies`, and `subtasks` (same shape, numeric ids). Plans + research
notes live in `.taskmaster/docs/`; completed tasks may move to `archive.json`.
⚠️ IDs must be **numbers, not strings** — Task Studio can't look up `"25"`.
