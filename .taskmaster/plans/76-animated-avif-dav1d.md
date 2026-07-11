# Task 76 — Windows: animated AVIF (`avis`) playback via dav1d

**Status:** planned — **rev2** (2026-07-11: initial Claude review, then Codex review
incorporated; supersedes the inline subtask text in `tasks.json`, which mirrors these phases)
**Scope:** Windows only (x64 primary, ARM64 mirror). macOS (Image I/O) and Linux (FFmpeg
`ff_live.rs`) already play animated AVIF — do not touch them.

## Problem

Animated AVIF shows only the first frame on Windows. `detect_animation`
(`crates/pb-decode/src/animation.rs:247`) flags ISOBMFF image sequences only on macOS and
Linux+livephoto; Windows matches neither cfg, so an `avis` file is treated as a still and WIC
decodes frame 0.

**Spike findings (2026-07-11, ARM64):** WIC `GetFrameCount()` returns 1 for `avis` (the AV1
Video Extension WIC codec exposes only the primary item), and Media Foundation cannot demux a
`.avif` at all (`MFCreateSourceReaderFromURL` → 0x80004005). There is **no OS-native Windows
path** for AV1 image sequences.

**Owner decision (2026-07-11):** add dav1d (C, via vcpkg — same pattern as libheif) behind a
cargo feature; demux the `avis` container ourselves; decode + color-convert each frame.
Rejected: FFmpeg-on-Windows (would reuse `ff_live.rs` but is a much larger dependency the
project deliberately avoids on Windows).

## Decisions locked (rev2)

| Question | Decision |
|---|---|
| HDR (PQ/HLG) avis | **Route to still** (owner-accepted). The WIC still path already renders HDR AVIF to fp16; dav1d playback would be an 8-bit SDR clamp (`AnimFrame` is structurally RGBA8). Probe excludes HDR from detection → no play hint, no washed-out loop. |
| FFI approach | **C accessor shim** compiled by the `cc` crate against the vcpkg-installed dav1d headers — not hand-mirrored Rust structs (see ABI section; this dissolves the layout-corruption risk rather than testing around it). |
| Alpha aux track | Ignored v1 — plays opaque; the demuxer must still positively select the **color** track. Documented limitation. |
| Fragmented (`moof`), `stz2`, encrypted entries, multiple `av01` sample descriptions | **Rejected in `probe_avis`** → no play hint → still path. Clean, honest, revisitable. |
| Playback model | **Batch v1** (decode all frames, then play — the GIF/WebP model), with measured escalation triggers (see Performance gates). |
| Loop count | 0 = infinite (GIF convention; matches `ff_live.rs:116` `VideoKind::sequence()`). |

## Why this is more code than libheif was

WIC / Image I/O / FFmpeg are high-level (file in, RGBA out). dav1d is a bare AV1 frame decoder
(OBUs in, planar YUV out), so we supply what it doesn't do: **(A)** an ISOBMFF `avis` demuxer
(sample tables → per-frame byte ranges + timing + `av1C`), and **(B)** YUV → RGB8 conversion
(layout, bit depth, range, matrix). The vcpkg/build side is nearly a copy of libheif (low
risk); the demux + YUV side is the real work and the fragile part.

## Verified review findings (both reviews; all checked against the tree)

1. **`isobmff` already compiles on Windows unconditionally** (`lib.rs:38`) — reuse it; no port
   needed.
2. **⚠ `isobmff_is_sequence` matches BOTH `avis` and `msf1`** (`isobmff.rs:64`). dav1d cannot
   decode HEVC — a brand-level reuse gives every animated HEIC a dead ▶. Superseded by
   `probe_avis` (below), which is stricter than brand-only for avis too.
3. **Cancellation gap is real:** `decode_motion_job` receives a cancel flag but the generic
   animation branch (`engine.rs:308`) calls `decode_animation(&bytes, fit)` without it.
   Navigating away would leave a multi-second AV1 batch decode running in an orphaned worker.
