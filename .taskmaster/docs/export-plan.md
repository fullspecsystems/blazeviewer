# Image Export — the full plan (task #89)

_Plan of record, **rev 4** — 2026-11-13. Rev 2 came from the owner design sessions plus a code
survey; rev 3 took review round 1 (Codex, 21 findings); rev 4 takes review round 2 (Claude Fable,
29 findings) and three owner decisions. Both review logs are in *Part 9*. Nothing is built yet.
"Today" claims were checked against `main` at `20407e91`; re-check before relying on one.
Related tasks: #81 (frame capture), #145 (gain-map decode), #146 (HDR brightness audit), #147
(existing Open-dialog OS traces), #148 (Copy carries the colour profile), #95._

**Review status:** rev 2 → Codex (gpt-6-astra, xhigh) → rev 3 → Claude (Fable 5.2, high) → rev 4.

## How to read this plan

| Part | What it covers |
|---|---|
| 1 | Product: goal, non-goals, how the directives apply, decisions |
| 2 | What exists today (survey, corrected by both reviews) |
| 3 | Architecture: crates, types, workers, safe writes, contract, dialogs, metadata/ICC, colour/HDR, settings, source support, input |
| 4 | The ladder: rungs 0–7 |
| 5 | Testing and measurement |
| 6 | Cross-platform execution |
| 7 | Risks |
| 8 | Open questions |
| 9 | Review logs |
| — | `## Handoff` |

Sizes are relative: **S** ≈ a focused session, **M** ≈ a few sessions, **L** ≈ a multi-session arc.

---

# Part 1 — Product

## 1.1 Goal

Export the current image — or the current video frame — to a file. The MVP is PNG with correct
colour. The full feature, inspired by squoosh.app: an **Export mode** with codec choice, quality
and resize; judging a lossy result against its source **on the real viewing surface** by flicking
between them; HDR- and wide-gamut-preserving output; and batch.

The reason to build this in Blaze rather than point people at Squoosh: the comparison happens in
the real viewer (GPU zoom, 1:1, instant flick), and it can show true HDR.

## 1.2 Non-goals

- Exporting what's on screen — crop, zoom region, a crop UI (*Later*).
- Editing beyond the existing rotation.
- Video clip export, transcoding, animated output.
- An App Store channel.

## 1.3 How the directives apply

- **Prime Directive.** Export never touches the blaze path: it runs on its own low-priority
  workers with their own capped thread pools, never takes a display decode worker, and never keeps
  the frame loop hot. In Export mode the interactions that must feel instant are *change a setting
  → see the result* and *flick source ↔ export*.
- **Decode-to-purpose.** Export decodes at full precision in the samples' true colour encoding.
  The display decode is untouched.
- **Never repeat likely work.** In Export mode the decoded source is kept in RAM, a setting change
  re-encodes without re-decoding, and the preview's encoded bytes **are** the file's bytes, so
  Export… after a preview writes them without re-encoding.
- **Measure.** p50/p95/p99, never means. `PB_PERF` measures input → present call; photon timing
  isn't wired, so the plan never claims it.
- **Privacy.** Exports are explicit user edits (ADR-018). Previews and caches are RAM-only.
  Metadata — including the colour profile's text — is stripped by default; Location never stays on;
  OS recent-file history is suppressed; OS dialog folder memory is removed.
- **Cross-platform first.** Encoders, state, rules, the allowlists and the panel model live in the
  core; shells render and run native dialogs.
- **Licensing.** Every crate in an encoder's dependency chain is audited before it lands. x265 is
  GPL → HEIC export is macOS-only via Image I/O. JPEG XL goes through `jxl-sys` (MIT OR
  Apache-2.0), never `jpegxl-rs` / `jpegxl-sys` (GPL-3.0-or-later). All choices stay compatible with
  the current FSL licence and with a future MIT or GPL licence.

## 1.4 Decisions

**Made (owner, 2026-11-13)**

1. The feature is in.
2. **What's exported:** the original at full resolution with the RAM rotation baked in.
3. **Metadata stripped by default** (*Strip metadata* checkbox; categories when unchecked).
   Location resets to off on every Export-mode entry.
4. **Wide gamut kept by default**; a "convert to sRGB" option may come later.
5. **Export folder:** beside the photo, remembered per source folder for the session only; a
   pinned "Open files in" folder is the start when there's no memory (*3.6*, revised 2026-11-14).
