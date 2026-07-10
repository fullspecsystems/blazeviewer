# Windows: stream Live Photo motion (port of task #69) — agent handoff plan

**For an agent working on the Windows box.** Written 2026-07-09 on the Mac, immediately
after landing the macOS port in commit `35cad0b` (`feat(live): stream Live Photo motion
on macOS via AVAssetReader`). **That commit is the spec** — read its diff first; this doc
maps each piece onto the Media Foundation backend and flags what's different.

## Context

Task #69 made Live Photo playback **streaming**: pressing `P` plays the first decoded
frame immediately and keeps extending the sequence while the rest of the `.mov` decodes,
and navigating away cancels the decode. Linux (FFmpeg) and macOS (AVAssetReader) are
done; **Windows still decodes the whole clip before playback starts**
(`crates/pb-decode/src/mf_video.rs`, batch-only, no cancellation). Measured on macOS the
user-visible wait went from 1.0–1.9 s to 13–161 ms; Windows should see a similar win
since `IMFSourceReader` is already a sequential decoder — the latency is purely the
"collect everything first" structure plus the timestamp lookahead.

## What is already done (shared, in the tree — do NOT rebuild any of this)

- **`MotionChunk` / `MotionHeader`** (`pb-decode/src/animation.rs`) — the streaming
  message vocabulary, with the state-machine contract documented on the type: one
  `Header`, then `Frame`s, then exactly one terminal `Done`/`Failed`; **cancellation
  returns silently with no terminal chunk**; never a `Frame` before the `Header`, never
  anything after a terminal.
- **`MotionCollector`** (same file) — collects a chunk stream into a whole `Animation`
  while validating the contract. The batch `decode_live_motion` becomes a thin wrapper
  over the streaming fn using this (see the macOS wrapper in `livephoto.rs` — copy that
  shape exactly). ⚠ Its `#[cfg_attr(not(target_os = "macos"), allow(dead_code))]` must
  widen to `not(any(target_os = "macos", windows))` once Windows uses it.
- **The whole consumer side** (`pb-app-core`): `AnimStream`/`StreamMsg`,
  `poll_anim_stream`, install/extend/done/failed handling, the disconnect-without-terminal
  fix, eager-prep and eager→Play upgrade — all platform-neutral, all already tested
  (six lifecycle tests in `app_core_impl.rs`). Windows only needs the producer + gates.
- **Test patterns**: `livephoto.rs`'s `#[cfg(test)]` module (missing file / garbage bytes
  / pre-set cancel / midstream cancel / full ordering contract / batch-wrapper agreement,
  corpus tests gated on `PB_LIVE_TEST_MOV` with a silent skip) — port it nearly verbatim.
- **`live_probe`** (`pb-decode/examples/live_probe.rs`) — the perf harness. Its streaming
  section is `#[cfg(target_os = "macos")]`; widen to `any(target_os = "macos", windows)`.

## What mf_video.rs already gets right (keep all of it)

- Sequential `ReadSample` pump — no per-frame seeks, nothing quadratic.
- `MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING` → the OS video processor applies
  the container **rotation** (portrait clips come out upright) and YUV→RGB32. So Windows
  needs **no `transform_to_quadrant` / `rotate_rgba`** — do not port those calls.
- Per-call `CoInitializeEx` + `Once`-guarded `MFStartup` (the decode runs on a fresh
  `std::thread` spawned by `start_live_stream`, so the streaming entry point needs the
  same init block the batch entry has today).
- `sample_to_rgba`: BGRX→RGBA swizzle honoring stride, **including negative stride
  (bottom-up rows)** and forcing alpha opaque — RGB32's fourth byte is undefined. Keep
  this helper; the shared `bgra_to_rgba_tight` does NOT handle bottom-up or the alpha
  forcing, so it is not a substitute.
- `mf_msg`'s `MF_E_TOPO_CODEC_NOT_FOUND` → "install the HEVC Video Extensions" message.

## Implementation steps

### Step 0 — baseline first (the old path is about to be restructured)

