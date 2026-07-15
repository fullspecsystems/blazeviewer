# AI Image Descriptions (task #44) + Copy Text from Image (task #45)

*Drafted 2026-07-03 from the owner design discussion; OCR sibling added same day.
Owner intends to start same day.*

Describe the current image with a vision model — on demand (`D`), or automatically when
the user parks on an image (opt-in) — overlaid where the detailed info panel goes, and
optionally spoken aloud. Two backends behind one core-owned seam: **Apple's on-device
Foundation Models** (macOS 27+, image input, ships with the OS) and an
**OpenAI-compatible local endpoint** (LM Studio / llama.cpp server / Ollama; Windows
and power users — Qwen3-VL-8B/27B-class runs well on the owner's RTX 5090).

## Owner decisions (2026-07-03)

- `D` = describe now (rebindable; also in the Image menu + context menu).
- **Auto-describe on park**: after ~500 ms parked on an image, describe it
  automatically; cancel on advance. **Opt-in toggle** (see privacy).
- Result renders in the detailed-info panel position; RAM-only cache per image so
  revisits are instant; everything dropped on exit.
- One-shot v1 — no streaming (revisit if a big local model feels slow; expected
  latencies are 1–5 s, nowhere near painful).
- Prompt is built in core from **salient metadata framed as unverified** (the
  wedding-photographer clock problem): capture time, camera/lens, raw GPS coords
  (offline reverse-geocoder = later upgrade), filename + parent folder.
- Custom prompt template = advanced feature, **settings.toml-only in v1** (no UI).
- **Speak descriptions**: a menu toggle in the spirit of Mute Live Photo Audio —
  sighted users leave it off; for vision-impaired users, park-to-describe + speech
  is the whole product. Platform TTS (offline system voices), no bundled anything.
- Settings gets a dedicated **AI Descriptions** section (own tab on macOS) with a
  **short privacy blurb** — a custom endpoint means your images are uploaded to
  wherever that URL points; keep it local / trust it (cloud services may retain
  images and train on them).

## WWDC26 confirmation — Apple FM image input is real (2026-07-04)

Watched **"What's new in the Foundation Models framework"** ([WWDC26 session
241](https://developer.apple.com/videos/play/wwdc2026/241/)). This validates the
Apple-FM backend's load-bearing assumption and sharpens two things:

- **Image input is NEW in the 2026 release (macOS 27), not the macOS 26 floor.** The
  2025 framework was text-only; the on-device model "is also gaining Vision
  capabilities" in 27. So the Apple backend (subtask 5) is genuinely macOS-27-gated —
  but the *feature* is not, because the local-endpoint backend ships it everywhere
  today (see the backend policy). The dev Mac is on 26.5 / Xcode 26.5, so subtask 5
  waits on the 27 SDK; subtasks 1–4, 6–8 don't.
- **The API is exactly the plan's shape** — dead simple, no model download, no setup:
  ```swift
  let response = try await session.respond {
      "Describe this image…"           // any prompt, incl. a user question (VQA)
      Attachment(NSImage(contentsOf: fileURL)!)   // file-URL handoff = subtask 5's design
  }
  ```
  Attachments accept `NSImage` / `CGImage` / `CIImage` / `CVPixelBuffer` / **file
  URLs**, any size and aspect ratio (no crop/pad). That confirms `CoreEffect::
  BeginDescribe { path, … }` can hand a file URL straight over — no pixel marshaling.
- **VQA is explicit** — the model answers *arbitrary questions* about an image, not
  just canned captions ("What animal is this?" + an image attachment). This is the
  seed of the **"Ask about this image"** extension (tasks.json #44 subtask 9): once
  describe works, a text-input affordance feeds a typed question through the *same*
  image-prep + `Describer` seam + panel. On-device FM makes it zero-setup / zero-cost
  / fully private; the local endpoint serves it on Windows. Post-v1.

- ⚠ **The "2M downloads" caveat is about Private Cloud Compute, not the on-device
  model — and we don't use PCC.** PCC is Apple's *cloud* escalation for heavier
  requests; it's free to developers under 2M first-time downloads (higher for iCloud+
  users). But **PCC is cloud**, so per ADR-018 (never cloud by our hand) the Apple
  backend must request the **on-device** model *only* and never fall through to PCC.
  The on-device path has no such cap, so the download threshold doesn't constrain
  PhotoBlaze. (Third-party server models — Anthropic/Google — also plug into the FM
  framework, billed per-token; not our path either.) **Action for subtask 5:** pin
  the session to on-device inference; do not opt into `PrivateCloudComputeLanguageModel`.

## Privacy stance (extends ADR-018)

- **Never cloud by our hand.** We ship no cloud backend. Apple FM is on-device; the
  endpoint backend sends images only to a URL the user typed in themselves.
- **Descriptions are RAM-only** — per-image cache in the session, dropped on exit,
  never a sidecar, never a disk cache. The no-trace integration test
  (`viewing_a_folder_writes_nothing_to_disk`) must still pass with the feature
  compiled in (an un-triggered describer does nothing; a triggered one writes RAM).
- Persisted config only: backend choice, endpoint URL + model, auto toggle, speak
  toggle, optional prompt template. All preference, no viewing trace.
- **Auto-describe is opt-in** precisely because of the endpoint backend: "local" is
  convention, not enforcement — auto mode + a remote URL would passively upload
  every image the user lingers on. On-demand `D` is always available; passive
  sending must be a deliberate election.
- Settings blurb (draft copy, tune at implementation):
  > Descriptions are generated on this Mac when using Apple's on-device model.
  > A custom endpoint uploads the image to whatever server the URL points at —
  > including automatically, if auto-describe is on. Only use an endpoint you
  > trust, ideally on this machine: images sent to online services may be kept,
  > and may be used to train future models.

## Architecture

Everything except one Swift file is shared. The core owns the policy; backends are
dumb "image + prompt in → text out" executors.

```
Action::DescribeImage ─┐
park >500 ms (opt-in) ─┼─► AppCore describe policy ──► backend seam
menu / context menu  ──┘        │       ▲                ├─ LocalEndpoint (pure Rust,
                                │       │                │   both platforms: ureq POST
              state machine ────┘       │                │   /v1/chat/completions,
              (idle/busy/shown/failed,  │                │   base64 JPEG)
               generation cancel,       │                └─ AppleFM (delegate to host:
               RAM cache per item)      │                    CoreEffect::BeginDescribe →
                                        │                    Swift FM session →
              rich panel + CoreEffect::Speak                 describe_finished/_failed FFI)
```

### Backend policy (core-owned, owner-accepted)

`DescribeBackend::Auto` (default) → Apple FM if the host reports it available
(macOS 27+, Apple Intelligence enabled), else the endpoint if a URL is configured,
else the menu item is enabled but `D` shows a one-line message pointing at Settings.
Explicit `AppleOnDevice` / `LocalEndpoint` override in Settings.

### New contract surface (pb-app-core)

- `Action::DescribeImage` — id `"describe"`, label `"Describe image"`, `OneShot`,
  default binding `D` (currently unbound; `⇧D` reserved for a future "detailed"
  variant). `Action::ToggleSpeakDescriptions` — id `"speak_descriptions"`, label
  `"Speak descriptions"`, menu-only by default (no key), checkbox state carried in
  `MenuState` exactly like `mute_live_audio`.
- `CoreEffect::Speak(String)` and `CoreEffect::StopSpeech` — shell TTS seam.
  **Navigation emits StopSpeech** (stale speech is as bad as a stale panel).
- `CoreEffect::BeginDescribe { path, prompt, generation }` — the Apple-FM delegate
  path (file-URL handoff; v1 scope is on-disk stills, see cuts). Results return via
  per-gesture FFI (`describe_finished(generation, text)` /
  `describe_failed(generation, kind)`) — the password-flow shape; **never an enum
  payload across swift-bridge** (NS2 gotcha).
- Settings (all `#[serde(default)]` so old files load):
  ```rust
  pub describe_backend: DescribeBackend, // Auto | AppleOnDevice | LocalEndpoint
  pub describe_endpoint: String,         // default "http://localhost:1234/v1" (LM Studio)
  pub describe_model: String,            // "" = endpoint's loaded/default model
  pub describe_auto: bool,               // dwell auto-describe (default false)
  pub speak_descriptions: bool,          // default false
  pub describe_prompt: Option<String>,   // advanced custom template (no UI in v1)
  ```

### Dwell / cancellation mechanics

Reuse the park detection that drives `EAGER_PREP_DELAY` (animation pre-decode): the
describe trigger fires from the tick path once parked ≥ 500 ms (constant
`DESCRIBE_DWELL`, tunable) **and** `describe_auto` is on **and** no cached
description exists for the item. Never keyed off "image changed" — hold-to-blaze must
not machine-gun the backend. Cancellation is the `scan_gen` pattern: a `u64`
generation bumped on every navigation; late results with a stale generation are
dropped on the floor (the HTTP worker also gets a cancel flag; FM results are just
ignored). Cache: `HashMap<usize, String>` keyed by item index, cleared with the
other index-keyed state on playlist rebuild and in `clear_session_state`.

## The prompt (core-built, identical for both backends)

Default template (placeholders substituted by the core; custom template via
`describe_prompt` uses the same placeholders):

```
Describe this image for someone who cannot see it. Lead with the subject and
setting in one sentence, then notable details (people, text, colors, mood).
Be concrete and concise — 2 to 4 sentences. Describe only what is visible.

Context — file metadata, which MAY BE WRONG or irrelevant; trust the pixels
over it and ignore anything that conflicts with what you see:
{context}
```

`{context}` lines (each included only when present and sane):
- `Filename: {filename}` and `Folder: {folder}` (parent dir name only, not the path)
- `Taken: {datetime}` — **junk filter**: drop exact epoch-default timestamps
  (1970-01-01, 1980-01-01, 2000-01-01 at 00:00) and future dates
- `Camera: {camera}` (make/model + lens if present)
- `Location: {gps}` — raw decimal coordinates labeled as coordinates; offline
  reverse-geocoding (nearest-city dataset, pure Rust) is a later upgrade
All fields come from the existing `meta_cache` / EXIF read — no new parsing.

`prompt.rs` in pb-app-core: pure `fn build_prompt(meta: &..., template: Option<&str>)
-> String`. **TDD target #1** — table tests for salience, junk-date filtering,
placeholder substitution, template override.

## Backends

### Local endpoint (pure Rust, both platforms — subtask 4)

- Image prep (shared, also pure): downscale decoded RGBA to ≤1024 long edge
  (pb-decode resize path), encode JPEG (quality ~85), base64. Never send the
  original file bytes (HEIC/RAW would be rejected by most servers anyway).
- `POST {endpoint}/chat/completions`, OpenAI chat schema: one user message with
  `image_url` (data URI) + the prompt text; `model` from settings ("" → omit —
  LM Studio uses the loaded model). Parse `choices[0].message.content`.
- **Blocking `ureq` on a dedicated worker thread** (decode-pool discipline; do NOT
  add tokio). Timeout ~120 s; connect timeout short (~3 s) so a dead endpoint
  fails fast with a clear message.
- Error taxonomy → user-facing: connect-refused ("Can't reach the endpoint — is
  LM Studio running?"), HTTP 404/400 (bad path/model), timeout, malformed reply.

### Apple Foundation Models (Swift, macOS 27+ — subtask 5)

- Availability: gate on `#available(macOS 27, *)` + the FM availability API
  (Apple Intelligence enabled); host reports it via a pulled FFI
  (`fm_available() -> bool`) the policy reads.
- Session per request with the image (file URL for on-disk stills — no pixel
  marshaling) + prompt; await text; `describe_finished`/`describe_failed` back in.
- **Build note:** requires the macOS 27 SDK (Xcode beta) while the deployment
  target stays 14.0 — all FM code behind availability checks; the build script's
  toolchain needs verifying before this subtask starts. If the SDK isn't on the
  dev machine yet, subtasks 1–4 + 6–8 are all buildable without it.

### TTS (subtask 7)

- macOS: `AVSpeechSynthesizer` (AVFoundation already imported for Live Photo
  audio). Windows: `Windows.Media.SpeechSynthesis` via the `windows` crate (WIC
  precedent) or SAPI (`ISpVoice::Speak` handles playback itself — simplest).
- `Speak` speaks; `StopSpeech` on navigation/quit; toggle in Image menu
  (checkbox, `MenuState`) + AI settings. When speak is on and a description
  arrives (manual or auto), speak it.

## Description panel (subtask 6, after subtask 1)

Task #44 can use the current `pb-hud` paragraph panel as an interim consumer, but
the durable target is task #54's rich-panel contract:

- `pb-app-core` owns a `DescriptionPanel` model: Markdown/plain source, busy/error/
  not-configured states, copy payload, and Ask/answer state.
- Windows/winit renders it in the future egui viewport overlay.
- macOS renders it natively once the SwiftUI/AppKit presenter exists, which gives
  selectable text, native copy, VoiceOver, and normal macOS text behavior.
- Until that lands, keep the disposable pure `markdown_to_plain` stopgap so model
  output reads cleanly in the HUD paragraph renderer.

The font-fallback cascade is still useful for the interim HUD and for non-Latin EXIF/text
values while rich panels migrate, but it is no longer the final answer for selectable
description text.

## Settings UI (subtask 8)

- **macOS**: a 4th tab "AI Descriptions" (own fixed per-tab height — the tabs
  auto-size per the 2026-07-03 rework). Rows: backend picker (Auto / Apple
  on-device / Local endpoint), endpoint URL (text field) + model (text field),
  auto-describe toggle, speak toggle, and the privacy blurb as a footnote-style
  caption under the endpoint rows.
- **egui**: an "AI Descriptions" group card (General or Display tab — owner's call
  at implementation), same rows, same blurb via `card_row` description text.
- ⚠ Settings tests: fold-only, pure (`fold_settings_form`) — **never** drive
  apply/save end-to-end (writes the real settings.toml; standing NS2 trap).

## v1 scope cuts (deliberate)

- On-disk stills only: archive entries punt with a toast (the Delete precedent);
  animations/Live Photos describe their poster/current frame — fine as-is.
- No streaming; no offline geocoder (raw coords in prompt); no custom-prompt UI
  (config key only); no detail levels (`⇧D` reserved); no per-language voice
  selection for TTS (system default voice).

## Sequencing (Rust-first; owner can start immediately)

1. **M1 — pure foundations, no UI:** subtask 1 (font fallback) + subtask 2 (prompt
   builder, TDD). Land independently; both improve the app today.
2. **M2 — feature works end-to-end vs LM Studio:** subtask 3 (seam/policy/state,
   unit-tested with a fake describer) + subtask 4 (endpoint backend) + subtask 6
   (current HUD paragraph stopgap plus the `DescriptionPanel` model). Fully testable on
   the 5090 box (or the Mac + LM Studio) with zero Swift written.
3. **M3 — productize:** subtask 8 (settings UI + blurb) + menu/keymap wiring.
4. **M4 — Apple backend:** subtask 5 (needs Xcode 27 SDK on the dev Mac).
5. **M5 — speech + auto polish:** subtask 7 (TTS) + auto-describe dwell tuning.

## Test plan

- Unit (TDD): `build_prompt` tables (salience/junk dates/templates); describe state
  machine with a `FakeDescriber` (request → busy → result; stale-generation results
  dropped; cache hits skip the backend; auto-describe respects opt-out + dwell).
- The endpoint backend's request-building and response-parsing as pure fns
  (`serde_json` in/out), no live HTTP in tests.
- No-trace: existing `viewing_a_folder_writes_nothing_to_disk` must stay green with
  the feature compiled in.
- Never end-to-end through settings persistence or the FM path (real config /
  needs the OS model); the FM Swift file is owner-smoked.

---

# Sibling feature: Copy Text from Image / Show Text (OCR, task #45)

Extract the text visible in the current image — arguably more useful day-to-day
than descriptions, and it shares the describe feature's entire worker/effect
skeleton. **Tier 1 only in v1** (owner-accepted): OCR on demand → clipboard and/or
a readable Text panel. The current HUD paragraph panel is an interim display; task #54
makes the durable panel selectable/copyable without requiring image-space text selection.

## Owner decisions (2026-07-03)

- **"Copy Text from Image"** in the Edit menu + context menu (the Copy Image
  Details precedent): runs OCR, puts all recognized text on the clipboard, toasts
  a confirmation ("Copied 214 characters" / "No text found").
- **`T`** (rebindable; currently unbound in defaults) shows the recognized text in
  the Text panel (currently the shared HUD paragraph slot; later task #54's rich
  `TextPanel`) to read before copying.
- Same privacy posture and it's *stronger* here: both engines are OS-built-in and
  fully on-device — no endpoint, no model download, nothing configurable to leak.
  Results RAM-only (cache alongside descriptions), dropped on exit.

## Engines (both on-device, no new deps beyond what the tree has)

- **macOS — Vision `RecognizeTextRequest`.** Available WAY before macOS 27 (no new
  SDK dependency — ships on the current floor), accurate mode + automatic language
  detection, returns lines with bounding quads in image coordinates. Runs
  Swift-side → the same delegate shape as the FM backend:
  `CoreEffect::BeginRecognizeText { path/pixels, generation }` →
  `ocr_finished(generation, text)` / `ocr_failed(...)`.
- **Windows — `Windows.Media.Ocr`** via the `windows` crate (WIC precedent).
  Blocking `.get()` on the async op from a worker thread. Constraints: languages
  need installed OCR packs (English ubiquitous); `OcrEngine.MaxImageDimension`
  (~2600 px) → feed the same ≤1024–2000px downscaled RGBA the describe path preps
  (SoftwareBitmap BGRA8 conversion). Quality is decent-for-print, weaker than
  Vision on stylized text — acceptable; the newer Windows App SDK `TextRecognizer`
  (Copilot+/NPU-gated) is a future upgrade behind the same seam, not the baseline.
- Reading order: both engines return lines in a reasonable order; join with
  newlines, collapse obvious duplicates. Keep raw line text — no cleverness in v1.

## Contract additions

- `Action::CopyImageText` — id `"copy_text"`, label `"Copy text in image"`,
  `OneShot`, menu/context-menu (no default key; users can bind one).
- `Action::ShowImageText` — id `"show_text"`, label `"Show text in image"`,
  `OneShot`, default `T`.
- Shared state machine with describe (busy/generation/cancel/RAM cache) — one
  `recognized_text: HashMap<usize, String>` beside the description cache. Copy
  reuses the existing `ClipboardPayload::Text` seam.

## Tiers deliberately NOT in v1 (recorded so we don't relitigate)

- **Tier 2 — VisionKit Live Text overlay (macOS-only, medium):** `ImageAnalyzer` +
  `ImageAnalysisOverlayView` layered over the CAMetalLayer gives true selection +
  data detectors, but must continuously track our fit/zoom/pan transform, ignores
  our rotation, and needs careful key/mouse-monitor gating — a bounded but real
  adventure. Revisit only if the clipboard-dump version leaves the owner wanting.
- **Tier 3 — cross-platform selection directly on the image (hard):** selecting text
  in the Text panel is covered by task #54. This tier is the harder Live-Text-style
  interaction where the user drags over text on the photo itself. We own the
  image→screen transform (highlight quads would be trivial and even rotation-aware,
  which VisionKit can't do), but a selection engine that *feels* right is weeks of
  work. Flagship-feature territory only.
- Cheap middle polish (optional, post-v1): while the `T` panel is up, draw the
  line bounding quads as translucent highlights over the image — pb-render quads +
  the transform we already own; display-only, no interaction.

## Garnish: QR / barcode payloads (part of #45, not a separate feature)

- **Pure-Rust decoder on BOTH platforms** — Windows has no OS barcode API, so a
  library is needed there anyway; using it everywhere (`rxing` = ZXing port,
  multi-format; or `rqrr`, QR-only) means no Swift, no FFI, identical behavior,
  zero C build risk, and unit tests that generate QR PNGs. (Vision's
  `DetectBarcodesRequest` exists on macOS but isn't worth a second backend here.)
- ⚠ **Full-res input**: unlike describe/OCR, barcode decoding needs
  pixels-per-module — run on the full-resolution decoded RGBA (grayscale), never
  the ≤1024 downscale.
- UX: no new key or menu item. The `T` panel lists payloads above the recognized
  text ("QR code → https://…"); "Copy Text from Image" includes them (toast:
  "Copied text + 1 QR code"). Later nicety (optional): a context-menu "Open Link…"
  when a payload is a URL — user-triggered only.

## Sequencing note

Tier 1 has **no macOS 27 dependency** — it can ship before the describe feature
finishes. Natural insertion: after describe's M2 (the state machine generalizes to
"analysis results" while it's being built), or even first as a warm-up for the
worker/effect shape. The QR garnish rides entirely on #45's plumbing (~half a day).

## Open questions (small, defer to implementation)

- Menu placement: "Describe Image" + "Speak Descriptions" under the Image menu
  (next to Mute Live Photo Audio) — assumed; owner may prefer View.
- Does `D` conflict with anything in the owner's customized keymap? (Defaults are
  clear; the editor's steal flow handles it either way.)
- Endpoint model listing (`GET /v1/models`) to populate a picker instead of a text
  field — nice-to-have, not v1.
