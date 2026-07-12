# Cross-platform FFmpeg video — Linux playback + macOS codec fallback

**Status:** planned, **rev4** (2026-07-12) — Claude pre-execution review: anchors re-verified
against the working tree (line refs → symbol names), VP8 HW carve-out, macOS codec-set
correction (MKV wraps H.264/HEVC — can't drop them), `ffvideo` ship-gating mechanism,
libva/VAAPI runtime-dep audit, task #83 thumbs interaction, macOS archive-poster
unification candidate. Awaiting final sign-off before execution.

> **Owner decisions locked (2026-07-12):**
> 1. **HDR is implemented properly from the start** — not SDR-only-with-tone-map. The fp16/P010
>    output path (§9 option c) is a v1 requirement, or we're back here in days.
> 2. **Hardware decode or go home** — without real HW decode (**VideoToolbox on macOS at
>    minimum, the owner-testable gate**) the feature is useless (never smooth on real-world 4K).
>    Software decode is an internal bring-up milestone only; it is **not** a shippable outcome.
> 3. **Linux audio** — reuse the proven `pw-cat` *output* (the cpal/ALSA-on-PipeWire underrun
>    disaster is documented, commit `3a6c538d`); the open question is only the *clock* (§7).
**Relates to:** task #79 (video tier 2 — "Windows shipped; Linux/macOS = parity work"; the
Windows-side authoritative plan is `.taskmaster/plans/79-video-playback-tier2.md`), task #79.10
(**GPU decode REQUIRED to ship** — owner decision), task #77 (LGPL/GPL compliance — **hard gate
for public release**), task #81 (displayed-frame capture).
**Supersedes:** the "MKV/WebM stay on the placeholder" limitation in
`macos-archive-video-posters.md`.

> **Framing correction (rev2):** this is a **cross-platform media-backend project**, not merely
> "add a decoder thread." The FFmpeg producer is the largest *isolated* Rust component, but the
> riskiest *correctness* seams are: (1) the macOS **dual-backend** UI/lifecycle contract, (2)
> **streaming audio + clock ownership**, (3) **capability-based** fallback (not by extension),
> (4) **custom-AVIO cancellation**, and (5) **reproducible signed distribution**. The plan is
> organized around de-risking those first.

---

## 1. Goal

One shared engine, two features:

1. **Linux video playback** (task #79 parity) — Linux has none today.
2. **macOS codec fallback** — AVFoundation can't demux Matroska/WebM or decode VP8/VP9, so those
   degrade to a placeholder today. FFmpeg backs the formats AVPlayer refuses.

Both are the same missing component: a cross-platform **FFmpeg video producer + audio subsystem**
feeding the existing `VideoSession`. **AVPlayer remains the preferred macOS backend** for what it
handles well (MP4/MOV/H.264/HEVC, HDR, hardware decode, system A/V sync); FFmpeg is the fallback.

## 2. Architectural center (keep) + the honest scope (corrected)

**Keep:** reuse `VideoSession`; AVPlayer-first on macOS; one FFmpeg producer for macOS fallback +
Linux; preserve `VideoInput::Bytes` (archives); feed the existing wgpu presentation path.

**Corrected scope — the display path is cross-platform, but the macOS *UI* is not.** `poll_video`
and `present_video_frame` are un-gated and render a `Session` frame through wgpu on any OS — but
the macOS shell's **controls, progress, scrubbing, lifecycle, and effect handling all assume a
`NativeVideoPlayer`**. A Session-backed video on macOS would *render* while the UI misbehaves:

*(Anchors below are symbol-first — the tree is under active churn (task #83 lands in these
files) and bare line numbers were already stale at rev4; re-grep the symbol.)*

- Controls show only when `nativeVideo != nil` (the `videoControlsVisible` computation,
  `CoreModel.swift` ~1464).
- Fractional scrubbing calls `nativeVideo?.seek(toFraction:)` directly (`CoreModel.swift` ~2467).
- Lifecycle reconciliation keys on `native_video_session_id()` only (`CoreModel.swift` ~1398).
- Progress values come from the AVPlayer observer; core-side progress exists only for `Session`
  (`video_progress_row()` in `app_core_impl.rs` — public, but a Rust API the winit shell reads;
  no FFI) — its own doc comment says "the core provides no progress" on macOS.
- The macOS effect handler implements only the `Native` commands (the `CoreEffect` switch,
  `CoreModel.swift` ~1873 — `PlayVideo`/`StopVideo`/`SeekVideoBy`/… all gate on
  `nativeVideo?.sessionId`).

So "the only Windows-only piece is the producer" (rev1) was wrong. A **dedicated dual-backend
phase** is required (§8).

## 3. Decision

- **FFmpeg via `ffmpeg-next`** (already a dep; `ff_live.rs` proves the glue). One dep demuxes
  MKV/WebM/AVI/… and decodes VP8/VP9/AV1/H.264/HEVC + audio. Rejected: pure-Rust demux + dav1d +
  a VP9 decoder (more work, less coverage, no good pure-Rust VP8/VP9); GStreamer (bundling pain).
- **Dedicated `ffvideo` feature** — do **not** fold into `livephoto` (which means "Linux Live
  Photo / animated HEIF"; enabling it on macOS for general video misrepresents ownership):
  ```
  pb-decode:   ffmpeg  (dep + init/custom-IO/probe/color helpers)
               livephoto = [ffmpeg, Linux Live Photo]
               ffvideo   = [ffmpeg, streaming producer/poster/audio]
  pb-app-core: ffvideo -> pb-decode/ffvideo
  pb-app / pb-mac-ffi: ffvideo forwarding
  ```
- **Shared FFmpeg modules** (so producer/poster/audio don't drift on stream-selection/timing/
  rotation/color, and `ff_live.rs` doesn't become a second media engine):
  `ffmpeg/{init,io,probe,video_producer,poster,audio_decoder,color}.rs`.
- **Dep fixes (blocking, small; both re-verified 2026-07-12):** add `resampling` to the
  `ffmpeg-next` features (the dep line in `pb-decode/Cargo.toml` — today only
  `codec, format, software-scaling`) and `libswresample-dev` to the apt line in
  `scripts/appimage.Dockerfile` (today only avcodec/avformat/swscale) — audio decode/resample
  needs libswresample; neither is present today.

## 4. Phase 0 — proof gates (BLOCKING; do these before committing to the build)

Each is a spike that *decides and proves* an unknown, mirroring the Windows #79 phase-0 discipline:

1. **Reproducible, LGPL-compatible FFmpeg build + package spike on macOS *and* Linux** (§9), with
   VideoToolbox (macOS) / VAAPI (Linux) enabled in the build.
2. **Custom AVIO + cancellation spike** (§6) — in-RAM archive bytes + abortable blocking work.
3. **macOS hardware-decode + HDR interop spike (the biggest unknown; HW + HDR are both required).**
   Prove end-to-end: FFmpeg VideoToolbox `hwaccel` decodes VP9/AV1/HEVC → `CVPixelBuffer`
   (NV12/P010, incl. a 10-bit HDR clip) → minimal/zero-copy to a wgpu texture (IOSurface/Metal
   interop) → through the fp16 scene-linear present path → correct on screen. This validates §9
   (HDR-now) + §10 (HW-required) together. Benchmark SW vs HW on the §10 gates as the baseline.
4. **Audio-sink clock spike** on both platforms (§7) — prove an *honest* played-position clock
   (macOS `AVAudioEngine` render time; Linux `pipewire-rs` stream-time or characterized `pw-cat`
   latency).
5. **Confirm the locked decisions hold** (HDR-now / HW-required / `pw-cat`-output) survive contact
   with the spikes; surface any that don't **before** committing to the build phases.

## 5. Producer contract — observable protocol from MF, FFmpeg-native internals

The **observable** `VideoProducerEvent`/`Msg` protocol matches `mf_video_producer` (the
`VideoSession` is already unit-tested against it); the **implementation** must be FFmpeg-idiomatic
— do **not** recreate the whole reader just because that was fastest for MF.

Signature mirrors `run_video_producer(input, fit, session_id, generation, events, msgs)`.
Events: `Opened{duration,width,height,has_audio,frame_bytes}` (frame_bytes = *real negotiated*
output — late-negotiation is fine, see `video_session.rs:661`), `Frame`, `EndOfStream{gen}`,
`Failed`. Msgs (single merged channel): `Credit`, `SeekTo{target,generation}` (zeroes credit
balance; latest-value wins), `Stop`.

FFmpeg mechanics to specify (P1):
- Select the best **non-attached-picture** video stream; record its index.
- Normalize PTS via stream time base + container/stream start time + frame `best_effort_timestamp`;
  deterministic fallback for missing timestamps; drain delayed **B-frames** at EOF.
- **Seek:** `av_seek_file`/seek to a suitable keyframe → flush decoder → decode forward → publish
  the first frame meeting landing tolerance (NOT reader-recreate). Poll for a **newer** `SeekTo`
  while discarding toward the target so a superseding seek stays responsive.
- **Rotation** (display matrix) applied; **sample-aspect-ratio** handled (scale to square-pixel
  display dims or extend the placement contract — ignoring SAR distorts anamorphic video).
- Mid-stream resolution/pixel-format/color/SAR change: a clean *failure* is acceptable v1;
  silently reusing stale geometry is not. Validate odd dims before choosing NV12; cap dims +
  allocation arithmetic before building frames.
- Corrupt/truncated input can't spin forever hunting one credited frame; decoder panics / FFI
  unwinds are fatal errors, **never** unwind across C.
- **Poster:** one-shot first-non-black decode reusing the shared `poster_frame_bright_enough`
  walk, exposed through a backend dispatcher (§8-poster). Must honor the decode-pool `cancel`
  flag (`decode_item_cancellable` already threads it to `decode_video_poster`) via the §6
  interrupt callback — poster probes run on the shared pool, and task #83's multi-consumer
  contract makes prompt cancellation matter more, not less.
- **Thumbs strip (task #83) rides for free — but test it:** the strip captures
  `Purpose::Display` decode outcomes, and video posters arrive through `decode_item`, so the
  FFmpeg poster automatically becomes the video's thumbnail on Linux (today: permanent
  placeholder tiles). Verify the placeholder→poster preview-upgrade lands in the strip too.

## 6. Custom AVIO + cancellation (Phase 0 spike, then Phase 2)

In-RAM archive playback is among the hardest, least-proven parts — not a checklist item.
- Determine whether `ffmpeg-next` exposes enough custom-I/O; else use `ffmpeg-sys-next` for
  `AVIOContext`. One lifetime-safe wrapper owns `Arc<Vec<u8>>` + cursor + opaque state +
  `AVIOContext` + buffer + `AVFormatContext`; freed in the correct order.
- Bounds-checked read/seek callbacks incl. `AVSEEK_SIZE`; return FFmpeg error codes on failure;
  **no Rust panic crosses a C callback**. Prove repeated open/seek/drop under sanitizers/Miri.
- **Cancellation:** an FFmpeg interrupt callback (`AVIOInterruptCB`) so Stop/navigation aborts
  blocking probe/demux/decode. "Stop is never deafened because we block only on `recv()`" is true
  *only between* decode ops — FFmpeg can block or consume many packets producing one frame, so
  cancellation must reach *inside* that work.
- No temp files (privacy). Test truncated/malicious/deliberately-slow inputs.

## 7. Audio — a separate subsystem (NOT owned by the video producer)

The video producer's credit contract (one credit = one video frame) is incompatible with
continuous audio. Audio is a **separate demux/decoder instance over the same `VideoInput`**:
```
FF video producer -> bounded video-frame queue (credit-driven)
FF audio decoder  -> bounded PCM ring -> platform sink -> authoritative clock samples
```
Design requirements:
- Duplicate demux (preferred over coupling audio to video credits). Streaming PCM, **never** a
  full-clip `Vec` — a 2-hour clip stays constant-memory. Fixed output format (interleaved f32 or
  s16) at a negotiated rate/channels. Defined ring capacity + backpressure.
- Handle underrun, device-loss, pause, resume, mute, seek, drain, EOS. The sink reports the
  position **actually played** (accounting for queued device latency), not PCM bytes written.
  Session + seek-generation gating on audio callbacks. Archive bytes owned without a 2nd full
  copy. Bounded thread teardown/cancellation.
- **macOS:** `AVAudioEngine`/`AVAudioPlayerNode` (Session audio only; `Native` videos keep
  AVPlayer's own audio). Define the clock source (render/sample time) + how output latency is
  included.
- **Linux (decision — reuse `pw-cat` output; spike the clock):** the audio *output* is already
  solved and proven — `pb-app/src/live_audio.rs` pipes raw f32 PCM to PipeWire's `pw-cat` and
  plays clean. **Do NOT reach for cpal/rodio:** commit `3a6c538d` documents that cpal's ALSA path
  on a PipeWire desktop advertises `buffer_size min: 1` and underruns nondeterministically
  (audible machine-gun clicks) under *every* buffer/config strategy tried — `pw-cat` (talking to
  PipeWire directly) was the fix. Also carry forward the FFmpeg-8 audio gotcha: deprecated
  `channel_layout()` reads empty, so a swresample ctx built from it rejects frames
  (`AVERROR_INPUT_CHANGED`) — interleave by hand or set the layout explicitly. **The only open
  question is the master clock:** a `pw-cat` *pipe* reports bytes *written*, not what the device
  has *played* (queued latency), and video makes audio the clock master. Phase-0 spike picks:
  (a) direct PipeWire via `pipewire-rs` for stream-time/latency (the robust honest clock — the
  earlier grief was cpal's ALSA emulation, **not** PipeWire itself), or (b) keep `pw-cat` output
  and derive the clock from bytes-written minus a *characterized* fixed queue/device latency
  offset. Reuse the SIGSTOP/SIGCONT pause + EOF-exit + no-disk streaming from `live_audio.rs`.
- **Muted interim** (Phases 2–4 ship video-only): report `has_audio = false` to the session **or**
  inject one `AudioClockState::Failed` sample. Simply omitting a sink after `has_audio = true`
  adds a ~1 s preroll before silent fallback under the current state machine.

## 8. macOS dual-backend shell contract (dedicated phase, FFI + Swift tests)

- Expose the **active backend kind** through `pb-mac-ffi`: none / native / session.
- Expose **Session progress** (elapsed, total, fraction, playing/buffering/seeking) to Swift; today
  Swift reads AVPlayer directly.
- **Scrubber:** route fractional seek through a core `video_seek_fraction` command when the backend
  is Session; keep direct AVPlayer seeking for Native.
- `videoControlsVisible` depends on **an active playable video**, not `nativeVideo != nil`.
- Keep the macOS **frame pump alive** while a Session plays (AVPlayer self-composites without it;
  `VideoSession::poll()` cannot).
- Confirm on **both** variants: keyboard play/pause, frame-step, mute, relative + fractional seek,
  hover reveal, resize pause/resume, slideshow suppression, toolbar state, deletion, navigation.
- Replace stale "macOS is always Native" comments.
- **Poster/metadata dispatcher:** `decode_video_poster` on macOS is currently a compile-time
  AVFoundation impl (`av_poster.rs`); "same split for posters" needs a real runtime dispatcher
  (AVFoundation for native-playable, FFmpeg for the rest), not just a `start_video_session`
  change.
- **Archive-poster split (owner decision 2026-07-12): keep both paths, FFmpeg as fallback.**
  The macOS archive-poster round-trip (`CoreEffect::RequestVideoPoster` → Swift
  `generateArchivePoster` → preview-upgrade feed-back) **stays primary** for native-format
  archive bytes — AVFoundation's HDR/rotation handling is battle-tested there. The FFmpeg
  bytes-poster (§5) posters only what AVFoundation refuses (MKV/WebM/VP8/VP9/… entries, which
  today are permanent placeholders). Consequence accepted: two poster paths must stay in
  color/rotation parity — the §14 corpus gets a same-clip-both-paths poster-parity check.
- **Fallback state machine** (capability routing, §8a) lives here.
- **Tests:** FFI + Swift unit/contract tests, not only an owner smoke.

### 8a. Route by playability, not extension

Two-level policy:
1. Cheap **known-unsupported-container** fast route → FFmpeg (MKV/WebM/…).
2. Nominally-native containers (MP4/MOV): try AVPlayer, **fall back to FFmpeg only on classified
   demux/codec failures**.

Fallback transition: allocate a fresh session id; stop + detach the failed AVPlayer; **don't show
the native failure toast before fallback is attempted**; start FFmpeg at the intended position
(usually 0, but preserve a known user seek if failure came after opening); surface **one** final
error only if **both** fail; **never** fall back for DRM / permission / missing-file /
cancellation / unrelated I/O; record which backend was tried to avoid loops.

## 9. Color / HDR / pixel format

- Per-frame precedence: decoded-frame metadata → stream → container → documented fallback
  (dims/codec). Map **primaries, transfer, matrix, range separately**. swscale applies matrix +
  range during YUV→RGB — the app must **not** re-apply those; the resulting RGB still needs correct
  primaries/transfer interpretation.
- **Decision (owner): implement HDR properly now — option (c).** RGBA8 can't hold PQ/HLG headroom,
  so v1 produces a correct **fp16 (scene-linear scRGB) / P010** output for HDR sources, not
  tone-mapped RGBA8. SDR sources stay on the cheaper RGBA8 path. This reuses the renderer's
  **existing fp16 scRGB HDR pipeline** (the stills/HDR-AVIF/HEIC path: `Rgba16Float` scene-linear
  intermediate + present-pass tone-map/encode + `primary_hdr()` swapchain detection) — video
  frames slot into the same present path rather than a new one.
- **`present_video_frame` fix (required for HDR video):** it currently treats every non-NV12 format
  as ordinary non-HDR image data (`app_core_impl.rs:6754`) — it must honor `Rgba16F`/P010 +
  `VideoColorInfo` (PQ/HLG transfer, wide primaries, peak) and route through the fp16 HDR present
  path, mirroring how `is_hdr(img)` + the fp16 intermediate work for HDR stills. This lands in the
  shared foundation, not deferred.
- **HW-decode interaction (§10):** VideoToolbox/VAAPI HDR decode yields **P010/NV12 (10-bit)**
  buffers — design the HW path, NV12/P010 upload, and the fp16 HDR present together (they're one
  coupled subsystem, not sequential add-ons).
- **Scope clarification — "HDR-now" is pipeline correctness, output follows the platform.**
  The renderer already models this: macOS EDR is wired (`set_edr_headroom`,
  `hdr_surface_wants_edr`), so HDR video gets real headroom there; on Linux
  `display::primary_hdr()` currently returns SDR (no desktop HDR detection), so PQ/HLG video
  correctly **tone-maps through the fp16 present pass** — right colors, SDR ceiling. That is
  the intended v1 Linux behavior, not a violation of decision #1; Linux HDR *output* arrives
  whenever `primary_hdr()` grows a Wayland/KMS path, with zero video-side changes.

### FFmpeg distribution — a proof gate (esp. macOS)

macOS today builds a Rust staticlib → links into the Swift exe → embeds Sparkle
(`build-swift-host.sh:36`); there is **no** FFmpeg build/linker-decl/dylib-embed/install-name/
signing/validation. The Phase-0 packaging spike must decide + prove:
- Exact FFmpeg source/version/config + enabled demuxers/decoders; arm64-only vs universal; macOS-14
  deployment target; `@rpath` install names; SwiftPM/Rust `linkerSettings`; copy FFmpeg + all
  non-system transitive dylibs into `Contents/Frameworks`; `install_name_tool`; **inside-out
  Developer-ID signing of every dylib before the app**; hardened runtime + notarization; verify
  with `otool -L` / `codesign --verify --deep --strict` / `spctl` / clean-machine launch; **no
  Homebrew / `/usr/local` / `/opt/homebrew` runtime dependency**; automated failure if any dep
  resolves outside the bundle.
- **Linux is not proven** just because linuxdeploy sees FFmpeg: add a clean-container/AppImage
  audit (`ldd`, isolated launch env, real per-codec decode tests) — dynamically-loaded codec libs
  may not be discovered from the main exe.
- **Linux VAAPI runtime deps are host-provided, by design:** VAAPI needs `libva` + the vendor
  driver (iHD/radeonsi/…), which are driver-tied and must **not** be bundled (same excludelist
  philosophy as Mesa/GL). The audit therefore covers: FFmpeg built with the `vaapi` hwaccel,
  `libva` soname resolution on a clean machine, and **graceful SW fallback when the
  driver/device is absent** (headless CI, VMs) — never a crash or a black frame.
- **Task #77** broadened into a concrete FFmpeg compliance deliverable: configure flags, license
  mode, notices, source/build-script availability, dynamic-replacement story, and a manifest of
  bundled LGPL/GPL components. "LGPL-only" **and** "supports every listed codec" must be *proven*
  compatible, not assumed.

## 9a. Format registration (extensions, pickers, associations) — unchanged from rev1

The target set (MKV/WebM/AVI/WMV/ASF/MPG/MPEG/MTS/M2TS/3GP/3G2) is **already registered** across
all six sites (audited 2026-07-12), so this plan adds *playback*, not selectability:
1. Recognition — `VideoContainer::from_extension` / `LibraryItemKind` (`pb-app-core/src/video.rs`)
2. Archive byte-stream MIME — `video_content_type` (`pb-decode/src/video.rs`)
3. Windows picker — `VIDEO_FILTER_EXTS` (`pb-app/src/main.rs`)
4. macOS Open panel — `presentOpenPanel` exts (`CoreModel.swift`)
5. Windows default-app — `VIDEO_EXTS` (`pb-app/src/default_app.rs`)
6. Linux desktop MIME — the `.desktop` `MimeType=` (`scripts/release-linux.sh`)

**Checklist:** any container FFmpeg lets us opportunistically add that is NOT in the set above
(FLV/OGV/TS/VOB/MXF/…) must be added to **all six** in lockstep, or it decodes but is
unselectable. (A future refactor could collapse 1+3+4+5 into one shared list.)

## 10. Performance / hardware decode — REQUIRED to ship (reconciled with #79.10)

Task #79.10 + the owner's 2026-07-12 call are authoritative and aligned: **hardware decode is
REQUIRED, not deferred.** *"GPU decode is NOT optional — software playback is only acceptable for
the lowest-resolution videos."* / *"HW decode or go home — otherwise it'll never be smooth in most
real-world playback."* VP9/AV1 (the whole point of the macOS fallback) are the expensive software
formats, so this bites hardest exactly here.

**Decision:** software-RGBA is an **internal bring-up milestone only, never a shippable outcome.**
Hardware decode + 10-bit (P010/NV12) delivery through the fp16 HDR present path (§9) must work
**before** the macOS fallback or Linux playback is declared shipped.
- **macOS = the hard gate (owner-testable):** FFmpeg VideoToolbox `hwaccel` (or a native
  `VTDecompressionSession`) for VP9 (Apple Silicon) / AV1 (M3+) / H.264 / HEVC → `CVPixelBuffer`
  (NV12/P010). Prefer a **zero-copy or minimal-copy** path to the wgpu texture (IOSurface/Metal
  interop) over a CPU readback. The owner validates this directly on Apple Silicon.
- **Linux = implement, but validation-limited:** FFmpeg VAAPI `hwaccel`. **The owner cannot test
  Linux hardware** (§16 risk) — so Linux HW decode ships behind whatever validation we *can* get
  (CI/software-fallback parity, a Linux tester, or a documented "HW path present but
  owner-unvalidated" caveat). Linux always retains the SW fallback for machines without VAAPI.
- **NV12/P010 + fp16 present** is the shared plumbing both platforms' HW paths deliver into
  (extends the `ReuseSlot` upload-reuse machinery already in `pb-render/src/gpu.rs`).
- **VP8 carve-out (hardware reality, not a choice):** no shipping silicon hardware-decodes VP8 —
  not VideoToolbox, and VAAPI VP8 is extinct in modern driver stacks. The HW-required gate is
  therefore scoped to **H.264 / HEVC / VP9 / AV1**; VP8 (and other legacy codecs FFmpeg covers,
  e.g. MPEG-4 ASP in old AVIs) plays on the SW path by necessity. Acceptable because VP8
  content is ≤1080p-era by construction — the §10 benchmark gates still apply to it at its
  native resolutions. *(Owner confirmed this reading 2026-07-12.)*

**Benchmark gates** (run before deciding VideoToolbox/VAAPI can defer): P→first-presented-frame
p50/p95; sustained decode throughput vs clip fps; frame misses/rebuffers over ≥5 min; CPU + power;
bytes copied + upload p95; seek-to-land p50/p95; memory plateau over long playback. Codecs: H.264,
HEVC, VP9, AV1 at 1080p30/60 + 4K30/60, on Apple Silicon + representative Linux Intel/AMD.

## 11. Displayed-frame commands (copy / OCR / describe / compare)

Copy currently re-decodes the *source* (`app_core_impl.rs:4866`) — for video that returns the
poster or fails, not the displayed frame. For Session playback, retain **one bounded CPU-side copy
of the last presented frame** (or an on-demand renderer readback) keyed by session id + seek
generation + item; use it for Copy / OCR / Describe / Compare. Clear on navigation/stop/failure/
replacement; keep the last frame available at EOS until teardown. (Overlaps task #81.) **If out of
scope, remove "full viewer feature set" from acceptance and list the limitations.**

## 12. Prefetch + lifecycle

"Prefetch" in acceptance = neighboring **still/poster** prefetch, **not** pre-decoding multiple
videos — only the active video has an FFmpeg session. Lifecycle proofs: navigate during open/decode/
seek/rebuffer/audio-init; delete during playback (after handle release); quit with blocked/corrupt
input; resize + scale-mode change; sleep/wake + audio-device change; archive close while FFmpeg
callbacks still hold the bytes; replay after EOS without rebuilding unrelated state; rapid switching
between Native and FFmpeg-backed macOS items.

## 13. Phasing (revised per review — proof gates first, Linux integration before macOS)

0. **Proof gates** (§4): packaging (mac+Linux), AVIO+bytes+seek+cancel, 5-min SW benchmark, audio
   clock, and the SDR/HDR + HW-scope decisions.
1. **Shared FFmpeg foundation** — feature structure, init, custom I/O, stream selection, timing,
   color (incl. the **fp16 HDR present-path fix** in `present_video_frame`, §9), rotation/SAR,
   probe. **Compiles + tests on macOS *and* Linux from the first PR.** *(SW decode here is the
   bring-up milestone, not a ship gate — see phase 6.)*
2. **Video producer + poster** — path input first, then bytes; session protocol, seek, EOS park,
   cancellation, corrupt-input limits; backend-independent poster/probe dispatcher.
3. **Linux video-only integration** — route *all* Linux video → Session. The simplest honest proof
   the engine is genuinely cross-platform (no AVPlayer to mask gaps).
4. **macOS dual-backend integration** (§8) — capability/fallback routing; FFI progress/backend
   state; SwiftUI controls + lifecycle parity; native formats stay AVPlayer.
5. **Streaming audio** (§7) — shared decoder; macOS + Linux sinks; honest clock; seek/rebuffer/
   device-loss. **Neither platform is "complete" before this.**
6. **Hardware decode — REQUIRED, not optional** (§10) — VideoToolbox on macOS (the owner-tested
   hard gate) + VAAPI on Linux, delivering NV12/P010 into the fp16 HDR present path; retest the
   §10 benchmark gates. **Neither platform ships on the SW path.** (Linux VAAPI is
   validation-limited — owner can't test Linux HW; SW fallback retained.)
7. **Distribution / release** — DMG/AppImage dependency closure, signing, notarization, notices,
   license artifacts, clean-machine matrix; resolve #77.

**Ship-gating (owner decision 2026-07-12): by discipline, not mechanism.** The owner will
simply not cut releases mid-project; no feature-flag gate is added to the release scripts.
(A mechanical gate — keeping `ffvideo` out of `release-macos.sh` / `release-linux.sh` feature
sets until phases 5+6 pass — was offered and declined.) Residual risk accepted: a release cut
between phase 3 and phase 6 would ship SW-only video; the mitigation is this plan's phase
tracker plus the release checklist. Note `ffvideo` still needs adding to both ship feature
sets **when** the feature completes (phase 7), along with the #77 notices.

## 14. Testing

**Corpus:** VP8-WebM, VP9-WebM, AV1 in WebM + MKV, H.264 + HEVC in MKV; VFR + B-frame-heavy;
negative/nonzero start PTS; rotation metadata; non-square SAR; odd dims; silent + Opus/Vorbis/AAC
audio; multiple video/audio streams + attached cover image; truncated/corrupt; SDR BT.601, BT.709
full+limited, P3, PQ, HLG; a long generated stream (constant memory); loose-path + archive-byte of
the same clip; a clip where native macOS open fails and FFmpeg succeeds; a clip where **both** fail
(exactly one final error). **Fixtures must be licensed/provenance-safe.**

**Tests:** no stale frame after a superseded seek; Stop latency while opening/reading/decoding/
seeking; A/V drift over 5 and 30 min; audio underrun → coordinated rebuffer (not video drift);
fallback with no poster flash / duplicate toast; native MP4 stays on AVPlayer; Session controls
work through SwiftUI (FFI/Swift contract tests); no files created/modified during loose + archive
playback (no-trace); runtime dependency closure + clean-machine launch for DMG + AppImage.

## 15. Acceptance criteria (measurable)

- macOS chooses AVPlayer for playable native media and **auto-falls-back** to FFmpeg on classified
  demux/codec failure — no duplicate error, no visible stale session.
- Linux uses the **same** FFmpeg producer/poster/audio + shared `VideoSession` semantics.
- Path + archive-byte playback support play/pause, mute, fractional + relative seek, frame-step,
  EOS replay, rotation, zoom/pan/scale, progress UI, deletion teardown, and displayed-frame
  commands **as explicitly scoped**.
- A/V drift ≤ **50 ms** steady state, ≤ **100 ms** after seek/rebuffer (starting targets).
- Memory plateaus independent of duration.
- Stop/navigation retires visible playback within one frame; all worker/native resources within a
  measured bound.
- **Hardware decode works** — macOS VideoToolbox owner-validated on Apple Silicon (the hard
  gate); Linux VAAPI implemented (validation-limited). The measured **codec/resolution matrix
  (incl. 4K VP9/AV1) meets real-time playback on the HW path** for H.264/HEVC/VP9/AV1; VP8 +
  legacy codecs meet real-time on the SW path at their native resolutions (the §10 carve-out).
  Beyond that carve-out the SW path is **not** a shippable outcome (§10 / #79.10).
- SDR color/range golden-tested; **HDR (PQ/HLG) is correctly preserved** through the fp16 scene-
  linear present path (option c) — not tone-mapped-only, not refused.
- DMG + AppImage launch on **clean machines** with no external FFmpeg/Homebrew/package-manager dep.
- Every bundled FFmpeg component/codec is in the **#77 compliance manifest**.
- No passive playback path writes files or viewing history.

## 16. Risks / open questions (carried)

Scale (producer ≈ `mf_video_producer`, + audio + **HW decode + HDR** + packaging — the required
scope grew with the owner decisions; genuinely multi-phase). **macOS VideoToolbox → wgpu HW-frame
interop** (IOSurface/Metal, ideally zero-copy) is the biggest technical unknown — Phase-0 gate #3.
**Linux HW decode (VAAPI) is owner-unvalidated** (no Linux test hardware) yet HW is a ship
requirement — mitigate via CI/a Linux tester/the SW fallback + a documented "HW present,
owner-unvalidated" caveat; do **not** let it block the macOS gate. Other: `ffmpeg-next` seek/
custom-I/O + `hwaccel` API surface; audio-clock accuracy; **#77 licensing gate** (now also covers
the HW-decode libs' license posture); macOS binary size — **corrected (rev4): H.264/HEVC/AAC
decoders CANNOT be dropped** ("AVPlayer already covers them" conflated codec with container:
MKV and AVI overwhelmingly wrap H.264/HEVC, and AVPlayer can't demux those containers, so the
FFmpeg fallback must decode them; FFmpeg's *built-in decoders* are LGPL — no x264/x265, which
are encoders — so no license cost). The droppable size is encoders, muxers, filters, and
network protocols, not mainstream decoders; feature-flag interaction with
`livephoto`/`dav1d`/`libheif` + CI lanes (keep non-video/headless/bench builds clean).