4. **The byte cap is soft:** `collect_frames` pushes the frame *then* checks
   `MAX_DECODED_BYTES` (`animation.rs:592-602`) — overshoot bounded to one frame. The Linux
   path already does it right (`ff_live.rs:232` checks the projected total first). Fix the
   shared logic to the projected check; new backend uses projected checks from day one.
5. **No vcpkg pin exists:** `setup-libheif.ps1` clones the moving vcpkg tip (`git clone
   --depth 1`, line 41) — any ABI/version assumption is not currently reproducible.
6. **`build.rs` early-returns when `CARGO_FEATURE_LIBHEIF` is unset** (line 25) — a naïve
   extension would silently skip a dav1d-only build. Restructure so each feature is handled
   independently.
7. **Present path:** `present_anim_frame` (`app_core_impl.rs:3892`) calls `set_image` per
   animation frame on the event-loop thread, and `StagingUpload` allocates a staging buffer
   per upload (`upload.rs:28-33`, documented as off the keypress hot path). **This path is
   shared with GIF/APNG/WebP today** — avis adds identical `AnimFrame`s, so this is a
   pre-existing characteristic, not a regression from this task. Consequence: measure (gates
   below), don't rebuild the presenter inside #76.
8. **CI:** x64 lane builds/tests `--features libheif` (`ci.yml:93-104`); the ARM64 lane
   (`ci.yml:112`, variable-toggled) runs plain workspace tests — **no native features at all**
   today. Release script `release-windows.ps1:101` and dev script `build-windows.ps1:31-44`
   both hardcode the libheif feature/paths.
9. **No third-party notices file exists** in the repo; dav1d is BSD-2-Clause, which requires
   reproducing the license text in binary distributions.
10. **Stale comments** claiming HEIF sequences are macOS/Linux-only, to sweep when wiring:
    `animation.rs:39` (`AnimationKind::Heif` doc), `animation.rs:257-261` (detect comment),
    `engine.rs:294-297`, plus a final grep for others (`lib.rs`, `meta.rs`).

### dav1d `send_data` semantics (dispute resolved)

The original plan said `dav1d_send_data` "can partially consume"; the Codex review said it
"either consumes fully or returns EAGAIN without consuming". Neither is the whole story: the
header documents consume-or-EAGAIN, but dav1d advances `Dav1dData.data/sz` in place when a
packet holds multiple temporal units (FFmpeg's `libdav1d.c` wrapper re-sends the same packet
while `sz > 0` for exactly this reason). **Write the loop robust to both semantics:** keep
re-sending the *same* `Dav1dData` while `data.sz > 0`, draining pictures on every EAGAIN, and
after the last sample keep calling `dav1d_get_picture` until EAGAIN to flush delayed frames.
Each avis sample is one TU, so plain EAGAIN-retry is the expected case — but the robust loop
costs nothing.

## Design

```
bytes (RAM, from PhotoSource) ─┐
                               ▼
   probe_avis (new; used by BOTH detection and decode — one decision, no dead hints)
     ├─ bounded ISOBMFF box reader (normal + extended sizes, size==0, nesting caps)
     ├─ positive color-track selection: handler 'pict' with an av01 stsd entry
     │  (alpha aux tracks are 'auxv'/tref-auxl — never selected; AVIF spec requires
     │   the color sequence's handler to be 'pict')
     ├─ REJECT (→ still, no hint): msf1, moof-fragmented, stz2, encrypted entries,
     │  >1 av01 sample description, HDR transfer (track-scoped colr; PQ/HLG)
     └─ carries: av1C configOBUs (may be legally empty), track CICP, timing basis
                               ▼
   sample-table expansion (stsz + multi-run stsc + stco/co64 → validated absolute
   ranges; stts/mdhd v0+v1 → per-frame Duration; checked arithmetic throughout)
                               ▼
   dav1d via C shim (cc-compiled accessors; opaque pointers on the Rust side)
     robust send/get loop; sample→picture mapping via the timestamp cookie;
     cancel checked before each sample and after each picture
                               ▼
   YUV→RGB8 (pure Rust, unit-tested): I420/I422/I444/I400, 8/10/12-bit,
   limited/full range, identity/BT.601/BT.709/BT.2020 → source-gamut RGBA8
                               ▼
   downscale_to_fit + PROJECTED byte/frame caps + normalize_delay
   + display ColorTransform (primaries/TRC → in-shader, matching macOS P3 behavior)
                               ▼
   Animation { kind: Heif, loop_count: 0, frames, truncated }
```

