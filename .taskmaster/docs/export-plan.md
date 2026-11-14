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
  AVIF/HEIC Exif item, JXL `Exif` box. `little_exif` vs a minimal writer is decided in rung 2 by
  testing.
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
image's** peak, then encode P3 and tag P3. Operator: the display's extended Reinhard, so an SDR
export matches what an SDR screen shows.

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
- Quit waits up to 4 s for an in-flight export (`finish_exports_for_exit`), from both shells.

**Cross-platform debt**

- **Windows: export is compile-checked only** — `save_dialog.rs` (IFileSaveDialog), the menu item,
  `export_worker::lower_thread_priority` (`THREAD_MODE_BACKGROUND_BEGIN`). The Windows machine
  must run manual-test section W. Strike this line once it does.
- **Linux:** untested (rfd save dialog; `nice(10)` worker priority).
- **Windows (session 3, #81):** video-frame capture is compile-checked only. It goes through MF's
  `decode_video_poster_replay(input, origin 0, t)` — verify it lands on the **paused frame** (MF's
  origin is assumed 0; if frames come out one off, pass the producer's origin), and note its
  limits: native-capped at 4096 px and HDR clipped to SDR. Manual test W12–W15.
- **Windows (session 2):** the right-click "Export as PNG…" item is compile-checked only. The #146
  WIC-stills and MF-poster audit tests are `#[ignore]`d on Windows — run
  `cargo test -p pb-decode hdr_audit -- --ignored --nocapture`; drop the attributes if they pass,
  else normalize per #146 subtask 3.

**Claimed**

- None. (#81 was claimed and built by the Mac in session 3; its Windows half is the debt below.)