Copy the test corpus to the Windows box (on the Mac it's `~/Downloads/test-images/live/`:
`IMG_0031` HEVC/P3/portrait 87f, `IMG_7681` H.264/709/portrait 44f, `IMG_9940`
HEVC/P3/landscape 65f — still + `.mov` pairs; `D:\Media\` is a sensible home). HEVC clips
need the **HEVC Video Extensions** Store package installed. Run
`cargo run --release -p pb-decode --example live_probe -- <clips> --dump <dir>` and
record each clip's total decode time + the PNGs (rotation/color reference).

Targets, same as macOS: first frame ≤ 250 ms p95, total ≤ baseline, cancel ≤ 100 ms,
retained bytes within `MAX_DECODED_BYTES`.

### Step 1 — restructure mf_video.rs into a streaming producer

New public fn with the exact shared signature:

```rust
pub fn decode_live_motion_streaming(path: &Path, max_long_edge: u32,
    cancel: &AtomicBool, emit: &mut dyn FnMut(MotionChunk))
```

Reshape `decode_inner`'s loop (most code moves over unchanged):

- **Init**: same COM/MFStartup block, then check `cancel` **before** creating the source
  reader and again before the first `ReadSample` (eager-prep streams get superseded fast).
  Setup failures emit `MotionChunk::Failed(DecodeError::Corrupt(mf_msg(e)))` and return —
  don't lose the friendly HEVC-extensions message.
- **Per-frame delay without lookahead**: the batch path paces frame *i* by sample *i+1*'s
  timestamp — that's a one-frame holdback, unacceptable for streaming. Use
  **`IMFSample::GetSampleDuration()`** (100 ns units) per sample instead — the direct
  analog of the `CMSampleBufferGetDuration` call the macOS port uses. Clamp to
  [1 ms, 2 s]; on error/zero fall back to `FALLBACK_DELAY`. (MF populates sample
  durations for file sources; verify on the corpus that the reported ~29/14/22 fps
  match the batch line.)
- **Header from the first decoded frame**, not the negotiated media type: after the first
  `sample_to_rgba` + `downscale_to_fit`, emit `Header { width: fw, height: fh, color,
  codec: "Live Photo" }` and remember `(fw, fh)`; every later frame must match or emit
  `Failed("frame size changed mid-stream")` and return. (The negotiated type is usually
  right, but `MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED` mid-stream is real; deriving
  from decoded output makes it a non-issue, exactly like the macOS port.) If the media
  type *does* change, the safest v1 is the mismatch-`Failed` above — do not try to
  re-negotiate stride mid-stream.
- **Cancellation**: check `cancel` at the top of each `ReadSample` iteration → `return`
  silently (no terminal chunk — Linux/macOS parity; the consumer has already dropped the
  stream). Dropping the `IMFSourceReader` is the cleanup; there's no explicit cancel call
  needed.
- **Budgets, checked before emit**: `count >= MAX_MOTION_FRAMES` **or**
  `bytes + rgba.len() > pb_decode::animation::MAX_DECODED_BYTES` (saturating add) →
  `truncated = true`, break, then emit `Done { loop_count: 1, truncated: true }` (≥1
  frame exists by construction). **Truncation must produce `Done`, not fall through to
  error handling** — this was a subtle review catch on macOS (an installed playback with
  no terminal parks on the decoded frontier forever).
- **Terminal**: EOF flag with ≥1 frame → `Done { loop_count: 1, truncated: false }`;
  zero frames → `Failed("Live Photo motion decoded no frames")`; any `ReadSample` error →
  `Failed(mf_msg(e))`.
- **Batch wrapper**: replace `decode_live_motion`'s body with the `MotionCollector`
  pattern from `livephoto.rs` (stream into a `MotionCollector::default()`, `finish()`),
  keeping the public signature. Delete the now-dead timestamp-lookahead pacing block.

### Step 2 — color (small, but do it honestly)

Today the Windows path tags `ColorTransform::srgb()` unconditionally. The video processor
converts YUV→RGB with the right *matrix* but does **not** convert *primaries* — so P3
clips (both HEVC corpus clips are P3-primaries) are likely oversaturated on Windows
today, the exact bug the macOS port fixed. Read the **native** media type's
`MF_MT_VIDEO_PRIMARIES` + `MF_MT_TRANSFER_FUNCTION`
(`reader.GetNativeMediaType(video, 0)`), map the `MFVideoPrimaries` /
`MFVideoTransferFunction` enum values to CICP code points (⚠ **the MF enums are NOT
CICP values** — write an explicit match: `MFVideoPrimaries_BT709` → 1,
`MFVideoPrimaries_BT2020` → 9, the P3 variant → 12; transfer `_709` → 1, `_sRGB` → 13,
`_2084` → 16, `_HLG` → 18; anything unknown → `ColorTransform::srgb()`), then
`ColorTransform::from_cicp(p, t, 0, true)`. Verify against the still: open `IMG_0031.HEIC`
and its motion side by side — saturation should now match (it was the macOS acceptance
check too). If the corpus shows the video processor *already* normalizing primaries
(motion matches still with plain srgb), keep `srgb()` and document why — measure, don't
assume, in either direction.

### Step 3 — gates + docs (mechanical; grep for each)

- `pb-app-core/src/app_core_impl.rs`: the two cfg gates on the streaming dispatch in
  `start_animation_decode` and on `start_live_stream` — add `windows` (they currently
  read `any(target_os = "macos", all(unix, not(target_os = "macos"), feature = "livephoto"))`).
- `pb-decode/src/lib.rs`: export `decode_live_motion_streaming` from `mf_video` under
  `#[cfg(windows)]`, next to the existing `decode_live_motion` export.
- `MotionCollector`'s two `cfg_attr(not(target_os = "macos"), allow(dead_code))` attrs in
  `pb-decode/src/animation.rs` → `not(any(target_os = "macos", windows))`.
- Stale comments that say Windows is batch-only (grep `"Media Foundation"` and
  `"Windows"` in pb-app-core): `app_core_impl.rs` (`start_animation_decode` +
  `start_live_stream` docs), `pb-app-core/src/animation.rs` (`StreamMsg`, `AnimStream`),
  `app_core.rs` (`anim_stream` field), `engine.rs` (`decode_motion_job` doc — after this
  its `live` branch is **dead on every platform except as the batch wrappers' caller**;
  say so), `mf_video.rs` module header (rewrite: it's no longer "the macOS spike's
  mirror" — macOS is AVAssetReader streaming now), and the `CLAUDE.md` "Known v1
  limitations" if it mentions Live Photo streaming platforms.
- `live_probe.rs`: widen the streaming-stats `#[cfg]` to `any(target_os = "macos", windows)`.

### Step 4 — tests

Port the `livephoto.rs` test module to `mf_video.rs` (same names, same assertions):
missing file → exactly one `Failed`; garbage bytes → exactly one `Failed`; pre-set cancel
→ zero chunks; cancel-after-3-frames → no terminal chunk, stops within a frame; full
ordering contract on `PB_LIVE_TEST_MOV` (Header first, Done last, ≥2 frames,
`loop_count == 1`, no interleaving); batch wrapper agrees with the stream. The
first two are CI-safe (no env var) — the Windows CI lane already runs
`cargo test` on the workspace, so they'll run automatically; corpus tests self-skip.

Run: `PB_LIVE_TEST_MOV=D:\Media\test-images\live\IMG_0031.mov cargo test -p pb-decode mf_video`
(also run once with the H.264 clip — it exercises the no-extensions-needed path).

### Step 5 — measure + CHANGELOG

- `live_probe` streaming stats vs the Step 0 baseline; check the four targets. Eyeball
  the dumped PNGs vs baseline (upright portrait, matching color).
- In-app smoke (owner or agent): `P` starts within a frame or two; navigate-away drops
  CPU promptly (Task Manager); dwell-then-`P` instant; frame-step (`,`/`.`) works on a
  Live Photo; audio starts at frame 0 and respects mute (Windows audio path unchanged).
- `CHANGELOG.md` `[Unreleased]` → `Changed`, mirroring the macOS entry (there are already
  Linux and macOS "Live Photos start playing almost immediately" entries — add the
  Windows one alongside; if unreleased still, they could be merged into one line).

## Gotchas / footguns (learned the hard way on the other two platforms)

1. **Silent cancel vs truncation are different terminals.** Cancel = return with nothing;
   truncation = `Done { truncated: true }`. Conflating them hangs installed playback.
2. **Byte budget before emit**, not after — the consumer retains every frame.
3. **Never emit two Headers, or a Frame after a terminal** — `MotionCollector` (used by
   the batch wrapper and its tests) will catch violations as `Corrupt` errors.
4. **The consumer already polls on Windows** (`poll_anim_stream` is unconditional in
   `tick()`); once the gates open, no shell/event-loop changes are needed at all.
5. `ReadSample` can deliver a null sample without EOF (gap/format tick) — the existing
   `continue` handling is correct; keep it (it must not emit anything).
6. Don't let a tool auto-trigger hosted CI (repo rule); the self-hosted Windows lane
   picks the push up on its own.
7. Release builds: `panic = "unwind"` and the decode thread is a raw `std::thread` — a
   panic in the producer becomes a channel disconnect, which the consumer now handles
   (routes through `stream_failed`), so don't add a `catch_unwind` wrapper; it's covered.

## Definition of done

All Step 4 tests green (incl. corpus on both HEVC and H.264 clips); the four perf targets
met and recorded in the commit message with the baseline numbers; portrait clip upright
and P3 motion color matching its still (or a measured justification for keeping
`srgb()`); `cargo clippy --all-targets -- -D warnings` and `cargo fmt` clean; Windows CI
lane green; CHANGELOG updated. Reference commit `35cad0b` in the message.