Graceful failure at every stage: any demux/decode error returns `DecodeError` → the app falls
back to the WIC still (`decode_animation` already wraps in `catch_unwind`). A build **without**
the feature is exactly today's behavior: first-frame-static, no play hint.

## ABI safety (P0)

The killer risk in hand-rolled dav1d FFI is `Dav1dSettings`/`Dav1dPicture`: large public
structs the library writes directly — a size/offset mismatch is silent stack corruption, and a
runtime version check after `dav1d_default_settings` is already too late.

**Approach: a tiny C accessor shim** (`crates/pb-decode/csrc/dav1d_shim.c`, compiled by the
`cc` crate against the vcpkg-installed headers, gated on the feature):

- The shim owns every dav1d struct: `pb_dav1d_open(n_threads, max_frame_delay)`,
  `pb_dav1d_send(ctx, ptr, len, cookie)`, `pb_dav1d_next_picture(ctx, out*)`,
  `pb_dav1d_picture_{plane,stride,w,h,bpc,layout,mtrx,range,trc,cookie}`,
  `pb_dav1d_picture_unref`, `pb_dav1d_close`, `pb_dav1d_version_ok()`.
- Rust sees only opaque pointers + a small POD struct **we** define. Because the shim is
  recompiled against the *installed* headers on every build, struct-layout drift is
  structurally impossible — the C compiler resolves it.
- `pb_dav1d_version_ok()` still asserts the runtime major version at open (belt and braces);
  called before anything else.
- RAII wrappers on the Rust side for context/data/pictures, exercised on every error exit
  (tests). Pictures unref'd promptly after conversion (each pins a full frame + decoder pool
  buffers); `close` on all paths.

**Reproducibility:** pin the vcpkg checkout — `setup-libheif.ps1` gains a `-VcpkgRef <commit>`
(default = a recorded known-good commit) and checks out that ref instead of tracking tip;
record the resulting dav1d version in the plan/docs. Benefits libheif reproducibility too.

Rejected: hand-mirrored nested Rust structs (the original plan) — strictly more work *and*
more risk than the shim; bindgen — drags libclang onto both build boxes.

## Cancellation + memory bounds (P0)

- New `decode_animation_cancellable(bytes, fit, &AtomicBool)` (existing `decode_animation`
  delegates with a never-set flag). `engine.rs:308` passes its already-plumbed `cancel`.
  The dav1d backend checks before each sample send and after each returned picture (before
  YUV conversion). `collect_frames` gets the same cheap per-frame check — GIF/WebP benefit
  free. Target: cancellation observed within one in-flight frame (p95 ≤ 100 ms on the corpus,
  excluding unavoidable decoder teardown).
- **Projected byte cap** (fix the shared `collect_frames` push-then-check too, mirroring
  `ff_live.rs:232`): a frame that would cross `MAX_DECODED_BYTES` is never retained; add a
  test that the retained total never exceeds the cap.
- Checked width/height/stride arithmetic; `try_reserve_exact` before large RGBA allocations
  (hostile dimensions must be an error, not an abort).
- Transient-memory awareness: at conversion time the peak is decoder-owned YUV + full-res
  RGBA + downscaled RGBA. Bound the *source* picture (reject `w*h` over a sanity ceiling,
  e.g. the existing still-decode max) before allocating conversion buffers, and account the
  retained animation in frames, not compressed bytes.

## Demux contract (explicit support/reject)

