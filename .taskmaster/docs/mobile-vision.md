# PhotoBlaze on mobile — iPadOS / iOS / (later) Android

> Status: **vision / design note**, not committed work. Captures a direction that
> emerged 2026-07-13. The concrete first engineering step is task **#88** (Apple
> Photos Library as a `PhotoSource`), which doubles as the iOS photo-access foundation.

## The thesis

PhotoBlaze's one obsession — *how fast you can flick through thousands of images* —
is **not** a desktop-only value. Touch photo browsing is slow and one-at-a-time;
the system thumbnail strip barely helps. A speed-first viewer that also reads the
whole device (Photos **and** Files **and** network shares) is a *real app*, not a
feature-poor clone of the built-in Photos app.

The differentiators, same as on desktop:

1. **Speed** — *Hold to Blaze*: press-and-hold accelerates through photos at the
   display's refresh rate, decoded-ahead and resident, exactly like holding a nav
   key on desktop.
2. **Universal sources** — the Photos library, plus arbitrary folders in Files /
   iCloud Drive / SMB shares / external drives. Not locked to one library.
3. **Respect your files** — a viewer, not a walled garden. Editing hands off to
   Photos or your third-party editor of choice.

## Why the architecture is already most of the way there

| Layer | Mobile-ready? |
|---|---|
| Renderer (**wgpu**) | ✅ Metal on iOS/iPadOS, Vulkan/GLES on Android |
| **`pb-app-core`** (AppCore/NS0) | ✅ Platform-neutral; maps *action vocabulary*, not keys — a SwiftUI/Compose shell is just another consumer, like the Mac SwiftUI shell |
| **Decode-to-fit** | ✅ Decodes to *screen* size — critical on a phone (a 48 MP HEIC → a small display is a huge memory win) |
| Hardware decode | ✅ iOS ImageIO/VideoToolbox; Android MediaCodec — dedicated silicon, fast-per-watt |
| **250 ms hold-delay** model | ✅ Already the exact primitive a long-press maps onto |
| **Thumbnails strip** (task #83) | ✅ Ports as the mark/review surface |
| Video + Live Photo playback | ✅ Already shipping (Win + Mac); mobile uses the same system-framework pattern |

The genuinely **new** work is: a touch gesture layer, memory-pressure discipline,
the mobile shell(s), and store distribution. Not a rewrite.

## The interaction model: **blaze → mark → commit**

The insight that makes touch delete/sort pleasant: batch it.

1. **Hold to Blaze.** A long-press (`minimumPressDuration` ≈ the 250 ms hold-delay)
   ramps the self-paced, refresh-synced advance. The vsync primitive is
   `CADisplayLink` on Apple (ProMotion → 120 Hz) and `Choreographer.postFrameCallback`
   on Android. Lift to stop; a quick tap is a single step.
2. **Tap to mark.** A quick tap toggles an in-app **mark** on the current photo
   (delete-candidate, or "add to *Keepers*") — instant, no system prompt, shown as a
   badge in the Thumbnails strip. The selection set is **RAM-only** (preserves the
   no-trace guarantee).
3. **Commit once.** "Delete 37" or "Add 37 to *Album*" — one operation over the whole
   set (see the confirmation table below).

This cull/sort flow is arguably *the* killer use case for a speed-first viewer, and
it's something the built-in Photos apps do clumsily.

## The two OS constraints to design around

Everything else is clean; these two shape the design.

### 1. Memory pressure
Mobile has unified memory and an aggressive OOM killer (iOS jetsam, Android
`onTrimMemory`). A deep resident ring of full-res textures hits a wall sooner than
on a desktop GPU. Mitigation is already core doctrine: **decode-to-fit** (screen res,
not sensor res) plus a **pressure-aware ring** that shrinks on the memory-warning
callback. Tuning constraint, not a blocker.

### 2. Deletion always shows a system confirm — but **batching collapses it to one**
The OS forces a confirmation on any deletion of a *library* asset (privacy). You
cannot suppress it. But the batch APIs take an **array** and prompt **once** for the
whole set — so "blaze → mark → Delete 37" is a *single* tap-through, not 37. Only
deletion prompts; non-destructive ops are silent:

| Operation | Apple (PhotoKit) | Android (MediaStore) | Prompt |
|---|---|---|---|
| Delete selected → trash | `PHAssetChangeRequest.deleteAssets([…])` | `MediaStore.createTrashRequest` / `createDeleteRequest` (API 30+) | **One** for the batch |
| Add selected to album | `PHAssetCollectionChangeRequest.addAssets([…])` | album/playlist write | **None** |
| Favorite selected | `PHAssetChangeRequest.isFavorite` | `IS_FAVORITE` | **None** |

Both platforms give a **30-day trash / Recently Deleted** recovery for free — we don't
build a recycle bin for library assets, the OS owns it.

**Precision on "move to album":** in Photos/MediaStore, albums are *collections*, not
folders — an asset lives in "All Photos" and can be in many albums. So "move to album"
is really "**add** to album" (non-destructive, silent); a true move only makes sense when
the current view is itself a user album (then also `removeAssets`). Only **user-created**
albums are writable (not smart albums / Recents).

## Selection + batch ops is a **cross-platform** feature, not iOS plumbing

PhotoBlaze today is a single-photo flick-through viewer with no multi-select concept.
Adding an ephemeral **selection set** (RAM-only) lives cleanly in AppCore's action
vocabulary and benefits **every** platform — batch-delete and batch-add-to-folder are
just as welcome on Windows/Mac desktop. Build it once in the core.

## Sources on mobile

- **Apple — the Photos library via PhotoKit** (`PHAsset` / `PHImageManager` /
  `PHLivePhoto` / `PHAssetResourceManager`). This is the **same framework on macOS and
  iOS/iPadOS** — the backend built for task #88 (Route B) *is* the iOS photo access.
  On iOS it's the *only* viable library route (no filesystem access to the library).
  iCloud downloads are handled transparently; Live-Photo pairing is native. Needs the
  `NSPhotoLibraryUsageDescription` consent prompt.
- **Apple — arbitrary folders** via `UIDocumentPickerViewController` +
  security-scoped bookmarks: Files, iCloud Drive, **SMB/network shares**, external
  drives (iPadOS). This is the existing `FsSource` model with iOS access ceremony.
- **Android — MediaStore** (the PhotoKit analog: all device photos/videos) +
  **Storage Access Framework** for folders/SD/network. Scoped storage (Android 10+)
  routes media through MediaStore and documents through SAF; there is no single
  `.photoslibrary` package.

## Editing hand-off

Long-press → menu → **share/open-in** (`UIActivityViewController` on Apple; Android
share intent) hands the image to Photos or any third-party editor. In-app
non-destructive edit is possible via `PHContentEditingInput`/`Output`. There is no
documented deep-link that jumps Apple's Photos.app to *that exact asset* in edit
mode — "hand off to an editor" is the robust path and fully covers third-party apps.

## Platform priority

1. **iPadOS — lead target.** A Magic-Keyboard iPad has a **hardware keyboard,
   pointer, external drives, and multiple windows** — the *desktop* interaction model
   (Space/arrows/hold-to-blaze) runs there almost verbatim, on the same wgpu/Metal +
   AppCore + PhotoKit stack. Big ProMotion screen, real files, **both** interaction
   models. Likely the strongest fit of any mobile target.
2. **iOS (iPhone)** — the touch-first variant of the same app; smaller memory budget,
   touch-only, but the same engine and sources.
3. **visionOS — the iPad build now, the immersive dream later.** visionOS runs the
   iPad app in compatibility mode, so a windowed PhotoBlaze is **nearly free the moment
   iPad ships** — that's the initial target, no extra work. PhotoKit works unchanged
   (task #88 backend ports again). The **immersive "life flashing before your eyes"**
   build is deferred until hardware access (owner returned a Vision Pro; needs one to
   develop on). Why it's the most architecture-flattering target on paper: **both eyes
   sample one decoded texture** (per-photo decode cost, not per-eye) so the perf model
   holds at 90 Hz/eye, decode/prefetch/ring are untouched, and **pinch-and-hold = Hold
   to Blaze**. Its one real lift is a **visionOS-specific present layer** — Compositor
   Services (`LayerRenderer` / `cp_drawable` + ARKit) or a RealityKit `RealityView` —
   because wgpu doesn't drive the immersive stereo/foveated path out of the box.
   Everything below the present layer is reused. (Tiny but affluent, high-willingness-
   to-pay, low-competition audience — a great demo/flex, not a business.)
4. **Android — possible, later, bigger lift.** wgpu + AppCore port fine; the new work
   is a Kotlin/Compose (or winit) shell over a JNI/NDK boundary, a MediaStore/SAF
   source, a `Choreographer` hold-loop, and `createDeleteRequest`/`createTrashRequest`
   (which conveniently also give batch-confirm + a 30-day trash). Two languages + the
   JNI seam make it more friction than Apple, but architecturally the same story.
5. **Web (WASM + WebGPU) — the playable demo, not a port.** Scope it as *the landing
   page you can play*, not PhotoBlaze-in-a-tab: reuse `pb-core`/`pb-decode`/`pb-render`
   compiled to WASM+WebGPU, source = the File System Access API folder drop (the one
   source the browser sandbox allows — no Photos library, no shares), Chromium-first,
   browser-or-WASM-decodable formats only. The browser's biggest limitation (local
   folder only) *is* the demo use case. Only worth it if it stays a ~day-scoped taste
   of the speed that funnels to the native downloads — not a parity attempt.

## Open question (product, not tech)

The tech is ready; the *thesis for touch* needs its own answer: **what is "fast
flick-through" on a touchscreen?** Hold-to-Blaze is the hypothesis. It wants real
on-device validation — the whole value proposition rides on it feeling as good as the
keyboard does on desktop.

## Relationship to current work

- **Task #88** (Apple Photos Library `PhotoSource`) is the concrete first step and the
  shared Apple foundation. Its Route B (PhotoKit) is the iOS photo access; keep it
  seam-clean so it ports.
- A **selection + batch-ops** feature in `pb-app-core` is the other prerequisite and
  pays off on desktop immediately.