6. **Export mode replaces the right-hand Inspector** with export controls + details.
7. **The area under the panel is handled by zoom/pan.**
8. **Flick first, wipe later;** tap toggles, hold peeks.
9. **OS recent-file history is always suppressed** — not only when pinned.
10. **JPEG XL output is wanted**, via a thin safe wrapper over `jxl-sys`.
11. **Export mode is a mode, not a modal — `Esc` and `Enter` keep their global meanings**
    (owner, 2026-11-13, "try it this way"; review F1/F15). The test: the rest of the keyboard still
    drives the image (Space/Backspace navigate, `R` rotates, `Y` flicks), unlike Settings or a
    dialog, where focus has moved to another window. So `Esc` quits (ADR-023's insta-quit) and
    `Enter` is Random. Commit is the bindable `Action::ExportCommit` on `⌘↩` / `Ctrl+Enter` —
    the app's own precedent (Ask About Image submits with `⌘/Ctrl+Enter`) — plus File ▸ Export…
    (`⇧⌘E`), which enters the mode or commits in it; leave with `E`, the ✕ or the menu. The truly
    modal pieces inside the flow (save dialog, "Custom…" size, batch progress) follow dialog
    rules natively: `Esc` cancels, `Return` commits. A stray `Enter` is cheap to undo
    (`Shift+Enter` is RandomPrev, and settings carry across navigation).
    **Pinned for revisit — see *Part 8* "Pinned".**
12. **The flick compares the prepared source with the decoded export** (F2), so the difference on
    screen is exactly the codec (and resize).

**Recommended, not yet confirmed** — *Part 8*.

---

# Part 2 — What exists today

_Survey on `main` at `20407e91`, corrected by both reviews._

**Pixels and colour**

- **Copy** (`app_core_impl/clipboard.rs`) decodes at full resolution on the event loop, bakes
  rotation, passes `Rgba8` untagged, tone-maps `Rgba16F` with extended Reinhard at `peak` after
  clamping negatives (`engine.rs` `to_clipboard_rgba8`) — BT.709 output only.
- **`DecodedImage`**: `Rgba8` / `Rgba16F` (scene-linear BT.709 extended); `ColorTransform` is a
  display reduction — red-channel curve only, sampled curves fitted to a gamma, a loose sRGB test
  (gamma 1.9–2.5) (`color.rs`). It is not a faithful colour description for export.
- **macOS Image I/O SDR** draws every source into an **8-bit Display-P3 premultiplied** context
  (`imageio.rs` `draw_cgimage_p3`) — including malformed JPEGs recovered through Image I/O
  (`lib.rs` `recover_via_fallback`).
- **ICC bytes are read then discarded** (zune, `image_backend`, jxl). **Precision is narrowed**
  (`to_rgba8()`; JXL f32 → 8-bit; PSD).
- **HDR stills:** WIC (`128bppRGBAFloat`) and Image I/O (`ExtendedLinearSRGB`) floats are not
  explicitly normalized; only FFmpeg/AV paths normalize PQ to 203 nits. Unverified → task #146.
  HLG uses a per-channel 1000-nit OOTF approximation (`color.rs` `hlg_to_scrgb`). No gain-map
  handling (#145).
- moxcms 0.8.1: `ColorProfile::encode()` + standard profiles.

**Encoders and metadata**

- `pb_decode::encode_jpeg_rgba8` — one production caller (`describe.rs`). `png` **0.18.1** is
  already in the lockfile via `image` (native cICP/cLLI/mDCV support); 0.17 is a pb-app-core
  dev-tooling dependency.
- `little_exif` 0.6 writes; `kamadak-exif` reads. The Details cache stores EXIF as display strings.
- `catch_panics` is private to pb-decode and typed to `DecodedImage`.

**Video and animation**

- The capture effect is dead (emitted nowhere; winit no-op; FFI falls to `Other`). Copy on a video
  copies the **poster** (#81).
- Playback buffers are fit-scaled. `decode_animation` has a 1.5 GB cap that truncates early at
  full size. `stream_hdr_frames` has no seek. The AVPlayer route's core position is a ~20 Hz mirror
  — "not the authority" (`video_native.rs`). Windows' FFmpeg is container-reading only; decode is MF.
- Compare pins an item and stops playback on jump.

**Ring, presentation and rendering**

- Identity-stamped ring slots (`SlotIdentity { item, content_gen, rep: Fit | Original }`); a
  different rep of the same item can evict another at the same rank; `compact_to` re-keys
  survivors as `Original`.
- **`full_res_eligible` excludes RAW, SVG, > `FULL_RES_MAX_PIXELS`, video and doors** — no ring
  Original exists for them. **In Fit mode at zoom ≤ 1 the screen shows the `Fit` rep**
  (`present_kind`). Animation/video frames draw via `set_image`, not the ring.
- **The renderer already has out-of-ring slots:** `stash_fit` / `present_stash` / `clear_stash`
  (`pb-render/src/lib.rs`, `gpu.rs`), presented through `held`, with a core-side identity mirror.
  `sync_stash_external` **overwrites** the ring's external-bytes figure with the stash sum alone.
- **At least seven sites re-present on their own triggers** (stash, GPU derive, sharpen,
  `rebind_same_item`/`reconcile_zoom_rep`, `present_item`, `set_image`, animation/video) — the
  #124 bug class.
- Source-ICC (mode 1) textures are never mipped; HDR bypasses the dimension clamp; `present_slot`
  takes geometry from the texture's own dims.
- `content_gen` is global — it bumps on any deck change.

**Input, panels, frame loop**

- `KeyUp` only removes from `held`. `ActionKind { OneShot, Nav, Held, FrameStep, Repeat }`.
  **`E` unbound; `⌘S`/`Ctrl+S` = Save Rotation; `[`/`]` = slideshow interval (Repeat); `Enter` /
  `NumpadEnter` = Random (Nav); `Y` = CompareToggle.** `MenuAction(CompareToggle)` has no key-up.
- **`Esc`:** winit handles it in the shell (`main.rs`, straight to `begin_exit()` unless a dialog
  is open); macOS resolves it via keymap → `Action::Quit` → `terminateNow()`. ADR-023: Esc stays
  unconditional quit.
- winit sends **no keyboard** to the egui overlay (ADR-023: no inline text inputs). macOS forwards
  every host-window key to the core.
- **Panel edits are core method calls**, not events: winit applies `PanelAction`s by calling core
  methods; the FFI exposes per-call methods.
- `SlotContent { Details, Text, Describe, Help }`.
- **`work_pending` keeps the frame pump running at display rate** while work is in flight — measured
  to drop frames when misused (`app_core_impl.rs`). winit has an `EventLoopProxy`.
- **The global rayon pool** is already used by display decodes (`jxl-threadpool`, `imagepipe`,
  `rawloader`, `jpeg-decoder`).

**Dialogs, settings, files, privacy**

- No save dialog exists. Open dialogs: `NSOpenPanel` with explicit `directoryURL`; `rfd` 0.14.1
  (only `FOS_PICKFOLDERS` / `FOS_ALLOWMULTISELECT`).
- **The macOS Open panel already persists `NSOSPLastRootDirectory` in the app's defaults domain**
  (confirmed on the dev Mac by key name) → task #147. On Windows, the Open dialog without
  `FOS_DONTADDTORECENT` likely adds opened photos to Recent Items / the Jump List, and the common
  dialog keeps its own MRUs (`ComDlg32\OpenSavePidlMRU`, `LastVisitedPidlMRU`) — unverified, #147.
- Settings: `picker_dir` ("Open files in"); `last_folder` (ADR-022) — **written even when pinned**
  (`nav.rs`). A deserialize error resets the **whole** config; several paths call `settings.save()`
  unconditionally.
- `quick_sort::unique_name` reserves nothing; `save_rotation::temp_sibling` uses a fixed name.
- `archive::ram_budget()` exists. The macOS app is not sandboxed (TCC-gated folders apply).
- No-trace: `viewing_a_folder_writes_nothing_to_disk` is in pb-app (Windows/Linux only); a
  core-level precedent that runs on macOS exists (`app_core_impl/thumbs.rs`).
- `pb-app` builds `AppCore` as a struct literal (the cross-platform trap).

---

# Part 3 — Architecture

## 3.1 Crates and modules

**Dependency direction** (F4): `pb-encode` holds the shared pixel/colour types, `prepare`, the
allowlists and the encoders, and **does not depend on pb-decode or pb-app-core**. `pb-decode`
depends on `pb-encode` **for types only** (`default-features = false`); every codec backend sits
behind a cargo feature so decode examples and fuzz builds never pull C/C++ encoders. `prepare` takes
a plain `PrepareSpec`, never an `ExportRequest`. The JPEG encoder moves into pb-encode **without** a
re-export; `describe.rs` (its only caller) changes its import.

| Where | What |
|---|---|
| **`crates/pb-encode`** (new) | Types; `prepare`; the sanitized-profile builder (*3.7*); the EXIF allowlist builder; PNG (rung 0c); the `Encoder` trait + `Caps` (rung 2); other codecs behind features (rung 4); the batch planner (rung 6); a `catch_panics` wrapper for encoders |
| `pb-decode` | `decode_for_export` → `ExportSource` (rung 0b), incl. the macOS export-specific Image I/O draw |
| `pb-render` | Two **aux textures** (A = prepared source, B = decoded export), modelled on the stash slots: upload / present / clear, identity guard, display-geometry override, mips for every colour mode, dimension clamp (rung 3) |
| `pb-app-core/src/export.rs` | `ExportState`: mode, draft settings, pending requests + destinations, in-flight temps, caches, panel model |
| `pb-app-core/src/export_worker.rs` | Write worker (rung 0c); preview worker (rung 3); worker-owned rayon pools |
| `pb-app-core/src/spring.rs` | Tap/hold state machine (rung 3) |
| `pb-app-core/src/app_core_impl/export.rs` | The `AppCore` export methods (two-halves rule) |
| `pb-app-core/src/app_core_impl.rs` | **The present gate** (F3): one "what is presented" field and one gate every re-present site goes through — this file's residency-and-present charter, not `export.rs` |
| `settings.rs` | `ExportPrefs` (lenient section). (`last_export_folder` was removed 2026-11-14 — § 3.6) |
| `pb-mac-ffi` + `mac/` | Per-call FFI methods, `NSSavePanel`, panel-key cleanup, the SwiftUI panel, key arbitration |
| `pb-app` | `IFileDialog` wrapper (open + save) on Windows, `rfd` on Linux, the pb-ui egui panel |

**One field on `AppCore`:** `export: export::ExportState` (`Default`) — the winit struct literal
changes once. The present-gate field is a second, added in rung 3 with the same cross-check.

## 3.2 Core data types (sketch)

```rust
// pb-encode — no pb-decode / pb-app-core dependency
pub enum ExportPixels { Rgba8(Vec<u8>), Rgba16(Vec<u16>), RgbaF32(Vec<f32>), Gray8(Vec<u8>), Gray16(Vec<u16>) }

pub enum SampleEncoding { Encoded, Linear }   // Linear + Hdr = the named convention (3.8)

pub struct ColorDesc {
    pub icc: Option<std::sync::Arc<[u8]>>,    // the source profile bytes, if any
    pub cicp: Option<Cicp>,                   // from nclx / cICP / VUI
    pub hdr: Option<HdrTransfer>,             // Pq / Hlg / LinearScene
}

pub struct ExportSource {                     // what decode_for_export returns
    pub width: u32, pub height: u32,
    pub pixels: ExportPixels,
    pub encoding: SampleEncoding,
    pub color: ColorDesc,                     // the TRUE description of these samples (3.8 Rule 1)
    pub alpha: AlphaMode,                     // None | Straight | Premultiplied
    pub provenance: Provenance,               // which path produced it (for the details panel)
}

pub struct PrepareSpec {
    pub rotation: Quadrant,
    pub target: TargetCaps,                   // what the output format can carry
    pub color_policy: ColorPolicy,            // KeepSanitized | KeepOriginal | Canonical(StandardSpace)
    pub tone_map: Option<ToneMapSpec>,        // only for SDR targets of HDR sources (3.8)
    pub resize: Option<Resize>,               // rung 2
    pub flatten_onto: Option<[u8; 3]>,        // rung 2, formats without alpha
    pub zero_transparent_rgb: bool,           // with Strip on (F25)
}

pub fn prepare(src: ExportSource, spec: &PrepareSpec) -> Result<ExportImage, PrepareError>; // consumes src (F27)
```

```rust
// pb-app-core::export — frozen at command time (C11)
pub struct ExportRequest {
    pub id: u64,
    pub source: SourceRef,        // Arc<dyn ItemSource> + item index within it + a stable item key (path / entry name)
    pub frame: FrameRef,          // Still | AnimationFrame(k) | VideoAt { pts, track } | Captured(pixels)
    pub rotation: Rotation,
    pub settings: ExportSettings, // format, quality, … , MetadataPolicy (not a built block — F4)
    pub dest: Option<PathBuf>,
}
```

- The **metadata block is built on the worker** from the frozen `MetadataPolicy` and typed tags
  parsed from the source bytes.
- A pending request's `Arc<dyn ItemSource>` pins an eager archive's RAM while its dialog is open —
  acceptable and documented (F27).

## 3.3 Workers, scheduling, wake, quit

- **Write worker** (rung 0c): FIFO, one job at a time (batch: capped N). Survives navigation;
  cancelled only by the user or quit.
- **Preview worker** (rung 3): latest-wins (one in flight + one queued); cancellation checkpoints
  between decode → prepare → encode → decode-back; non-interruptible results discarded.
- **CPU isolation** (F8): neither uses the decode pool. **Every rayon-using codec runs inside a
  worker-owned `rayon::ThreadPool`** with a capped thread count (the global pool is shared with
  display decodes). Export threads run at **low OS priority** (`QOS_CLASS_UTILITY` on macOS,
  `THREAD_MODE_BACKGROUND_BEGIN` on Windows, `nice` on Linux). Backends with internal threading
  (libheif) are capped in the export decode.
- **Wake** (F8): **never added to `work_pending`.** Workers wake the loop event-style — winit's
  `EventLoopProxy`; a main-actor kick on macOS — or, if that's impractical for a shell, a low-rate
  `SetWake` poll (~20 Hz) while export work is in flight.
- **RAM admission (refuse-before-reserve):** estimate source bytes + decoded + prepared + scratch +
  output bound against a budget from `archive::ram_budget()`. File exports refuse with a message;
  previews may reduce resolution with a visible note.
- **Panics:** encoder calls go through pb-encode's `catch_panics` wrapper; a panic is an error
  toast. A disconnected channel is a failure.
- **Quit** (F11): `HideWindow` first, then let the **single in-flight write finish within a bound**
  (a few seconds); cancel queued jobs and batches; unlink tracked in-flight temps from the main
  thread during teardown. Test: quit mid-write leaves either the finished target or nothing.

## 3.4 Writing files safely

- **Temp files:** `create_new` a sibling named `<stem>.blaze-export-<random>.tmp` (no leading dot —
  a crash leftover stays visible, F7), tracked in `ExportState` until committed or unlinked.
- **Commit, in order of preference** (F7; as built in session 1):
  1. **Hard link** temp → target, then remove the temp — atomic and no-clobber on APFS, NTFS and
     ext4, with portable `std` calls (built instead of `renamex_np` / `renameat2` / `MoveFileExW`).
  2. **Placeholder:** where links aren't supported (exFAT/FAT, many SMB shares), `create_new` the
     target as a zero-byte placeholder, then rename the temp over our own placeholder.
  - **Confirmed replace** (the dialog confirmed an overwrite): rename-over.
  - On macOS, `FileManager.replaceItemAt` with an `itemReplacementDirectory` is an option for the
    replace case.
- **If the temp sibling can't be created** (`EPERM`/`EACCES` — e.g. a TCC-gated folder whose grant
  covers only the chosen file): write directly to the granted path, accepting non-atomicity, and
  delete it on failure.
- **Pending destinations** (F7): `ExportState` tracks destinations of queued/in-flight exports from
  rung 1; suggested names avoid them, and a dialog result colliding with one is refused with a
  message — so two queued exports can't race to one new name.
- **Never overwrite the source:** compare **file identity** (device + inode / volume serial + file
  index), not paths — catches hard links (F26).
- **Existence checks happen off the event loop** (a sleeping network share can block) (F27).
- **Durability:** `fsync` the temp before rename on local volumes; network volumes best-effort.
  Nothing scans for or deletes stray temps.
- **Hand checks in rung 1:** an exFAT stick, an SMB share, and a TCC folder (Desktop) after
  `tccutil reset`.

## 3.5 Contract and FFI additions (per rung)

Following the existing patterns (F22): **panel edits are core method calls**, not events — winit
via `PanelAction`-style calls, macOS via per-call FFI methods.

| Rung | Core / contract | macOS shell | winit shell |
|---|---|---|---|
| 0a | `CaptureVideoFrame { request_id, … }` effect; `video_frame_captured(request_id, time_used, pixels, color)` method | AVPlayer capture handler; FFI arm | MF capture path; match arm |
| 1 | `Action::Export` (all ~17 `Action` registration sites, per #137.7); `ShowSavePanel { request_id, start_dir, suggested_name, format }`; `save_panel_resolved(request_id, path, replace_confirmed)`; `MenuState` + `ContextMenuState` fields (the latter crosses the FFI positionally); help-overlay rows | `NSSavePanel` sheet; panel-key cleanup (*3.6*); menu item `⇧⌘E` via `macos_menu_chord` | `IFileSaveDialog` wrapper; `rfd` on Linux; menu item |
| 2 | `SlotContent::Export`; `ExportPanelModel`; `export_set_*` methods; `Action::ExportCommit`; mode toggle; `DialogKind` + `DialogResult` for "Custom…"; generalize `menu_hint`'s single bool into a context | SwiftUI panel; Custom… sheet | pb-ui panel; a **pb-ui checkbox component** if missing; Custom… dialog |
| 3 | Spring key resolution (`KeyResolution::SpringStart` + a key-up path); `export_flick(pressed, timestamp)`; `export_toggle()` for menus/accessibility | Panel button down/up with `NSEvent.timestamp`; VoiceOver toggle | Button down/up with shell-stamped time |
| 6 | `ShowFolderPanel { request_id, start_dir }` | `NSOpenPanel` (folders) | wrapper (folders) |

**Every new effect/method lands with both shells' consumers in the same rung**, or *Handoff* records
cross-platform debt.

## 3.6 The export folder and OS dialogs

**Revised by the owner, 2026-11-14** (after using it: "the save dialog opens to the last folder I
was in … doesn't feel like what I'd want most of the time"). The dialog starts, in order:

1. where you last exported **from this same source folder** this session (`ExportState::
   folder_memory`, keyed by the image's folder or the archive; RAM only) — a run of exports
   from one folder keeps landing together;
2. else the **pinned** "Open files in" folder, if one is set;
3. else **beside the photo** — its own folder (an archive entry: the archive's folder), as
   Preview's Export does.

**Nothing is remembered across launches**, and export writes nothing but the file:
`Settings::last_export_folder` (rung 1's ADR-022 addendum) and the Q6 "pinning clears it" rule
are **removed**. An existing `settings.toml` still loads (unknown keys are ignored) and drops the
stale key at its next save.
- **Always pass an explicit start folder.**
- **OS file history is suppressed** (owner decision 9):
  - **macOS:** after **any** open or save panel closes, remove `NSOSPLastRootDirectory`,
    `NSNavLastRootDirectory` and `NSNavRecentPlaces` from the app's defaults (the app always passes
    `directoryURL`, so nothing is lost). The Open-panel half is task #147.
  - **Windows:** one `IFileDialog` wrapper for **open and save** in `pb-app`:
    `FOS_DONTADDTORECENT` (+ `FOS_OVERWRITEPROMPT | FOS_STRICTFILETYPES | FOS_FORCEFILESYSTEM` for
    save). Rung 1 checks whether entries still land in `ComDlg32\OpenSavePidlMRU\<ext>` and
    `LastVisitedPidlMRU`; if they can't be suppressed, that's an owner decision. Moving the Open
    dialog onto the wrapper is task #147.
  - **Linux:** check whether the GTK dialog records into `recently-used.xbel`; if it can't be
    prevented, owner decision.
- **Native-dialog persistence is verified by hand** on each platform; the core no-trace test can't
  see OS-owned writes.

## 3.7 Metadata and the colour profile

**Never user choices:** orientation (always 1); embedded thumbnails/previews (always dropped);
colour meaning (always preserved).

**The colour profile is sanitized, not rebuilt** (F6 replaces rev 3's Standard/Matrix/LUT split):

- With **Strip metadata on**, the output profile is the source profile with:
  - **data tags copied verbatim from an allowlist** — `rXYZ gXYZ bXYZ wtpt chad rTRC gTRC bTRC kTRC
    A2B0..2 B2A0..2 cicp` (plus what the profile class requires);
  - neutral `desc` and `cprt` written by us;
  - the header's creator, manufacturer, model, date and profile ID zeroed;
  - **every other tag dropped**.
- A **canonical** profile is swapped in only on a **tight, TRC-inclusive match** to a standard
  space (never the display's loose gamma 1.9–2.5 test).
- Pixels are untouched for every RGB profile — no CPU colour conversion in the MVP. Gray sources
  keep a gray profile and are written as gray where the format allows (PNG, JPEG); CMYK is refused
  in v1.
- **Strip off + "Original colour profile":** the source bytes verbatim, with a details note.
- **Residual risk (documented):** a custom-calibrated profile's numbers can remain a weak device
  fingerprint even when sanitized.
- **Untagged sources** are tagged sRGB (PNG: an `sRGB` chunk, F25).

**Pixels under transparency:** with Strip on, RGB is zeroed where alpha is 0 — hidden pixel data
under transparent areas can't leak (F25).

**The metadata control:** ☑ *Strip metadata* (checked on every entry — *Part 8*). Unchecking reveals:

| Category | Default when revealed | Sticky | Contents |
|---|---|---|---|
| Camera & date | ☑ | yes | Capture date/time + offset, make/model, lens, exposure |
| Copyright & credit | ☑ | yes | EXIF Artist / Copyright |
| Location | ☐ | **never** (and forced off in batch — F19) | GPS IFD |
| Generation data | ☐ | yes | AI prompts/workflows (#137); PNG → PNG first; the panel says workflows can carry file/model names |
| Original colour profile | ☐ | yes | Source bytes verbatim |

- Per-photo warnings; categories the photo lacks are greyed.
- **Allowlist only:** typed tags parsed from the source bytes on the worker (not the Details cache's
  display strings), values bounded, written into a fresh block — JPEG APP1, PNG `eXIf`, WebP `EXIF`,
  AVIF/HEIC Exif item, JXL `Exif` box. **Decided 2026-11-25:** kamadak-exif's own writer
  (`exif::experimental::Writer`, BSD-2, already shipped via pb-decode); no new crate. TIFF writes
  native tags + real Exif/GPS sub-IFDs; HEIC goes through Image I/O's property dictionaries.
- **The sanitized-profile builder and its leak fixture land in rung 0c**, because rung 1 already
  ships Strip-on exports (F6).

## 3.8 Colour and HDR

**Rule 1 — each export backend reports the truth.** `ExportSource.color` describes the samples
actually returned.

- **macOS export decode does not reuse the 8-bit P3 draw** (F5). It gets its own Image I/O draw: the
  `CGImage`'s **own colour space** when it is RGB, **16 bpc** when the source exceeds 8 bits,
  **straight alpha**, and ICC from `CGColorSpaceCopyICCData`. It falls back to P3 only when the
  source space can't back a bitmap context, tagged P3.
- Rung 1 acceptance includes: an sRGB AVIF/HEIC exports as sRGB with unchanged samples.

**Rule 2 — one luminance convention:** linear, BT.709 primaries, 1.0 = 203 nits. Every HDR backend
normalizes into it — established by task #146 (PQ **and HLG** anchors) before any HDR output ships.

**Rule 3 — branch on the real transfer.** PQ keeps code values only when primaries/matrix/range are
compatible. **HLG stays HLG** (cICP 9/18) where the format allows — the HLG decode path is an
approximation (F18). SDR P010 is SDR.

**Rule 4 — read-back:** never write an HDR format we can't read back.

**HDR → SDR (rungs 1–4 and SDR targets)** (F18): tone-map in **Display-P3 linear** (not BT.709 —
negatives aren't clamped away first), with the white point from the **full-resolution prepared
image's** peak, then encode P3 and tag P3. Operator (revised 2026-11-14 after the owner saw exported HDR reds turn magenta): **hue-keeping**
extended Reinhard — out-of-gamut channels are clipped to 0 (desaturating toward luminance was
tried first, and added blue to the owner's BT.2020-red test bar), then the
*brightest channel* follows the curve and all three scale with it (`pb_color::tonemap`). The same
maths runs in the clipboard path (Copy / OCR / Describe) and the renderer's SDR shader (HDR images
only — SDR content is untouched), so an SDR export still matches what an SDR screen shows. The
per-channel curve it replaced compressed each channel separately, which pushed bright crimson to
magenta and orange to yellow. The macOS EDR highlight roll-off now scales by the brightest channel
too.

**Light-level metadata:** from the final prepared image — per-pixel max(R, G, B) in nits; MaxFALL =
the mean of those maxima; alpha ignored; non-finite → 0; units per format (PNG `cLLI`:
0.0001 cd/m²). Never the display `peak`.

| Format | Wide gamut | HDR | Notes |
|---|---|---|---|
| PNG | 8/16-bit + `iCCP` / `sRGB` | 16-bit + `cICP` + `cLLI` (`png` 0.18 reads them; write as raw chunks) | HDR PNG reader support is uneven; macOS Image I/O reads it |
| JPEG | 8-bit + ICC | Gain map | Most compatible HDR |
| WebP | ICC | — | HDR → tone-mapped, with a note |
| AVIF | ICC or nclx, 10/12-bit | PQ/HLG + `clli`; gain maps | Best all-rounder |
| JPEG XL | Yes | Native | Weakest reader support |
| HEIC | Yes | PQ/HLG or gain map | macOS only |

## 3.9 Settings and persistence

- `export: ExportPrefs` is loaded through **one lenient deserializer** (F16): parse the section to a
  `toml::Value`, try `ExportPrefs`, fall back to defaults. A type error in `[export]` can never reset
  the whole config. (This replaces rev 3's per-field `serde(default)` + enum fallbacks.)
- Persisted preference ≠ current capability: a saved HEIC on Windows/Linux stays saved; the panel
  uses the default format that session.
- `last_export_folder: Option<PathBuf>` — *3.6*. The pinned session folder lives in `ExportState`.
- Tests: missing / partial / malformed / unknown-value sections, an unavailable codec, a round trip.

## 3.10 Source support matrix

| Source | `decode_for_export` | Policy |
|---|---|---|
| JPEG (RGB / gray) | 8-bit + profile (zune) | Gray stays gray where the format allows |
| CMYK JPEG | — | **Refused** in v1 |
| Malformed JPEG recovered via Image I/O | macOS export draw (Rule 1) | Details note "recovered" |
| PNG / TIFF / WebP / BMP / TGA / QOI / PNM / ICO / DDS / FF | 8/16-bit + profile (`image`) | Gray stays gray |
| Float TIFF / EXR / Radiance | f32 linear | HDR in the named convention; SDR targets tone-map |
| GIF / APNG / animated WebP / AVIF | Frame 0 at full res (rung 0a: the frozen frame) | Composited |
| JPEG XL SDR | f32 → 16-bit encoded + profile | — |
| JPEG XL HDR | rung 5 | — |
| HEIC/AVIF SDR — Windows | 8/10/12-bit + nclx/profile (WIC/libheif) | — |
| HEIC/AVIF SDR — macOS | Export draw: own space, 16 bpc if > 8-bit, straight alpha | P3 only as a fallback |
| HEIC/AVIF HDR | f32, normalized (#146) | SDR targets tone-map (*3.8*) |
| PSD | In-crate composite, or the export Image I/O draw | Composite only |
| SVG | **Rasterized at the target size** (intrinsic size when not resizing) | No raster upscaling involved (F20) |
| RAW | Full-size embedded preview when `raw.rs` uses it, else demosaic | Details say which |
| Video frame / Live Photo motion | Rung 0a | Disabled until 0a |
| Archive entry | As its kind, via `SourceRef` | — |
| Door | — | Not exportable |

- **Resize** (rung 2): Lanczos3 in **linear light with premultiplied alpha**, then re-encoded.
  **Presets never upscale raster sources** (F20). The GPU texture limit caps **previews**, never
  the file.
- **Alpha:** formats without alpha flatten onto white in v1 (details note).

## 3.11 Input routing and the spring state machine

**Keys in Export mode:**

| Key / input | Meaning |
|---|---|
| `E` | Toggle Export mode |
| `Action::ExportCommit` (proposed `⌘↩` / `Ctrl+Enter`, bindable) | Export… |
| File ▸ Export… (`⇧⌘E` on macOS) | Enter the mode; in the mode, commit |
| Panel ✕ / menu | Leave the mode |
| `Esc` | **Quit, as everywhere** (ADR-023) — unless the owner amends it |
| `Enter` / `NumpadEnter` | Random, as everywhere |
| `⌘S` / `Ctrl+S` | Save Rotation |
| Space / Backspace | Navigate, keeping settings |
| `[` / `]` | Quality down / up (contextual; slideshow is stopped in the mode) |
| `Y` | Flick (rung 3) |
| `Tab` | Hide panels: Export mode **suspends** (contextual keys revert; preview hidden); `Tab` again resumes |

- **No inline text fields** — ADR-023 and the held-key release net stay intact. Resize uses presets
  + "Custom…" (a macOS sheet / the pb-ui dialog window).
- **Checkboxes and menus on winit are pointer-operated** unless given contextual bindings — the
  keyboard-only acceptance in rung 2 says exactly which controls are keyboard-reachable (F23).
- **macOS Tab focus** (Full Keyboard Access): while a panel control is first responder the key
  monitor passes keys through, except key-ups for keys the core still holds. Rung 2 decides whether
  it ships then (*Part 8*). VoiceOver acts through accessibility actions.

**Spring state machine** (`spring.rs`). State: `shown ∈ {Export, Source}`, `owner ∈ {None,
Key(physical), Pointer}`, `pressed_at` (an **event timestamp** stamped by the shell at receipt —
`NSEvent.timestamp` on macOS — so a stall between down and up can't turn a tap into a peek, F14).

| Event | Condition | Result |
|---|---|---|
| Down(src) | owner = None | switch `shown`; owner = src; `pressed_at` = event time |
| Down(src) | owner ≠ None | ignored |
| Down (auto-repeat) | — | ignored |
| Up(src) | src = owner, held < `SPRING_HOLD` | keep (a tap); owner = None |
| Up(src) | src = owner, held ≥ `SPRING_HOLD` | switch back (a peek); owner = None |
| Up(src) | src ≠ owner | ignored |
| Toggle (menu / accessibility) | owner = None | switch `shown` |
| **Settings change / navigation / rotation** | any | owner = None; **`shown` = Export** — you always see the effect of what you changed |
| FocusLost / leave or suspend mode | any | owner = None; `shown` = Export |

- `Y` currently resolves as `OneShot`, and key-up only removes from `held` → rung 3 adds a
  `KeyResolution::SpringStart` and a key-up path (*3.5*).
- Physical-key ownership survives a keymap change mid-hold. `SPRING_HOLD` ≈ 300 ms in `engine.rs`.

**Mode × action table** (F17) — every row becomes a test in rung 2/3:

| While in Export mode… | Behaviour |
|---|---|
| Slideshow running | Entering the mode stops it; slideshow keys are reassigned |
| `P` on an animation / video / Live Photo | Plays as normal; the preview is dropped while playing; when paused, the frozen frame (rung 0a) becomes the source |
| Delete / Quick Sort the current item | The mode stays on the next item; the preview is dropped |
| Open a new deck / streaming scan | The mode stays; the preview is rebuilt for the new item |
| Land on a door or CMYK JPEG | The panel says why it can't export; Export… disabled |
| `Tab` (hide panels) | Mode suspends (above) |
| Compare pin (`⇧Y`) | Suspended in the mode |

**Preview identity** uses the request-style `SourceRef` + stable item key, **not** the global
`content_gen` (which bumps on unrelated deletes, F17).

---

# Part 4 — The ladder

| Rung | Deliverable | User-visible | Depends on | Size |
|---|---|---|---|---|
| 0a | Full-res frame capture (#81) — own capture thread | Copy/OCR/Describe use the real frame | — | M–L |
| 0b | `decode_for_export` + minimal `prepare` | No | — | M |
| 0c | `pb-encode` (types, PNG, sanitized profile) + write worker + safe writes | No | 0b | M |
| 1 | MVP: Export as PNG | **Yes** | 0b, 0c (0a for video/animation frames) | M |
| 2 | Export mode panel (+ `Encoder` trait, resize, flatten, JPEG) | **Yes** | 1 | L |
| 3 | Live preview + flick (present gate, aux textures, preview worker) | **Yes** | 2 | L |
| 4 | Codec breadth | **Yes** | 2 (3 for previews) | L |
| 5 | HDR export | **Yes** | 4, #146; #145 for gain-map sources only | L |
| 6 | Batch | **Yes** | 2 | M |
| 7 | Later | — | — | — |

Each rung lands core-first with tests, then shell by shell; user-visible rungs add a `CHANGELOG.md`
line; every rung updates *Handoff*.

## Rung 0a — Full-resolution frame capture (task #81)

Independent of #89: it runs on **its own one-shot capture thread** at low priority (F12).

- **Frozen identity:** video `{ pts, track, seek_generation }`, animation `{ frame index }`.
- **Session route** (FFmpeg on macOS, **MF** on Windows): a one-shot full-res decode at the frozen
  PTS (seek to the preceding keyframe, decode forward) from the same input and selected track; P010
  PQ/HLG kept with its real transfer.
- **AVPlayer route:** the core can't freeze an exact PTS (its position is a ~20 Hz mirror), so **the
  shell reads `currentTime()` when it handles the effect**, captures with `AVAssetImageGenerator`
  (zero tolerance, full size), and **the reply carries the time used**. For HDR, add a seek/PTS entry
  point to #144's `stream_hdr_frames`.
- **Animations:** composite-and-discard up to frame k at full resolution (not `decode_animation`,
  whose 1.5 GB cap truncates early).
- **Consumers:** Copy, OCR, Describe, Export. **Copy on a video becomes asynchronous** — a
  copy-when-done state, as `copy_image_text` already has. Compare is out of scope.
- Replies validate against the request; on demand only.

**Tests.** Stale replies rejected; frozen identity survives navigation; pixel conversions; the
animation compositor matches playback at full resolution. **Acceptance.** A paused mid-scene frame
reaches Copy/OCR/Describe at native resolution on both platforms and both macOS routes.

**Built (session 3, 2026-11-14) — with two deliberate deviations from the design above:**
- **No shell round trip on the AVPlayer route.** AVPlayer's periodic time observer fires on
  pause, seek and step (and `pause()` now publishes explicitly), so the core's mirrored position
  is *exact* whenever the video is paused — the case that matters — and at most one ~50 ms report
  stale while it plays. The frame is then decoded **in Rust through FFmpeg** (present in every
  normal Mac build — dev and release; a `--no-ffvideo` build refuses video capture with a toast)
  rather than `AVAssetImageGenerator`: full resolution, HDR on the verified 203-nit scale, archive
  bytes included, and unit-testable. The never-emitted `CaptureNativeVideoFrame` effect and
  `NativeCapturePurpose` were deleted.
- **No dedicated capture thread.** OCR and Describe already run on workers, so `scan_job` /
  `describe_job` call `frame_capture::capture` there; Export does it on the export worker; only
  Copy needed a new one-shot thread (`pb-frame-copy`, low priority).
- **Where things are:** `pb-app-core/src/frame_capture.rs` (`FrameSource` frozen at command
  time, `FrameKey` cache key, `capture`) + `app_core_impl/frame_capture.rs` (`frame_target` /
  `frame_key` / `frame_is_moving`, async Copy); `pb-decode/src/ffmpeg/frame_at.rs`
  (`ff_decode_frame_at`: keyframe seek, decode forward, the last frame ≤ t + 1 ms, refcounted
  `av_frame_ref` so only the winner leaves the GPU); Windows uses MF's
  `decode_video_poster_replay(origin 0, t)` (native-capped 4096, SDR — compile-checked only).
- **Sources of `t`:** the Session route's presented `current_pts`; the native route's mirror; a
  Live Photo's motion = the sum of earlier frame delays; an animation = its index (re-decoded at
  full size via `decode_animation_cancellable(None)` only when the shown frame was fitted).
- **Caches:** `ImageText.frame` and `frame_capture.described` record the frame each OCR /
  description came from; a cached result is reused only for the same frame. The automatic runs
  (Text tab open, opt-in auto-describe) skip a *playing* video/animation and read the paused
  frame once it stops.
- **Measured:** frame captures on local 1080p/1440p HEVC + H.264 phone clips: 25–125 ms (the
  first includes hardware-decoder start-up).

## Rung 0b — `decode_for_export` + minimal `prepare`

- `decode_for_export(bytes, name, frame) -> ExportSource` per *3.10*, including the **macOS export
  Image I/O draw** (*3.8* Rule 1). No fit-scaling; orientation for every sample type.
- `prepare` in this rung does only what the MVP needs: un-premultiply, rotate (any sample type),
  tone-map per *3.8* for SDR targets, gray handling, zero-under-transparency. Resize and flatten wait
  for rung 2 (F13).
- **No Copy change here** — Copy gaining a profile is task #148 (F13).

**Tests (first).** Through `decode_for_export` and **independent container inspection** (never the
display decode): 16-bit PNG keeps 16 bits; P3 JPEG/PNG returns its ICC byte-identical; the macOS
export draw returns an sRGB AVIF's samples unchanged and a 10-bit HEIC at 16 bpc; rotation
proptest; un-premultiply; tone-map against reference values. Malformed-input tests and fuzz entry
points for the new decode variants.

**Acceptance.** Display decode output bit-identical before/after on the corpus; blaze p95 unchanged.

## Rung 0c — `pb-encode`, write worker, safe writes

- `crates/pb-encode` per *3.1*: types; `prepare`; **PNG via `png` 0.18** (8/16-bit, gray, colour-type
  reduction — opaque → RGB; `iCCP` / `sRGB`; raw chunks); the **sanitized-profile builder**; the JPEG
  encoder moved in (describe.rs import updated). **No `Encoder` trait yet** (F13).
- `export.rs` + `app_core_impl/export.rs` (two halves); `export_worker.rs` with the **write worker
  only**, worker-owned rayon pool, low priority, event-style wake (*3.3*); safe writes (*3.4*);
  quit behaviour.
- The single `export` field on `AppCore`; **Windows cross-check from the Mac**.
- **Core no-trace test that runs on macOS** (precedent: `app_core_impl/thumbs.rs`): viewing writes
  nothing; one export writes exactly one file, leaves no temp, and sets `last_export_folder` only
  when unpinned.
- **ICC leak fixture + colorimetry round trip** (F6): a profile carrying identifying text in `desc`,
  `cprt`, `dmnd`, `dmdd` and a private tag → the sanitized output contains none of it and transforms
  colours identically (moxcms round trip).

**Tests.** 8/16-bit/gray PNG round trips pixel-identical; profile bytes as intended; write jobs
survive navigation; failures and panics leave no file; every commit path incl. the placeholder
fallback; pending-destination collisions; file-identity source refusal; quit mid-write; RAM
admission; no `work_pending` involvement.

## Rung 1 — MVP: Export as PNG

- `Action::Export` (default `E`, pending confirmation) + File ▸ Export… (`⇧⌘E`) + context menu → the
  request is **frozen** → save dialog (*3.6*) → write worker.
- Enabled for stills, archive entries, animations (frame 0 until 0a), Live Photo stills; disabled
  for doors, CMYK JPEG, and video until 0a.
- Suggested name `<stem>.png` (video `<stem>-0h12m34s.png`), avoiding existing files (checked off
  the event loop) and pending destinations.
- Colour per *3.7* (sanitized profile, Strip on — the only mode in rung 1) and *3.8* (HDR →
  P3 SDR, toast says so). No other metadata.
- Toasts: "Exported name.png" / "Export failed — reason"; Reveal if the toast supports an action.
- `last_export_folder` + ADR-022 addendum; macOS panel-key cleanup for the save panel.
- **PNG default** chosen from corpus numbers (`png` levels × filters vs `oxipng` 1–3): encode
  p50/p95, bytes/MP; target 24 MP p95 around 1–2 s (provisional).

**Acceptance.** A P3 HEIC → PNG matches Preview/Photos/a browser and Blaze; an sRGB AVIF exports
sRGB with unchanged samples; 16-bit round trip (container inspection); rotation baked;
navigation mid-export doesn't cancel it; **nav p95 holds** under the manual protocol (*Part 5*);
no recents/MRU entries (Windows registry checked by hand) and no `NSOSPLastRootDirectory` left on
macOS; exFAT / SMB / TCC hand checks pass.

**Perf.** `ExportFile` from **dialog confirm**, split into queue, decode, prepare, encode, write.

## Rung 2 — The Export mode panel

**Progress (session 7, 2026-11-24) — Custom… is in, on both shells:** `SizeChoice` gained
`Width` / `Height` / `Percent` beside `LongEdge` (pb-encode's `Resize` too, all downscale-only);
`SizeChoice::code` keeps the kind in the top four bits so a bare long edge is still its own number;
prefs write `size = "width 1500"` (`"long 2048"`, `"height 800"`, `"50%"`, `"full"`) and still
read rung 2's first `long_edge`. **Custom…** goes through the dialog seam the plan named:
`AppCore::export_custom_size` → `ShowDialog(DialogKind::ExportSize)`, the shell pulls
`export_size_dialog()` and answers `DialogResult::ExportSizeChosen`; the result line as you type is
`SizeDialog::readout` (pure, shared). macOS: a native sheet (`ExportSizeSheetView`, Return = OK,
Esc = Cancel) from a "Custom…" entry at the foot of the Size pop-up; winit: a "Custom Size" dialog
window (`dialog::export_size_dialog`). **The pre-export HDR note** now comes from the live preview
(rung 3): once a preview is made, the panel says "This HDR image is exported as SDR (Display P3)."
**Rung 2 is complete (2026-11-25).** **Metadata (§ 3.7):** the library half (an agent, merged
94ca325c): `pb_decode::read_export_metadata` reads the source's EXIF / PNG generation text,
`pb_encode::prepare_with_metadata` allowlists typed tags into a fresh block (Orientation = 1;
never MakerNote, serials, UserComment, thumbnails), and every encoder embeds it (`writes_metadata`).
The app half: `KeepMetadata` on `ExportSettings` (camera + copyright on, location off at every
entry and never persisted, generation PNG-only, the original profile opt-in), both workers pass
`keep.choice(strip)`, the preview reports what the source carries so the panel greys the rest,
and the note says what's kept. End-to-end test: `unchecking_strip_copies_the_chosen_metadata`.
**Also done 2026-11-25 (Mac session):**
- **The `[`/`]` menu context:** in the mode, View ▸ Slideshow Faster / Slower read Lower /
  Higher Quality (Compress Faster / Smaller for PNG; greyed for lossless and TIFF). They step it
  from the menu too: `MenuState::step_keys` and `AppCore::export_menu`; the action arms are
  re-meant in the mode.
- **View ▸ Export Mode:** a check item on both shells.
- **macOS Tab focus (Q9):** with Full Keyboard Access on, Tab moves focus into the panel
  (`ExportFocus` / `@FocusState`, mirrored to `CoreModel.exportPanelFocused`). While a control
  has it, the key monitor hands keys to the control, still forwards key-ups (the release net),
  and Esc returns focus to the photo instead of quitting. Without FKA, Tab hides panels as
  before. winit: pointer-only as documented.

**Progress (session 6, 2026-11-14) — Export mode is in, end to end, on both shells:**
- **Core** — `export_panel.rs` (pure: `ExportSettings` = format / JPEG quality / PNG compression
  / size / Strip; `ExportPrefs` = the persisted part, `[export]` in settings.toml, read **field
  by field** so one bad value falls back alone (F16); `SIZE_PRESETS` 3840…800 long edge, greyed
  at or above the source's long edge (F20); the `ExportPanel` view model + plain-language notes;
  `PanelKey`, the cheap per-tick change key; and **the contextual key layer**
  `export_key_override` — today `[`/`]` = quality, and the pinned `Enter`/`Esc` revisit is one
  more arm there). `app_core_impl/export_panel.rs` (enter/leave, setters, commit, prefs saved
  when the mode closes / at Export… / at quit — never per slider tick; `export_menu_enabled`).
- **Slot model** — `Panels.export`: the mode covers the Inspector without closing it (leaving
  shows the tab that was open), Help overlays it, an Inspector key leaves it (and shows that
  tab), `Tab` suspends it (panel and keys), `E` while hidden reveals.
- **Actions** — `Action::ExportMode` (`E`, new) toggles; `Action::Export` (File ▸ Export…,
  ⇧⌘E; keys **⌘↩** on macOS, **Ctrl+Enter** elsewhere — never Win+Enter, Narrator) enters the
  mode, or exports in it. Requests freeze `ExportSettings`; the worker runs `dyn Encoder` with
  `PrepareSpec { caps, resize, strip }`; `ShowSavePanel` carries the extension (`png`/`jpg`) and
  both dialogs allow only that type; `.jpeg` is accepted for JPEG.
- **Shells** — macOS: `ExportPanel.swift` (native segmented pickers, a stepped slider, a
  pop-up with disabled presets, a checkbox, a prominent Export… button) in the Inspector's
  top-trailing slot, over per-call FFI getters/setters (`export_refresh` + `export_*`). winit:
  `panels_ui::export_panel` (pb-ui combos, `slider_stepped`, `toggle`, `primary_button`) via
  `PanelAction::Export*`.
- **Mode × action rows tested** (one test each): entering stops a slideshow and `[`/`]` become
  quality; navigation keeps the mode and settings; `Tab` suspends; a door says why and refuses;
  Help overlays; an Inspector tab leaves and leaving restores it; `Esc`/`Enter` unchanged. Plus
  an end-to-end JPEG export (rotated, 2048 long edge, `.jpg` enforced).
- **Not yet (rest of rung 2):** the metadata allowlist + categories when Strip is unchecked —
  **today unchecking Strip only keeps the original colour-profile bytes** (no EXIF is written
  either way); the *Custom…* size dialog; the HDR note before export (the toast still says
  "HDR as SDR" after); compare-pin suspension (rung 3, with `Y`); menu-hint context for
  `[`/`]` (the Slideshow Faster/Slower items still advertise them while the mode re-means
  them); a View-menu "Export Mode" check item; macOS Tab-focus into the panel (Q9).

**Progress (session 2, 2026-11-14) — the pb-encode core is in; nothing user-visible yet:**
`encoder.rs` (`Encoder` trait + `Caps` + `ExportFormat { Png(effort), Jpeg { quality } }`),
`JpegEncoder` (8-bit, no alpha, profile embedded via `image`'s `set_icc_profile`; 4:2:2),
`PngEncoder`, and `prepare` gained `caps` (flatten onto `flatten_onto` — output-space encoded,
composited in encoded space — and 16 → 8-bit narrowing) and `resize: Option<Resize>`
(`LongEdge` / `Fit`, **as displayed** — the box swaps for a quarter turn; downscale-only; Lanczos3 in
**linear light** with premultiplied alpha, via `fast_image_resize` on borrowed f32 buffers).
`PrepareSpec::full(rotation, strip)` is rung 1's spec; the export worker still writes PNG through
`encode_png` directly — **switching it to `dyn Encoder` is the first step of the wiring**. Corpus:
`export_bench -- <folder> --jpeg` sends all 31 files through JPEG q85 at a 2048 px long edge
(p50 ≈ 8 ms for 1 MP sources, 120–390 ms for 20–36 MP; P3 keeps its profile, HDR tone-maps, alpha
flattens). Still to do in rung 2: 2.0 input routing, the view model + `export_set_*`, the mode ×
action table, the metadata allowlist, both shells' panels.

**2.0 Input routing** (*3.11*): `ExportCommit`, `E` toggle, `Esc` unchanged, `[`/`]` contextual,
`Tab` suspend, optional macOS Tab-focus arbitration; tests for the release net in every focus state.
Build the Export-mode bindings as a **contextual keymap layer** so the pinned `Enter`/`Esc` revisit
(*Part 8*) is a one-line default change.

**Design.**

- `SlotContent::Export` wins the right-hand slot (Help can overlay); leaving restores the previous
  Inspector tab.
- **The `Encoder` trait + `Caps` arrive here**, with the JPEG encoder as the second implementation.
  **Resize** (presets + Custom…) and **flatten** join `prepare`.
- **Panel:** format (PNG, JPEG); quality/effort; resize; ☑ Strip metadata + categories; HDR note for
  HDR sources; **details** (source → output dims, codec, size, colour space, HDR, location present,
  provenance notes); Export… button; ✕.
- The view model in the core; edits via `export_set_*` methods; prefs per *3.9*.
- **The mode × action table** (*3.11*) — one test per row.
- **Metadata:** the typed EXIF allowlist and embedding (*3.7*).
- **macOS:** SwiftUI, native controls, VoiceOver — **run `/mac-arsed-mac-app` first**. **Windows:**
  pb-ui components only (add a checkbox component if missing).

**Tests (first).** View model per source kind × format; Location resets; allowlist fixtures (GPS in
EXIF, XMP, IPTC, PNG text; generation payloads); orientation = 1; prefs compatibility; the mode ×
action rows.

**Acceptance.** Enter, navigate, rotate, change format/quality and export with the keyboard on both
shells; the rung's notes list exactly which controls are pointer-only on winit; exported metadata
matches the chosen categories (`exiftool`); legible in light and dark.

## Rung 3 — Live preview + flick

**Progress (session 7, 2026-11-24) — built, core + both shells; the Mac GUI not driven, winit
compile-checked only.** In Export mode the photo **becomes the export** a moment after the screen
settles; `Y` flicks to the original; the panel shows the exact **File size**.
- **Present gate (F3), as a cover rather than a gate.** The renderer has two **aux** textures
  (`upload_aux` / `present_aux` / `dismiss_aux` / `aux_presented` / `clear_aux`, modelled on the
  stash). On macOS a native video layer above Metal is hidden while a preview covers.
  Presenting one *covers* whatever is presented and remembers it (`UnderAux`, the ring
  texture held by `Arc`); `dismiss_aux` uncovers it exactly; any other present supersedes it. The
  core decides once per frame in `AppCore::draw` (`export_preview_target`) and covers again after
  any of the viewer's own re-presents — so none of the seven re-present sites needed touching. A
  `derive_fit(Held)` is refused while an aux is up (it would derive the item's Fit from the
  preview). The aux VRAM joins the stash in `sync_stash_external` (one sum).
- **Both sides from the export pipeline (F2).** Original = `prepare` with `Caps::ALL`, no resize;
  export = the encoded bytes read back with the display decoder (`decode_bytes`) — **except PNG**,
  which is lossless, so its prepared pixels are shown and the decode-back skipped. Both upload
  full-size and **mipped, profiled or not** (`create_image_texture`'s new `mip_icc`: the mip
  shader's sRGB curve is exact for sRGB/P3, close for others — pb-encode's resize makes the same
  trade), so the flick differs only by codec and resize.
- **Deviation: unrotated textures, not "presented at R0".** The export bytes have the rotation
  baked in (F10 holds); the worker turns the decoded side back to the source's orientation and
  both draw under the normal view transform. Pan/zoom/clamp math is untouched that way. A resized
  export is laid out at the source's size (F20's "shown at source size", with a note).
- **Worker** (`export_preview::PreviewWorker`): one thread, latest-wins with a generation checked
  between stages; RAM-only caches — the decoded source of the item on screen, and encoded files
  within 192 MB (going back to a setting reads the file back instead of encoding it). Dropped —
  caches and all — when the mode closes; the aux textures freed too. Not its own rayon pool: no
  stage needs one yet.
- **Scheduling:** a cheap per-tick `TickKey` (`content_gen`, item, frame, rotation, the view's
  extra turns, settings — no allocation); a request goes once it has held for **120 ms**, so a
  slider drag or blazing makes none. The full `PreviewKey` (path, or container + name, + frame +
  `content_gen` — never the index) is built only then. While new settings encode, the last preview stays up and the panel
  says "Calculating…". Polled at 16 ms through the wake list, never `work_pending` (F8).
- **Flick:** `export_key_override` maps `CompareToggle` (`Y`) to `ExportKey::Flick`; key-up comes
  through `CoreEvent::KeyUp`; `FocusLost` resets. The menu's Compare with Pinned toggles in the
  mode; `⇧Y` (pin) is off there with a toast. **Deviation (F14):** press times are receipt times
  (`Instant::now()`), not shell event timestamps. The panel's **Preview** control (Export |
  Original) is a toggle — no pointer hold-to-peek. The "chip" is a toast ("Original" /
  "Export (412 KB)").
- **F10:** Export… with the settings on screen writes the preview's bytes as they are
  (`PreEncoded` through `ExportRequest` / `ExportJob`; the worker skips to the commit).
- **Measured** (`cargo run --release -p pb-app-core --example preview_bench -- <folder>`, the
  31-file corpus, M-series Mac): first preview of an item at PNG Balanced p50 **235 ms**, p95
  **2.2 s** (the 36 MP NEF); a setting change, JPEG 85 full size, p50 **14 ms**, max **292 ms**;
  JPEG 85 at 2048, p50 14 ms, max 445 ms (the resize dominates). PNG compression levels (fresh
  `export_bench`): Faster p50 12 ms / p95 109 ms at 1.82 MB/MP; Balanced 155 ms / 1.74 s at
  1.37 MB/MP; Smallest 454 ms / 4.0 s at 1.36 MB/MP (the NEF: 6.5 s for −3 %).
- **Tests:** the spring (tap, hold, ownership, reset), sizes, turning back; core: settles then
  covers (and the gate re-covers after a viewer present), `Y` tap / panel / menu, stale preview
  kept while the next encodes, navigation uncovers and re-previews, `Tab` suspends and leaving frees
  everything, Export… writes the previewed bytes byte for byte, doors/empty deck have none.
- **Independent review (session 7), fixed before commit:** (1) the source key used the item's
  *name*, which a folder listing gives as the bare file name — two `IMG_0001.JPG` in a recursive
  scan shared a preview, and Export… could write the other's bytes; it's the **path** now (plus
  container + name for archive entries). (2) `[` then `]` within one encode let the abandoned
  quality's result land over the one on screen and stick at "Calculating…" — going back now
  cancels the job in flight. (3) A result dropped while you were on another photo was never asked
  for again on return — dropping it now clears `asked`. (4) A new texture behind the slot already
  on screen didn't draw on macOS (no target change) — landing draws. (5) `R` on a 3D model turned
  its preview (the capture ignores R, the view doesn't) — both sides are turned back by the view's
  extra turns (`display_turns`). (6) **macOS:** a paused MP4/MOV on the AVPlayer route sits in a
  layer above Metal, hiding the preview — the host now hides that layer while a preview covers
  (`export_preview_covering` → `MetalCanvasNSView.setVideoCovered`). (7) The per-tick key held
  the deck's `Arc` address, so every streaming-scan swap uncovered the preview and reset the flick
  — `content_gen` + item suffice (every swap that changes what an index means bumps it). (8)
  `dismiss_aux` could restore peak 1.0 over the constructor's HDR image — the recorded peak now
  starts from it. (9) A dead worker left "Calculating…" forever — `drain` reports it and the core
  starts over. Regression tests for 1–3.
- **Not done:** `PB_PERF` episodes `ExportPreview` / `ExportFlick` (only per-stage timings print);
  ~~the export-at-its-own-1:1 toggle (F20)~~ (done 2026-11-25: "Show at export size" lays
  both flick sides out at the export's own size; `PreviewState::own_size`, the draw re-presents
  on a new layout); speculative ±5 encodes; viewport-first encoding; a
  dedicated chip; compare-pin *suspension* (it's refused in the mode instead).

**3.0 Prerequisites.**

- **The present gate** (F3): one core field for what's presented (ring rep / stash / export A /
  export B) and one gate that every re-present site goes through — refusing while an export side is
  presented. Test: resize, zoom across 1.0, a sharpen landing and an Original upgrade while a preview
  is shown — the preview stays.
- **Two aux textures** modelled on the stash slots, with a full identity guard, the display-geometry
  override, mips for every colour mode, and the dimension clamp. **External VRAM:** replace
  `sync_stash_external` with one "sum all external holders" function and charge the aux textures
  there — **no ring-capacity change** (rebuilding the ring would re-decode neighbours).
- **The preview worker** with its own rayon pool.

**Design.**

- **Both flick sides come from the export pipeline** (F2): **A** = the prepared image before resize
  and encode; **B** = the decode-back of the encoded bytes, presented at A's geometry. Same upload
  path, same mips, same colour mode — the visible difference is exactly codec + resize. Works for
  RAW, SVG, oversized images and frames, which have no ring Original.
- **Preview bytes are file bytes** (F10): prepared **with** rotation baked and the metadata block
  included, presented at R0. Export… with the shown settings writes the cached bytes; the size
  readout is exact. `R` re-prepares and re-encodes.
- **Identity:** (`SourceRef` + item key, frame, settings hash, prepare revision) — travels through
  upload, presentation and readouts.
- **Flick:** the spring machine (*3.11*); the panel button with event timestamps; a chip shows
  which side ("Source" / "PNG · 412 KB").
- **Caches** (RAM-only, budgeted): the decoded `ExportSource` for the current item (a setting change
  never re-decodes — vital for RAW); encoded bytes per identity (LRU). Returning to a cached setting
  is a **re-upload** of B, measured as its own case — not a rebind (F21).
- **Resized previews:** a readout says "Shown at source size — export is W×H", plus a toggle to view
  the export at its own 1:1 (F20).
- Scheduling: debounce slider drags (~100 ms or on release, measured); latest-wins; no encodes while
  blazing in the mode.
- Deferred until measured: speculative ±5 encodes; viewport-first encoding.

**Tests (first).** Present-gate scenario; spring transitions; identity rejects stale uploads after
rotate/seek/settings/navigation; geometry for every rotation × resize rounding × fit/fill/1:1 × pan;
caches; reduced-preview state.

**Perf.** `ExportPreview` (setting change → present call) and `ExportFlick` (key-down → present
call) for cold, encoded-cache and resident cases, p50/p95/p99. No numeric preview target is designed
around until measured (F23).

## Rung 4 — Codec breadth

| Codec | Candidate (licences checked on crates.io, 2026-11-13) | Notes |
|---|---|---|
| JPEG (better) | `mozjpeg` (IJG) / `mozjpeg-sys` (IJG AND Zlib AND BSD-3) | nasm |
| WebP (lossy) | `webp` (MIT OR Apache-2.0) / `libwebp-sys` (MIT) → libwebp (BSD-3) | C |
| AVIF | `libavif` / `libavif-sys` (BSD-2) + libaom (BSD-2 + AOM patent licence) or rav1e; `ravif` (BSD-3) | cmake; vcpkg |
| **JPEG XL** | **`jxl-sys`** (MIT OR Apache-2.0, libjxl 0.12) + our thin safe wrapper in pb-encode; optional **`zune-jpegxl`** (MIT OR Apache-2.0 OR Zlib) for pure-Rust lossless. **Never `jpegxl-rs` / `jpegxl-sys`** (GPL-3.0-or-later); not `jxl-encoder` (AGPL/commercial) | Audit libjxl's chain: highway, brotli, skcms/lcms2 |
| HEIC (macOS) | Image I/O | system |
| PNG (smaller) | `oxipng` (MIT) | pure Rust |
| Score | `ssimulacra2` (BSD-2) | pure Rust |

- Whole-chain audit (`cargo about` + native libs + release linkage) before each lands;
  `THIRD-PARTY-NOTICES.md`; amend CLAUDE.md's "We only ever DECODE" when the first encoder ships.
- Each runs inside the worker-owned rayon pool with a thread cap; per-codec options, `Caps`,
  colour/EXIF embedding, cancellation; defaults tuned with `ssimulacra2`.

**Session 8 (2026-11-24, Mac) — WebP, AVIF, JPEG XL, HEIC and TIFF built.** Owner ask: "work
through a batch of them". mozjpeg and oxipng are not in this batch.

- **pb-encode.** `ExportFormat` names every format whatever the build carries; `Codec::available`
  says what was compiled in, and `encoder()` refuses the rest with `EncodeError::Unavailable`, so
  pb-app-core and the shells need no `cfg`. `Encoder::lossless()` lets the preview show prepared
  pixels instead of reading a lossless file back. `Caps` gained `gray`: WebP, AVIF and HEIC have
  no gray layout, so `prepare` widens gray to RGB and turns a gray profile into an RGB one with the
  same curve (`icc::rgb_for_gray`; neutral pixels render identically).
  - **TIFF** (`tiff` 0.11, already linked by `image`): 8/16-bit, gray/RGB, alpha as an unassociated
    ExtraSamples tag (written directly: the crate's `extra_samples` counts a fifth sample), ICC as
    an `UNDEFINED` tag, LZW + horizontal predictor. Extension `.tif`.
  - **WebP** (`libwebp-sys` 0.14.4 → vendored libwebp 1.6.0, built with cc, pre-generated bindings,
    NEON on Apple Silicon): lossy (method 4, sharp YUV, alpha quality 100) or lossless (`exact` on,
    so it decodes to exactly the prepared pixels). ICC via libwebp's own mux (`ICCP`, `VP8X`).
    16383 px limit: the panel says so and disables Export… (`FormatChoice::max_edge`).
  - **AVIF** (`ravif` 0.13 / rav1e 0.8.1, pure Rust; `avif-asm` adds rav1e's assembly — NEON via cc
    here, nasm on x86-64). 10-bit 4:4:4 full range. **ravif can't embed an ICC** (avif-serialize
    writes only nclx, and omits `colr` entirely for its defaults), so `avif::add_icc` edits the
    finished file as libavif writes one: a `colr prof` property inserted before any nclx (in `ipco`
    *and* in the primary item's `ipma` order — the first draft got the association order wrong;
    a test pins it), the nclx's primaries/transfer set to 2 (unspecified), `iloc` re-pointed. Tested
    structurally and end to end: Image I/O reads a P3 export as P3 red (pb-decode
    `blazes_avif_and_heic_exports_keep_their_profile_and_alpha`). Speed 6, half the cores.
  - **JPEG XL** (`jxl-sys` 0.1.12 → vendored libjxl 0.12.0, cmake + bindgen; never `jpegxl-rs` /
    `jpegxl-sys`, GPL): lossy (libjxl's quality → distance; quality 100 clamps to its smallest lossy
    distance, 0.05 — distance 0 means "switch to lossless", refused with the lossy setup) or
    lossless; 8/16-bit, gray, alpha; effort 7, half the
    cores. `jxl::decode_rgba8` (libjxl, into the file's own space) for the preview — below.
  - **HEIC** (macOS only, Image I/O through a hand-rolled FFI like `pb-decode/imageio.rs`): 8-bit +
    alpha, profile via the `CGImage`'s colour space. Image I/O quantizes quality to a few steps
    (65 = 70, 80 = 85 give identical files).
- **pb-decode.** A `colr` ICC now outranks an nclx in the same file (libavif's reading; the old
  first-box-wins scan read our AVIFs — and libavif's — as "unspecified").
- **pb-app-core.** `FormatChoice` grew WebP / AVIF / JXL / HEIC / TIFF; `FormatChoice::offered()`
  is what the panel lists. Per-format quality (`webp_quality`, … — the scales differ) and
  `webp_lossless` / `jxl_lossless`, all persisted; a saved format this build lacks starts as PNG.
  `[`/`]` step the current format's quality (nothing while lossless or for TIFF). The preview's
  lossless shortcut is `encoder.lossless()`. Features `export-codecs` (WebP + JPEG XL) and
  `avif-asm`, forwarded by pb-mac-ffi and pb-app.
- **Shells.** Format is a pop-up (grouped: PNG, JPEG · WebP, AVIF, JPEG XL, HEIC · TIFF); a Lossless
  checkbox (Mac) / switch (egui) under it for WebP and JPEG XL; Lossless **greys Quality out rather
  than removing it** (no layout shift — the owner's ask from the File size spinner); the save panel
  / dialog names and filters each format. `build-swift-host.sh` always builds the codecs (hard-fails
  without cmake); `build-windows.ps1` adds them (CMake needed; `-NoCodecs` to skip; `avif-asm` when
  nasm is on PATH, x64 only).
- **Colour bugs found by measuring** (each pinned by a test):
  - **moxcms 0.8.1 writes its Bradford cone matrix as `chad`** in every RGB profile it constructs
    (fixed upstream in 0.9). Most engines ignore `chad` for matrix/TRC profiles, which hid it;
    jxl-oxide recovers primaries through it and read our P3 red at xy (0.573, 0.422). `icc::fix_chad`
    corrects every profile we build. Profiles copied from files keep their own (correct) `chad`.
  - **`profile_for_cicp` labelled every profile "sRGB IEC61966-2.1"** (moxcms starts from sRGB's):
    a P3 or BT.2020 export showed as sRGB in Get Info. Now "Display P3" when it is, else "RGB
    profile".
  - **`display_p3_profile()`** (HDR tone-map output) came from moxcms's `new_display_p3`, whose
    slightly-off D50 white and curve skcms can't name. Now built from the P3 code points — skcms
    names it P3 / D65 / sRGB, like Apple's.
  - **Viewing lossy wide-gamut JPEG XL** is wrong in Blaze (task #156): jxl-oxide gamut-maps through
    sRGB on the way to P3 (P3 red → (216, 55, 55)). Export mode's preview reads lossy JPEG XL back
    with **libjxl** instead (`export_preview::read_back_jxl`, which asks for the file's own space —
    libjxl would otherwise return clipped linear sRGB), so the preview is the file. The viewer
    itself is an owner call (#156).
- **Default qualities** (`examples/codec_bench.rs`, the 23 distinct images in the corpus at 2048 px,
  8-bit, Strip on; read back as the preview reads them; SSIMULACRA2 in linear sRGB): each format's
  grid setting whose **median** score is closest to JPEG 85's (83.6 at 1.41 bits/px):

  | Format | Default | Median | p10 | bits/px | vs JPEG 85 |
  |---|---|---|---|---|---|
  | JPEG | 85 | 83.6 | 80.9 | 1.414 | — |
  | WebP | 90 | 83.7 | 80.5 | 1.276 | −10 % |
  | AVIF | 85 | 80.6 | 79.1 | 0.759 | −46 % |
  | JPEG XL | 85 | 82.5 | 75.6 | 0.886 | −37 % |
  | HEIC | 90 | 84.1 | 65.6 | 1.832 | **+30 %** |

  HEIC's tail (p10) stays low at every quality: it is 4:2:0, which smears saturated edges (the
  RGB-CMY chart, a logo), and Image I/O's HEIC is larger than JPEG at equal quality. WebP had the
  same tail (73.3) until the review found sharp YUV wasn't actually on (lossy input was imported as
  YUV); with it, 80.5 — at −10 % rather than −16 %, and ~45 % slower to encode.
- **JPEG encoder trial (2026-11-25, owner: "research and compare POC implementations").** Three
  permissive candidates (jpegli-rs and zenjpeg are AGPL; out). Each ran in its own example binary
  against the current encoder (`image` crate, baseline **4:4:4**, not 4:2:2 as its doc said) on
  the 23-image corpus at 2048 px; file size at the same median SSIMULACRA2 as the current
  encoder's q70 … q95:

  | Encoder | q70 | q75 | q80 | q85 | q90 | q95 | ms @ 2048 px |
  |---|---|---|---|---|---|---|---|
  | **jpegli** 0.1.0 (C++, libjxl 0.10.2's; defaults: 4:4:4, adaptive quant, **sequential** — see the correction below) | −11.7 % | −8.6 % | −9.0 % | **−8.3 %** | −9.6 % | −22.1 % | **5** (current 8) |
  | mozjpeg 0.10.13 (C), 4:2:0 | −11.3 % | −7.9 % | −2.7 % | −1.1 % | −1.3 % | — | 40–80 |
  | mozjpeg (C), 4:4:4 | −9.4 % | −1.0 % | +0.9 % | +1.0 % | +0.6 % | — | 50–110 |
  | mozjpeg-rs 0.9.2 (pure Rust), 4:2:0 | −10.9 % | −7.5 % | −2.7 % | −0.8 % | −0.5 % | — | 30–60 |
  | mozjpeg-rs, 4:4:4 | −6.3 % | +2.3 % | +2.8 % | +2.7 % | +2.2 % | — | 40–80 |

  - **mozjpeg isn't worth adding:** about nothing at the default q85, and its 4:2:0 default drops
    the weakest images (p10 73 vs 81 at q85: saturated edges). The pure-Rust port matches the C
    library, so it would be the one if ever.
  - **The `mozjpeg` crate's `write_icc_profile` numbers ICC chunks from 0** (ICC.1 starts at 1), so
    decoders drop the profile: every wide-gamut photo scored 55–80 until the trial wrote its own
    APP2 markers. Whoever wires a libjpeg-API encoder: number from 1.
  - **jpegli wins but isn't shippable as packaged (task #157).** jpegli-sys builds its own libjxl
    0.10.2 + **Highway 1.1.0**; its `-L` comes before jxl-sys's, so `-lhwy` resolves to 1.1.0 for
    libjxl 0.12 too (built against 1.2.0). It links and runs (a JPEG XL round trip beside jpegli
    passed), but the ARM SVE / NEON_BF16 target bits moved between 1.1 and 1.2, so libjxl could
    dispatch to the wrong target on SVE hardware (Apple Silicon and x86 are unaffected: bits
    equal). jpegli itself only imports `hwy::AllocateAlignedBytes` / `FreeAlignedBytes`
    (identical). The fix is a jpegli built against the same Highway 1.2.0 as jxl-sys. **This
    release keeps the current encoder.**
  - **Built (2026-11-26, task #157): `crates/pb-jpegli`.** The owner chose to do it before the
    release. jpegli's encoder (libjxl tag v0.10.2, 17 files) and a C++ shim are compiled with
    `cc` against a copy of **jxl-sys's own Highway headers** (1.2.0, `457c891`); no Highway is
    built or linked there, so jpegli's calls resolve against jxl-sys's libhwy and the app has one
    Highway. jxl-sys is pinned `=0.1.12` and `build.rs` fails on a Highway version mismatch;
    `vendor/README.md` + `refresh.sh` reproduce the tree byte for byte. The shim runs a whole
    encode behind a C ABI: `error_exit` throws a C++ exception the shim catches (jpegli is built
    with exceptions on, unlike libjxl's own build), the output goes to a `std::vector`, and the
    bytes reach Rust through one callback, so no libjpeg struct or Rust panic crosses the FFI.
    `JpegEncoder` uses it under pb-encode's `jpegli` feature (in `export-codecs`); without it
    (Linux, plain builds) export keeps the `image` crate's encoder, now `encode_jpeg_baseline`.
    Describe's upload JPEG stays on the `image` crate everywhere.
  - **Corrections to the trial:** (1) it ran jpegli **sequential**: the library's default
    progressive level is 0, not 2; its "5 ms" is the sequential time. (2) "jpegli only imports
    two Highway functions" held only for that build on Apple Silicon, where Highway 1.1 compiled
    one target and dispatch collapsed to a static call. Built against 1.2.0's headers, jpegli
    also imports `hwy::GetChosenTarget` and `SupportedTargets` (it uses `HWY_DYNAMIC_DISPATCH`),
    and on x86-64 it always would have: the shared-Highway build was the right fix, not a nicety.
  - **Progressive level, measured** (`codec_bench`, same corpus and settings; the baseline's
    rows reproduce rung 4's 83.58 at 1.414 bits/px exactly). Scores are identical across levels
    (the quantization doesn't change), only the entropy coding:

    | jpegli | q85 bits/px | vs baseline q85 | ms @ 2048 px |
    |---|---|---|---|
    | level 0 (sequential, the library default) | 1.259 | −11 % | 5 |
    | level 1 | 1.079 | −24 % | 10 |
    | **level 2 (shipped; cjpegli's default)** | **1.071** | **−24 %** | 12 |
    | `image` crate baseline | 1.414 | — | 7 |

    jpegli's grid at level 2: q70 74.9 / 0.758 · q75 76.7 / 0.827 · q80 79.2 / 0.923 ·
    q85 82.3 / 1.071 · q90 85.9 / 1.485 · q95 90.4 / 2.038 (median SSIMULACRA2 / bits/px).
    Interpolated to the baseline 85's score (83.6), jpegli needs ~1.22 bits/px: **−14 % at equal
    quality** (the trial's sequential −8 %, plus progressive's entropy coding).
  - **The default stays 85:** by the rule every default follows (the grid setting whose median
    is closest to the baseline JPEG 85's 83.6), jpegli 85 (82.3) beats 90 (85.9). So the
    owner's saved setting keeps its meaning, and a default export is 24 % smaller at a score 1.3
    lower — still above JPEG XL 85's 82.5. With jpegli as the JPEG, WebP 90 is +19 % of JPEG
    85's size rather than −10 %.
- **Independent review** (Codex, gpt-6.2-astra at xhigh, read-only, of b0e0c01d): no
  use-after-free, leak, ABI or buffer-size problem in the WebP / libjxl / CoreFoundation FFI, and
  `add_icc` verified against avif-serialize's opaque and alpha layouts. Twelve findings, all fixed
  in the follow-up commit, each with a test where one applies:
  1. **A TIFF export could replace a file the dialog never approved:** the Mac's panel accepts
     `.tiff`; the core appended `.tif`, and the worker replaced an unrelated `x.tiff.tif`. Now the
     format's aliases (`.tiff`, `.jpeg`/`.jpe`, `.heif`/`.hif`) are its own, and an appended
     extension sets `ExportJob::may_replace = false` — `commit_file` refuses an existing file there
     (`WriteError::Exists`). (`WriteError`'s texts also lost their em-dashes.)
  2. **Gray + alpha TIFF kept a GRAY profile on RGBA samples** — now converted like `Caps.gray`.
  3. **The ICC-outranks-nclx scan crossed items** (a thumbnail's or alpha plane's profile could
     colour the photo; an sRGB profile was skipped for a wide-gamut nclx). `isobmff` now resolves
     the **primary item's** `colr` through `pitm` → `ipma` → `ipco` (a profile wins within the
     item; sRGB is an answer); the byte scan is only the fallback for an unwalkable `meta`.
  4. **rav1e's assembly carries dav1d's BSD-2 notices** (VideoLAN, Two Orioles, Martin Storsjo, …)
     — now `licenses/rav1e-asm-NOTICES.txt`, generated from the 93 files' own headers.
  5. **A default Linux build (no libheif) can encode AVIF but not read it back**, so it can't be
     previewed there (the export still works); the end-to-end test now probes each format's
     read-back and skips what this machine can't decode.
  6. **Gray profiles displayed as sRGB** (pre-existing, preview and viewer): `ColorTransform` now
     uses a gray profile's own curve (identity matrix); sRGB-like curves stay a passthrough.
  7. **WebP's sharp YUV was never on** (lossy input imported as YUV) — now ARGB; re-measured above.
  8. **AVIF with alpha doubled its thread budget** (ravif encodes colour and alpha at once, each
     with the count) — split when there's alpha.
  9. **⌘↩ / Ctrl+Enter bypassed the WebP size limit** the disabled button showed —
     `ExportSettings::size_limit` now gates `export_commit` too.
  10. **Tuning one format evicted another's cached preview** — the preview keys on
      `ExportSettings::normalized()` (only what shapes the current format's file).
  11. **`build-windows.ps1`:** the codecs no longer hang off `-NoNative` (they build from source,
      not vcpkg), and `avif-asm` needs NASM ≥ 2.15 (rav1e's floor), not just any nasm.
  12. **The CHANGELOG promised WebP / JPEG XL on Windows** (the release script doesn't build them):
      now "Mac-only in this release". libjxl's smallest lossy distance is **0.05**, not 0.01.

  Codex's two macOS runtime notes (the HEIC test failing at `CGImageDestinationFinalize`, the
  Image I/O read-back seeing zeros) came from its sandboxed process; both pass in a normal run.
- **Encode times at full size** (what the preview waits for; `codec_bench --timing`, the same 23
  images, M5 Ultra, release, the codecs at half the cores):

  | Encode | p50 | p95 | max (36 MP NEF) | |
  |---|---|---|---|---|
  | JPEG 85 | 8 ms | 131 ms | 174 ms | |
  | HEIC 90 | 25 ms | 72 ms | 122 ms | Apple's hardware encoder |
  | JPEG XL 85 (effort 7) | 36 ms | 513 ms | 866 ms | effort 5: −45 %; kept 7 |
  | JPEG XL lossless | 72 ms | 749 ms | 984 ms | |
  | WebP 90 | 83 ms | 1.5 s | 2.0 s | sharp YUV; 1.1 s without |
  | AVIF 85 (speed 6) | 96 ms | 1.2 s | 2.3 s | speed 7/8 only ~25 % faster; kept 6 |
  | TIFF | 34 ms | 529 ms | 773 ms | |
  | WebP lossless | — | — | 7.1 s (effort 50) | 9.6 s at libwebp's default 75 |

  **WebP lossless effort is 50**, not cwebp's 75: on the two big images it makes the same file
  (7.46 vs 7.47 and 1.84 vs 1.84 bits/px) in two-thirds of the time; 25 is faster but 9 % bigger.
  It is still the slow one — JPEG XL lossless is 4–7× faster and 8–12 % smaller on those two. AVIF, the one we
  feared, lands in two seconds on 36 MP with rav1e's NEON assembly.

## Rung 5 — HDR export

- **Prerequisites:** #146 (luminance + HLG audit — macOS half done 2026-11-14); `cICP` PNG
  read-back (**done 2026-11-14**, `pb-decode/src/png_hdr.rs`); HDR JPEG XL decode; #145 only for
  gain-map sources.
- ⚠ **`png` 0.18 reads `cICP` / `mDCV` / `cLLI` but its encoder never writes them from `Info`**
  (checked in 0.18.1's source). Write them as raw chunks with `Writer::write_chunk` after
  `write_header` and before the image data — `png_hdr.rs`'s test helper does exactly this.
- **Outputs:** 16-bit PQ/HLG PNG (`cICP` + `cLLI` (+ `mDCV`)); AVIF 10/12-bit PQ/HLG + `clli`;
  gain-map AVIF; JPEG XL; gain-map JPEG (libultrahdr, audited, or Image I/O on macOS); HEIC on macOS.
- *3.8* Rules 1–4; HDR menu (Keep / SDR fallback / Tone-map) greyed per format; "Preview as SDR".
- **Tests:** PQ and HLG round trips; light-level algorithm; gain-map round trips. **Acceptance:** HDR
  in Blaze and in a second HDR-aware viewer.

## Rung 6 — Batch

- "All items in this folder" first; multi-select is a separate design.
- "Export all N…" in Export mode with the current settings — **Location forced off** (F19).
- **Pure up-front planner:** reserves every output name; refuses output == input (file identity);
  suffix or skip only (no overwrite in v1). No-clobber commits; an external `EEXIST` re-suffixes.
- Folder dialog via the wrapper; naming template; progress + Cancel + summary; capped write workers
  with RAM admission.
- **Tests:** planner; concurrent same-stem jobs; cancel; counts; nav p95 holds (manual protocol).

## Rung 7 — Later

Wipe divider; convert-to-sRGB option; Settings ▸ Export (same stored values); quick export and
drag-out; share sheet; presets; crop UI; `fit_rect` insets; lossless JPEG → JPEG XL; IPTC/XMP credit
fields; pinning a captured frame for Compare; Copy carries the profile (task #148).

---

# Part 5 — Testing and measurement

- **Test-first** for every pure piece (pb-encode, `prepare`, spring, planner, folder policy,
  allowlists, sanitizer, view model).
- **Fidelity via the export decoder and independent container inspection.**
- **Property tests:** rotations; batch plans; preview identity.
- **Golden images:** export → decode → render → nv-flip vs the source (headless).
- **Privacy:** the macOS-runnable core no-trace test; the pb-app test on Windows/Linux; the static
  audit (new write sites: the write worker's commit, two settings fields, the macOS defaults-key
  cleanup); ICC/EXIF/XMP/IPTC/PNG-text leak fixtures; hand checks of OS dialog persistence.
- **Fuzz/malformed inputs** for new parsers.
- **Nav p95 under load — manual protocol until a replay harness exists** (F23): on a fixed corpus
  folder with `--metrics`, hold Space for 20 s idle, then again with a 24 MP export (or a batch)
  running; compare p95/p99. A scripted nav replay is worth building before rung 6.
- **Perf episodes** measure to the present call; stage timings for file exports; codec tables.

# Part 6 — Cross-platform execution

| Work | Mac | Windows | Linux |
|---|---|---|---|
| Core, pb-encode, pb-decode, pb-render | Builds + tests; **cross-checks pb-app** | `cargo test` | Container smoke |
| macOS shell | ✅ | — | — |
| winit shell (incl. the `IFileDialog` wrapper) | Compile cross-check only | ✅ runs it | rfd + egui |
| Native codec builds | Homebrew dev / pinned release | vcpkg | system / AppImage |
| OS dialog persistence + TCC / exFAT / SMB checks | ✅ | ✅ (registry MRUs) | ✅ (`recently-used.xbel`) |

Claim in *Handoff* before starting; fetch before starting and before pushing; a debt line for any
shared-code change the other machine can't verify; a revert lever for unverified UX; **stage
explicit paths only** (shared working copy).

# Part 7 — Risks

| Risk | Mitigation |
|---|---|
| GPL via a wrapper crate | Whole-chain audit; `jxl-sys`, not `jpegxl-*` |
| HDR luminance differs by backend | #146 before HDR output; a display bug gets fixed there |
| Mis-tagged colour | Rule 1; the macOS export draw |
| Present-engine re-presents over the preview | The present gate + scenario test |
| Export work slows browsing | Own workers, own rayon pools, low priority, no `work_pending` |
| Codec builds on three platforms | One codec at a time; PNG/JPEG need none |
| Slow AVIF/JXL encodes | Source cache, debounce, latest-wins; preview bytes reused for the file |
| VRAM / RAM pressure | External-bytes accounting; refuse-before-reserve; files refuse rather than degrade |
| Metadata / ICC leaks | Allowlists; sanitizer; leak fixtures from rung 0c |
| File collisions and odd filesystems | Exclusive temps; three-step commit; pending destinations; file identity |
| Keys swallowed / Esc surprises | No inline text; ADR-023 kept; release net tested |
| Shell drift / struct-literal break | Per-rung contract table; Windows cross-check |
| UI creep | Progressive disclosure; greying |

# Part 8 — Open questions

1. ~~`Esc` in Export mode~~ — decided (*1.4* #11): quit, pinned for revisit.
2. ~~Commit key~~ — decided (*1.4* #11): `⌘↩` / `Ctrl+Enter` + `⇧⌘E`, pinned for revisit.
3. **`E`** toggles Export mode (rung 1: opens the save dialog directly).
4. **HDR default** — "HDR with SDR fallback" recommended.
5. **Normal compare tap/hold** — recommended.
6. ~~**Pinned + save start folder**~~ — superseded 2026-11-14 by § 3.6's revision: nothing is
   persisted any more, so there is nothing for pinning to clear.
7. **Strip-metadata stickiness** — always starts checked (recommended).
8. **RAW** — match the viewer (embedded full-size preview) recommended; a demosaic option?
9. **macOS Tab focus into the panel** in rung 2, or later?
10. **Windows MRU keys / Linux recents** — if they can't be suppressed, accept or work around?

**Pinned (owner, 2026-11-13) — revisit once Export mode is in daily use:**

- **`Enter` commits in Export mode** and/or **`Esc` leaves the mode**, if the owner finds they keep
  pressing them naturally. The owner's view: losing your place to an unexpected `Enter` or `Esc` is
  likely costlier than not being able to jump to a random image mid-export. `Esc` → leave mode
  would be an ADR-023 amendment.
- **To keep the flip cheap,** rung 2.0 implements Export-mode bindings as a **contextual keymap
  layer**: switching `Enter` to `ExportCommit` or `Esc` to "leave mode" in that context is a
  one-line default change plus its tests, not a redesign.

# Part 9 — Review logs

**Round 1 — Codex CLI, gpt-6-astra, xhigh.** "Directionally sound, not ready unchanged." 21 findings,
all verified and accepted in rev 3.

| Id | Sev | Finding | Rev 3 disposition | Revised in rev 4 |
|---|---|---|---|---|
| C1 | critical | `jpegxl-rs`/`-sys` GPL | Audit rule | `jxl-sys` chosen |
| C2 | major | Collision safety | Exclusive temps, planner | F7 fallbacks |
| C3 | major | ICC is metadata | Clean rebuilt profiles | F6 sanitizer replaces it |
| C4 | major | OS filename history | Recents suppressed | F9: dialog keys, #147 |
| C5 | major | macOS P3 draw | Honest labels | F5: own export draw |
| C6 | major | 203-nit convention unverified | Rung 5.0 audit | #146 |
| C7 | major | P010 transfer; `peak` | Rule 3; light-level algorithm | F18: HLG kept |
| C8 | major | Preview identity | Aux texture | F2/F3 |
| C9 | major | Ring eviction/compaction; mips | Aux texture | F3: stash model + accounting |
| C10 | major | Rotation, resize geometry | Unrotated previews | F10: rotation baked, R0 |
| C11 | major | Frozen requests | `ExportRequest` | F4: `MetadataPolicy` |
| C12 | major | Scheduling/budgets | Two workers | F8/F11 |
| C13 | major | Fit-scaled buffers | Frozen-identity capture | F12 |
| C14 | major | Key routing | No inline text | F1/F15/F17 |
| C15 | major | Source matrix | *3.10* | F5/F20 |
| C16 | major | Cache repeats decodes | Source cache | F10/F21 |
| C17 | minor | Display-string EXIF | Typed parsing | — |
| C18 | minor | JXL/#145 sequencing; fidelity tests | Per-source deps | — |
| C19 | minor | Settings compatibility | `serde(default)` | F16: lenient deserializer |
| C20 | minor | Stale tasks | Reconciled | Reconciled again |
| C21 | minor | Not photon; dialog time | Honest wording | — |

**Round 2 — Claude Code, Fable 5.2, high.** "Not ready unchanged; 0b/0c/1 can begin after a small set
of corrections." 29 findings. Load-bearing claims re-verified (F1, F2, F3, F5, F8, F9, F15, F16, F24
confirmed directly). **All accepted.**

| Id | Sev | Finding | Disposition |
|---|---|---|---|
| F1 | major | Esc rule reverses ADR-023 | Esc stays quit; owner to confirm (*1.4*, Q1) |
| F2 | major | Flick compares different pipelines; no Original for RAW/SVG/frames | A and B both from the export pipeline |
| F3 | major | Present engine re-presents over the preview | Present gate; stash-modelled aux; unified external bytes |
| F4 | major | Crate dependency cycle | Direction fixed; features; `PrepareSpec`; `MetadataPolicy` |
| F5 | major | macOS export through 8-bit P3 | Own export draw in rung 0b |
| F6 | major | Lossy clean-profile sketch; tests a rung late | Sanitized-profile allowlist; tests in 0c |
| F7 | major | Safe-write fallbacks, TCC, queued duplicates, hidden temps | Three-step commit; direct-write fallback; pending destinations; visible temps; hand checks |
| F8 | major | Wake contract; shared rayon; priority | Event-style wake; worker pools; low priority |
| F9 | major | OS dialog traces (macOS confirmed) | Key cleanup; Windows MRU checks; task #147 |
| F10 | major | Preview ≠ file; re-encode on export | Preview bytes are file bytes |
| F11 | major | Quit drops confirmed exports | Bounded finish; temp cleanup |
| F12 | minor | 0a hidden dependency, async Copy, AVPlayer PTS, animation cap, MF | Own capture thread; all four spelled out |
| F13 | minor | Front-loaded machinery | Trait/resize/flatten → rung 2; preview worker → 3; Copy ICC → #148 |
| F14 | minor | Spring gaps | Toggle event; settings force Export; key path; event timestamps |
| F15 | minor | Return is Random | Bindable `ExportCommit`; `⇧⌘E` |
| F16 | minor | `serde(default)` vs type errors; `last_folder` pinned | Lenient deserializer; stricter export rules; session folder in `ExportState` |
| F17 | minor | Mode × action unspecified; global `content_gen` | Table; identity by `SourceRef` |
| F18 | minor | HDR→SDR unspecified; HLG approximation | P3-linear tone-map; HLG kept; HLG anchors in #146 |
| F19 | minor | Batch Location | Forced off in batch |
| F20 | minor | SVG/resize semantics | Rasterize at target; no upscaling; 1:1 export view |
| F21 | minor | One texture vs two resident previews | Two aux (A, B); re-upload measured |
| F22 | minor | Contract under-listed | Per-rung table *3.5* |
| F23 | minor | Unmeasurable acceptance | Manual nav-p95 protocol; no design around 250 ms; pointer-only controls stated |
| F24 | nit | `png` 0.18 | Adopted |
| F25 | nit | PNG colour type, `sRGB`, transparent RGB | Adopted |
| F26 | nit | File identity for source check | Adopted |
| F27 | nit | Blocking `exists()`, pinned RAM, `prepare` copies | Off-loop checks; documented; consuming `prepare` |
| F28 | nit | #81 title, CLAUDE.md, Linux `libc` | Fixed / noted |
| F29 | nit | `E` changes meaning rung 1 → 2 | Accepted as-is |

---

## Handoff

**Status (2026-11-26):** **JPEG export goes through jpegli** (task #157, `crates/pb-jpegli`;
Rung 4's "JPEG encoder trial" has the build notes and measurements) and **zip 9 for archive doors**
(task #155's zip half). Verified on the Mac: pb-jpegli's tests (progressive 4:4:4, ICC chunks from
1 across three markers, EXIF in APP1, a jpegli error returns `Err` instead of exiting, eight
encodes in parallel), pb-encode's JPEG tests against both encoders, `codec_bench` on the corpus.
**Not verified:** jpegli on Windows at all (see Cross-platform debt).
**Trial Mac release build** (`release-macos.sh --release` from `dbbb3e8a`, build id clean; not
notarized, not uploaded, the shipped 0.4.0 DMG in `dist/` left as it was): signed with the
Developer ID under the hardened runtime, `codesign --verify --deep --strict` passes; no Homebrew
paths, FFmpeg only via `@rpath` from `Contents/Frameworks` and every dylib says "LGPL version 2.1
or later"; jpegli, libwebp and libjxl linked, with one `hwy::GetChosenTarget`; `minos 14.0`;
`licenses/` includes the IJG README and THIRD-PARTY-NOTICES the IJG line. DMG 28.1 MB (0.4.0:
22.4 MB, before any export codecs). Notarization wasn't exercised; nothing new needs it (jpegli
is static), but the real release is its first run with these codecs.

**Status (2026-11-24, session 8):** **rung 4's first batch is built** — WebP, AVIF, JPEG XL, HEIC
(Mac) and TIFF; see Rung 4's session-8 note, which also records four colour bugs measuring found
(moxcms's `chad`, "sRGB" descriptions on P3 profiles, an unnameable P3 profile, and jxl-oxide's
gamut squeeze — the last now task #156). Verified on the Mac: pb-encode 61 tests with every codec
(+ 53 with the defaults), pb-decode incl. Image I/O reading our AVIF / HEIC exports back as P3 with
alpha, pb-app-core incl. every offered format previewing and exporting end to end and a P3 photo's
JPEG XL previewing in full gamut; clippy `-D warnings` on pb-encode / pb-decode / pb-app-core /
pb-mac-ffi with every codec; the Swift host builds with them; `codec_bench` on the corpus (the
defaults). An independent Codex review (gpt-6.2-astra, xhigh) found twelve issues — all fixed in
the follow-up commit (Rung 4's note lists them). **Not verified:** the Mac GUI (manual test § 14); **all of Windows and Linux** — the
Windows clippy cross-check passes *without* the C codecs (it can't build them), nothing run: W20.
`build-windows.ps1`'s edits weren't even parsed (no PowerShell here).

**Status (2026-11-24, session 7):** **rung 2's Custom… size and rung 3's live preview are built**
— see both rungs' session-7 notes. Verified on the Mac: pb-app-core tests all pass (incl. 13 new
preview tests that run the real worker on real JPEGs — three of them regressions for an
independent review's findings — and the Custom… dialog flow); pb-render 79 incl. goldens; clippy
`-D warnings` on pb-encode / pb-render / pb-decode / pb-app-core / pb-mac-ffi; the Swift host
builds; `preview_bench` on the corpus. **Not verified:** the Mac GUI (the preview on screen, the
Metal aux path, the sheet, the Preview control) — manual test § 13; **all of winit** — the
Windows clippy cross-check passes (all targets), nothing run: W19. The WgpuRenderer aux slots have
no GPU test (they need a surface); the core's logic runs against a recording double (`AuxOk`).

**Status (2026-11-14, session 6):** **rung 2's Export mode is built** — see Rung 2's session-6
note. Verified on the Mac: pb-app-core 1,100 tests (+4 ignored) incl. the mode × action rows
and an end-to-end JPEG export; pb-encode 37; clippy `-D warnings` on pb-app-core / pb-encode /
pb-mac-ffi; the Swift host builds. **The Mac GUI was not driven** (the panel's look, the controls,
⌘↩, the save sheet's JPEG type) — manual test § 12. winit: **compile-checked only** (Windows
clippy cross-check, all targets, clean) — manual test W18.

**Status (2026-11-14, session 3):** **rung 0a (#81) is built** — Copy / Copy Text / Describe / Ask /
Export act on the video or animation frame on screen (Mac verified by tests; GUI and Windows not
driven). See Rung 0a's "Built" note. Session 2's status follows.

**Status (2026-11-14, implementation session 2):** rungs **0b, 0c and 1 are built** — Export as PNG
end to end on macOS, compile-checked for Windows. Session 2 closed rung 1's gaps (right-click item,
Q6), ran the macOS half of the HDR brightness audit (#146, rung 5's prerequisite), fixed HDR PNG
decode, and landed **rung 2's pure pb-encode core** (not wired to any UI yet — see Rung 2). Manual
test script: `.taskmaster/docs/export-rung1-manual-test.md`. Rung 0a (#81) not started.

**Verified** (on the dev Mac, macOS)

- `cargo test`: pb-encode 28, pb-decode 270, pb-app-core 1,043 — all pass, incl. the new
  core-level export no-trace test (`app_core_impl/export.rs`: viewing writes nothing; one export
  writes exactly one file, no temp left, the frozen 90° rotation baked in, the folder remembered
  only when unpinned).
- `cargo clippy -D warnings --all-targets` clean: pb-encode, pb-decode, pb-app-core, pb-mac-ffi
  natively; **pb-app (tests included) via the Windows cross-check** — so the `IFileSaveDialog`
  wrapper and the background-priority thread compile against `windows` 0.62.
- The Swift host builds (`build-swift-host.sh --release`): NSSavePanel sheet + File ▸ Export as
  PNG… (⇧⌘E). **The GUI itself was not driven** — behaviour-unverified until the owner runs it.
- Corpus sweep (`cargo run --release -p pb-app-core --example export_bench -- <folder>`) over
  ~/Downloads/test-images: 31/31 decodable files exported (JPEG, AVIF, WebP, HEIC incl. 10-bit and
  HDR, PNG, NEF, JXL, SVG). sRGB AVIF/HEIC stay sRGB through the new macOS export draw (F5);
  iPhone HEICs stay Display P3; 10-bit HEICs export 16-bit; HDR tone-maps to P3 (visually checked
  on one). `sips` reads the sanitized profiles as "Display P3" / "sRGB IEC61966-2.1".
- **PNG default = `Balanced`** from those numbers: encode p50 158 ms / p95 1.7 s, 1.37 MB/MP;
  `Fast` 10 ms / 104 ms but 1.82 MB/MP (+33%); `High` 435 ms / 3.9 s for −1%.

**Not verified**

- Everything in the manual test script — notably the sheet, the toasts, menu enablement, and that
  `NSSavePanel` doesn't add to Recent Items.
- Windows: the dialog's behaviour, `FOS_DONTADDTORECENT`, the `ComDlg32` MRU keys (W5), background
  priority. Linux: rfd's save dialog and GTK `recently-used.xbel`.
- TCC behaviour for the temp sibling next to a granted save path (the direct-write fallback is
  unit-untested on a real refusal); exFAT/SMB commit paths (the placeholder fallback).
- Copy on a video copies the poster on Windows (inferred).
- **HDR luminance (#146), macOS half measured 2026-11-14** (`pb-decode/src/hdr_audit.rs`, synthetic
  PQ + HLG cards): Image I/O stills (display **and** export decode), FFmpeg posters → exactly on the
  203-nit scale; AVFoundation posters +2–3 % on PQ (≈ one JND). So rung 1's HDR → SDR export on the
  Mac starts from correct values. **Not HDR at all:** JPEG XL (shows SDR). PNG with `cICP` showed
  the raw PQ/HLG code values as SDR — **fixed in session 2** (`png_hdr.rs`: PQ/HLG → scRGB via a
  cached LUT + the CICP primaries matrix → `finalize_hdr_scrgb`; display and export decode, every
  platform; an SDR `cICP` sets the colour transform, outranking `iCCP` per the spec). WIC + MF owed
  on Windows (ignored tests there).

**Decisions / corrections (implementation)**

- Rev 2–4 as before (Esc quit + Enter Random owner-confirmed 2026-11-13, pinned for revisit).
- **No-clobber commit = hard link first, then a `create_new` placeholder** — portable std calls
  instead of `renamex_np` / `renameat2` / `MoveFileExW` (same guarantee, no libc/windows APIs;
  § 3.4's order changed accordingly).
- **The JPEG encoder moved** to `pb_encode::encode_jpeg_rgba8` (no re-export); `describe.rs` and
  pb-decode's test fixtures use it from there.
- **Session 2:** the right-click item "Export as PNG…" on both shells (`ContextMenuState::can_export`,
  hidden for videos and doors — the menu's existing omit-don't-disable idiom), in a new file group
  with Show in Finder / Quick Sort.
- **Session 4 (owner feedback, 2026-11-14):**
  - **The start folder** is now beside the photo, with per-source-folder session memory in RAM;
    `last_export_folder` and the Q6 code are gone (§ 3.6).
  - **The success toast is clickable** on macOS: an export notice (`ExportState::notice`,
    `ExportNoticeView.swift`) — "Exported name.png" + a magnifying glass; click → Show in Finder
    (`RevealPath`); 5 s, held while hovered (leaving restarts the hold), dismissed by navigating
    or any other command; its expiry rides the core's wake list, not `work_pending`.
  - **File ▸ Show Last Export in Finder/File Explorer** (`Action::RevealLastExport`, no default
    key): the keyboard / VoiceOver route, and Windows' reveal — its HUD toast isn't clickable
    yet. `last_export` is RAM-only.
- The save-panel cleanup removes `NSOSPLastRootDirectory`, `NSNavLastRootDirectory`,
  `NSNavRecentPlaces` after each export dialog.
- **HDR → SDR keeps hue (session 5, owner report "HDR red looks kind of magenta"):** the
  per-channel Reinhard in `prepare`, `to_clipboard_rgba8` and the present shader is replaced by
  `pb_color::tonemap` (clip out-of-gamut channels, then Reinhard on max-RGB with the channels
  scaled together — § 3.8). Owner's HDR10 test pattern (BT.2020 bars near 10,000 nits): the red
  bar was (255, 0, 94); first fix (desaturate toward luminance) would still give ≈ 62 blue; the
  clip gives pure red — pinned by `the_test_patterns_bt2020_red_bar_stays_red`. Verified: pb-color unit tests (the old curve's 5× ratio jump is pinned
  as the counter-example), a prepare test, and a **GPU golden test** that the shader's HDR branch
  matches the CPU map within 2/255 (`offscreen_hdr_tone_map_keeps_hue_and_matches_the_cpu_map`,
  via a new `render_offscreen_hdr`). SDR images take the unchanged path (goldens pass).
- Quit waits up to 4 s for an in-flight export (`finish_exports_for_exit`), from both shells.

**Cross-platform debt**

- **Windows (2026-11-26, jpegli, task #157):** `crates/pb-jpegli` has only been compiled with
  Apple clang. With MSVC it adds `/EHsc` (the shim's error path throws through jpegli; `cc`
  doesn't add it), `/bigobj`, `/std:c++17`, `HWY_STATIC_DEFINE` and `_CRT_SECURE_NO_WARNINGS`;
  jxl-sys builds the same Highway with MSVC, so the headers should compile, but jpegli's own code
  never has here. Owed, on x64 **and** ARM64: `build-windows.ps1` builds; `cargo test -p
  pb-jpegli` and `cargo test -p pb-encode --features jpegli jpeg` pass; a JPEG export opens in
  Photos with its profile. **The lever if MSVC can't build it:** remove `"pb-encode/jpegli"` from
  pb-app-core's `export-codecs`, and JPEG export falls back to the `image` crate's encoder (the
  default quality is 85 for both, so nothing else changes). jpegli then ships Mac-only, and the
  changelog line saying so stays true.

- **Windows (2026-11-25, metadata):** the egui panel's five category switches (under Strip
  metadata) are compile-checked only. The library half's WebP / AVIF / JPEG XL / TIFF embedding is
  cross-platform but tested on the Mac only: export a GPS-tagged JPEG with Location on and check
  the file with `exiftool` on Windows.
- **Windows (2026-11-25, menus + F20):** View ▸ Export Mode (a check item), the retitled
  `[`/`]` items (`StepKeys` via `set_text`), the egui popup's same items, and the egui panel's
  "Show at export size" switch are compile-checked only (Windows clippy cross-check clean).
  Check that the muda titles update when entering and leaving the mode.
- **Windows (2026-11-25, Mac panel layout):** on the Mac, **Lossless now sits beside the Format
  pop-up** (no row comes or goes between formats), the label column is 78 pt ("Compression" is
  74), and **Compression is a pop-up** (the segmented control overflowed its column and shifted
  the panel sideways on PNG). The egui panel already had Compression as a combo, but its Lossless
  switch is still a row of its own: its 120 px Format combo + 40 px switch + label (~226 px) don't
  fit the ~184 px controls column of the 320 px panel. Doing the same there means measuring on
  Windows (a narrower combo, a smaller label column, or a wider panel). Owner's call whether
  Windows needs it; nothing is broken there.
- **Windows (session 8, rung 4):** `build-windows.ps1` now builds `export-codecs` (libwebp via cc;
  libjxl via **CMake + bindgen**, so the Developer shell is entered for it too — and now regardless
  of `-NoNative`) and `avif-asm` when NASM ≥ 2.15 is on PATH (x64). **Unbuilt and never run** —
  Codex's PowerShell-grammar parse is the only check it has had — run it; if `jxl-sys` or `libwebp-sys` fails,
  `-NoCodecs` is the lever, and report the error here. Then W20 (the egui Format combo, the Lossless
  switch, Quality greying, the dialog filters). **`release-windows.ps1` was deliberately left
  alone** (still `libheif,dav1d,ffprobe`): add `export-codecs` (and `avif-asm` on x64 with nasm)
  there once the dev script has built them on that box — and change the CHANGELOG's "WebP and
  JPEG XL are Mac-only in this release" when it does. Until then a Windows release lists PNG,
  JPEG, AVIF and TIFF.
  `avif-asm` on Windows ARM64 is untried (rav1e's aarch64 assembly under MSVC).
- **Linux (session 8):** the AppImage build (`release-linux.sh`) has no `export-codecs` — it needs
  cmake in `appimage.Dockerfile` and a container run to prove it. Until then Linux exports PNG,
  JPEG, TIFF and AVIF (pure-Rust rav1e, no assembly); the CHANGELOG says WebP / JPEG XL aren't in
  the Linux build yet.
- **Windows (session 7, rungs 2–3):** the Custom Size dialog window, the panel's File size /
  Preview rows, and the renderer's aux preview textures on **DX12** (mipped RGBA8, covering the
  ring / held / single-image paths) are compile-checked only. Manual test W19 — watch for shimmer
  at fit on a big photo (mips) and an instant `Y`.
- **Windows (session 6, rung 2):** the egui Export panel (`panels_ui::export_panel`), its
  `PanelAction`s, Ctrl+Enter, the format-aware `IFileSaveDialog` (`save_export`: JPEG filter +
  default extension) are compile-checked only. Manual test W18.
- **Windows: export is compile-checked only** — `save_dialog.rs` (IFileSaveDialog), the menu item,
  `export_worker::lower_thread_priority` (`THREAD_MODE_BACKGROUND_BEGIN`). The Windows machine
  must run manual-test section W. Strike this line once it does.
- **Linux:** untested (rfd save dialog; `nice(10)` worker priority).
- **Windows (session 5):** HDR photos/video on an **SDR** Windows desktop now display through the
  new hue-keeping shader branch (Metal-verified on the Mac; compile-checked for Windows). Look at
  a saturated HDR highlight on an SDR screen: reds should stay red.
- **Windows (session 3, #81):** video-frame capture is compile-checked only. It goes through MF's
  `decode_video_poster_replay(input, origin 0, t)` — verify it lands on the **paused frame** (MF's
  origin is assumed 0; if frames come out one off, pass the producer's origin), and note its
  limits: native-capped at 4096 px and HDR clipped to SDR. Manual test W12–W15.
- **Windows (session 2):** the right-click "Export as PNG…" item is compile-checked only. The #146
  WIC-stills and MF-poster audit tests are `#[ignore]`d on Windows — run
  `cargo test -p pb-decode hdr_audit -- --ignored --nocapture`; drop the attributes if they pass,
  else normalize per #146 subtask 3.

**Claimed**

- None. (#81 was claimed and built by the Mac in session 3; its Windows half is the debt below.
  Rung 4's first batch was built by the Mac in session 8; mozjpeg and oxipng are unclaimed.)