| Area | v1 policy |
|---|---|
| Box reader | Normal + extended (64-bit) sizes, `size==0` = to-EOF, FullBox version/flags, nesting depth + total-walk caps. |
| `stsz` | Both forms (per-sample table + constant `sample_size`). `stz2` → reject in probe. |
| `stsc` | Full multi-run expansion; `sample_description_index` validated — >1 `av01` description → reject in probe. |
| `stco`/`co64` | Both; every `offset+size` range validated against `bytes.len()` before slicing; overlapping/out-of-range → `DecodeError`. |
| `stts` / `mdhd` | Full run expansion, checked arithmetic; `mdhd` v0 + v1; `timescale == 0` → error. Variable durations supported (test fixture). |
| Edit lists / `ctts` | Ignored — correct, not lazy: the AV1-ISOBMFF binding forbids composition offsets for `av01` tracks (shown-frame order == decode order). |
| `moof` fragmentation | Reject in probe (no hint). |
| Encryption (`encv`/`sinf`) | Reject in probe (no hint). |
| `pasp` / non-identity track matrix | Ignored v1, documented. Rare in avis (rotation on stills is item-level `irot`/`imir`, which don't apply to the track). If the corpus surfaces a rotated/anamorphic avis, revisit. |
| Canvas | First decoded frame fixes canvas dims (existing `collect_frames` convention); a mid-sequence render-size change → `DecodeError`. |
| OBU normalization | **None needed:** the binding mandates size-fielded OBUs in samples and `av1C`, and dav1d accepts TUs with or without temporal delimiters. Proven by fixtures from two muxers (avifenc + ffmpeg) rather than a rewrite pass. |
| `av1C` configOBUs | May be legally empty (seq header in-band in sample 0) — skip the config feed, not an error. |
| Loop metadata | None exists in avis → `loop_count: 0`. |

`probe_avis` returns the parsed decision (selected track, CICP, config OBUs, table offsets) so
detection and decode share one code path — detection never promises what decode can't attempt.

## Color correctness

- **`color_from_colr_box` alone cannot drive YUV conversion** — it intentionally returns
  `None` for sRGB/no-op *display* transforms, but the converter always needs matrix + range.
  Probe carries a track-scoped CICP struct: primaries, transfer, matrix, full/limited range,
  chroma sample position — read from the **selected trak's** `stsd` `colr` (never a
  whole-buffer scan; an alpha track's `colr` must not win).
- Precedence (per MIAF): nclx `colr` overrides bitstream `color_config`; when `colr` is
  absent, use the picture's sequence-header values via shim getters (dav1d parses them).
- Supported matrices: identity/RGB, BT.601-family, BT.709, BT.2020-NCL. Anything else
  (ICtCp, 2020-CL, unspecified-and-no-colr defaults to 601 per convention — decide and test)
  → `DecodeError`, never a silent guess.
- Limited + full range at 8/10/12-bit with deterministic rounding; 10/12 → 8 by shifting
  after matrix (document the exact pipeline in the module header).
- Chroma upsampling v1: nearest (co-sited), documented — visually fine at photo scale;
  bilinear is a measured follow-up if the corpus shows fringing.
- Two color stages stay separate: YUV matrix yields **source-gamut** RGB; primaries/TRC ride
  the display `ColorTransform` into the shader (mirror macOS, which carries real P3 — never
  hard-code `ColorTransform::srgb()`).
- **HDR backstop:** probe rejects PQ/HLG via track `colr`. A colr-less HDR file slips the
  probe (accepted rarity — hint shows, playback fails to the still): the decode path checks
  the sequence-header transfer from the first picture and returns `DecodeError`. No manual
  sequence-header bit-parsing needed for v1.

## Performance gates (replaces "event loop never stalls")

Decode is off-thread, but presentation is not (`present_anim_frame` → `set_image` per frame on
the event loop; per-upload staging allocation). That path is **shared with GIF/APNG/WebP
today**, so #76 measures rather than rebuilds it:

- **Measure on the corpus (release build, x64 + ARM64):** P-to-first-motion latency; total
  decode time + peak resident memory; `present_anim_frame` CPU p50/p95/p99; animation
  deadline misses / visible stutter; dav1d thread counts **compared, not guessed** (start
  2–4 threads, `max_frame_delay = 1` — the decode pool already parallelizes across images).
- **Escalation triggers (pre-agreed, so results force action, not debate):**
  - First-motion p95 > ~250 ms → reuse/generalize the existing `MotionChunk` streaming
    infrastructure so playback starts after a safe prefix (the machinery exists; task #69).
  - Present-path deadline misses during animation → file a **separate follow-up task** for a
    texture/bind-group-reusing animation upload path (two-slot ring). It fixes GIF/WebP too,
    which is exactly why it's not scoped inside an AVIF task.

## Phases

1. **Pin + policy lock.** `-VcpkgRef` pin in `setup-libheif.ps1`; record the dav1d version;
   create the third-party notices file (dav1d BSD-2-Clause text; audit what libheif/libde265
   already require while there) and wire it into the shipped artifacts.
   **✔ Done 2026-07-11:** pin = vcpkg `a0400024711b283056538ac19ced80b91a83c24c` (the
   2026-06-26 tip both existing trees were already on → libheif 1.23.0, libde265 1.1.1,
   **dav1d 1.5.3**; the shim's ABI target). `THIRD-PARTY-NOTICES.md` created (verbatim dav1d
   1.5.3 COPYING; LGPL static-link compliance for libheif/libde265 flagged as a pre-existing
   open item for the owner) and copied into the Velopack pack dir by `release-windows.ps1`.
2. **Build plumbing.** `dav1d` feature (pb-decode + pb-app pass-through); **restructure
   `build.rs`** so libheif and dav1d are independent (kill the early return); vcpkg port
   install in the setup script; `cc` shim compilation; `dav1d.lib` preflight with actionable
   panic. *Acceptance:* all four feature combos build (`none`, `dav1d`, `libheif`,
   `libheif,dav1d`); `dav1d` is a compile-checked no-op on non-Windows.
   **✔ Done 2026-07-11:** all four combos build; clippy (pb-decode + pb-app, both
   features) clean; `dav1d::tests::linked_dav1d_is_the_pinned_version` proves the static
   lib links and the shim's headers agree with it (1.5.x, API major 7); full pb-decode
   suite (115 tests) green with `libheif,dav1d`. dav1d installed into both local vcpkg
   trees (binary-cache hit); `setup-libheif.ps1` now installs `dav1d:$Triplet` and
   checks both libs. Non-Windows no-op enforced by the `target_os == "windows"` gate in
   build.rs (`cc` is an unconditional build-dep — build scripts can't cfg-gate on
   features, so an optional `cc` would break feature-off builds).
3. **FFI via C shim.** As specified above; RAII wrappers; version check first. *Acceptance:*
   keyframe OBU → picture smoke test; drop/error-path tests leak-clean.
4. **Demuxer + `probe_avis`.** Bounded box reader, positive track selection, the
   support/reject table, sample expansion, timing. *Acceptance:* unit tests per table form +
   malformed-input tests; probe rejects each unsupported class.
5. **Cancellable decode + memory bounds.** `decode_animation_cancellable`, `engine.rs`
   wiring, projected-cap fix in shared `collect_frames`, `try_reserve`, robust send/get loop
   with the timestamp cookie (`Dav1dData.m.timestamp = sample index`, read back from the
   picture — robust to hidden alt-ref frames yielding zero pictures).
6. **YUV→RGB + CICP color.** Matrices/ranges/depths/layouts incl. I400 and odd dimensions;
   colr-vs-seq_hdr precedence; HDR decode-time backstop.
7. **Detection + dispatch.** `detect_animation` `cfg(all(windows, av1_dav1d))` branch calls
   `probe_avis` (avis-only, HDR-excluded, no dead hints); `decode_animation_inner` routes
   `Heif` to the backend; still AVIF stays on WIC; feature-off keeps `Unsupported`. Sweep the
   stale macOS/Linux-only comments (`animation.rs:39,257`, `engine.rs:294`, grep for more).
8. **Tests, fixtures, fuzz.** See matrix below.
9. **Perf measurement.** The gates above; record numbers in the task on close.
10. **Release/CI/docs.** `release-windows.ps1` (preflight + `--features libheif,dav1d`);
    `build-windows.ps1` (same detection, a `-NoDav1d` analog or fold into `-NoHeif`→
    `-NoNative`); ci.yml x64 lane (ensure-step + feature lists); **ARM64 lane**: add the
    vcpkg ensure + a feature-on test job mirroring x64 (it currently tests no native features
    at all — add libheif+dav1d together, still behind the `WIN_ARM64_RUNNER` toggle);
    CLAUDE.md (AVIF row, wired-notes, release steps); CHANGELOG Added: "Animated AVIF now
    plays on Windows." Note msf1 stays first-frame-static (HEVC).
11. **E2E verify.** x64: `anim_info` on corpus `animated/3.avif` (26 frames — sibling
    `3.webp` confirms) → Heif kind, sane delays; app run per acceptance below; feature-off
    build unchanged; workspace tests + clippy `-D warnings` + fmt. Then ARM64: vcpkg build +
    the same smoke. Sync any drift back into `tasks.json`.

## Test matrix

- **Demux units:** every supported table form; malformed bounds/arithmetic (truncated tables,
  overlapping ranges, `timescale == 0`, giant `stsz`); probe rejection per class (msf1,
  moof, stz2, encrypted, multi-stsd, HDR-colr).
- **Fuzz:** the demuxer is a new hostile-bytes parser → `cargo-fuzz` target on
  `probe_avis` + sample expansion. Pure Rust, no native dav1d needed, so it runs on the
  existing (Linux) fuzz setup.
- **YUV units:** known-value vectors — limited + full range; 8/10/12-bit; identity, 601, 709,
  2020; I420/I422/I444/I400; odd dimensions.
- **Fixtures** (tiny, committed, generated by `avifenc` *and* one by ffmpeg for muxer
  diversity), each with **per-frame solid colors so expected RGB is deterministic** — exact
  assertions with small tolerance, no reference decoder needed in CI (a one-time local visual
  diff vs FFmpeg during phase 11 covers the rest): 8-bit 4:2:0 SDR; 10-bit SDR; P3 (asserts a
  non-sRGB `ColorTransform` is carried, mirroring macOS); alpha aux track (asserts color-track
  selection + documented opaque output); variable `stts` durations; odd dimensions; one 4:4:4
  or identity-matrix case.
- **Behavioral:** cancellation stops before the sequence completes; retained bytes never
  exceed `MAX_DECODED_BYTES` (projected cap); detection negatives (msf1, HDR, feature-off,
  corrupt); ABI: shim version check + error-path RAII tests.
- **Perf:** the phase-9 corpus measurements (first-motion, decode time, memory peak, present
  cost, deadline misses; x64 + ARM64; thread-count comparison).

## Acceptance criteria (task-level)

- `animated/3.avif` plays: ▶ hint, `P` loops ~26 frames, correct color and timing; still
  shows instantly first.
- Animated HEIC (`msf1`) and HDR avis show **no** play hint (still path, full fidelity).
- Feature-off build identical to today; corrupt/truncated/unsupported avis falls back to the
  WIC still via `DecodeError` — never a crash, never a dead hint from `probe_avis`-approved
  files failing for foreseeable reasons.
- Cancellation p95 ≤ 100 ms on the corpus (excluding one in-flight frame's teardown).
- Perf gates measured and recorded; escalation triggers honored if crossed.
- CI x64 + ARM64 lanes build/test the shipped feature set; both release scripts updated;
  third-party notices ship; workspace tests + clippy `-D warnings` + fmt clean on both arches.
