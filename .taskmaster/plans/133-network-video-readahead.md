# #133 — Network video playback: read-ahead + read diagnostics (+ scoped discard)

**Status:** planned, Codex-reviewed (v2) → in progress
**Owner evidence (2026-07-23):** the Ghost in the Shell UHD MKV (~50–80 Mbps x265 HDR,
TrueHD Atmos) is choppy on the M5 Air **over SMB** at high-bitrate sections and
**flawless from local SSD on the same machine** — with CPU 78% idle and GPU ~0% during
the drops. Same build, same file: input starvation, not compute. The Studio playing
from local `~/Downloads` never showed it.

## Why this is the right fix (and why it's privacy-clean)

The Session route's demux reads are **demand-driven with a 32 KiB AVIO buffer** and no
read-ahead. SMB delivers the film's *average* bitrate easily but stalls on latency
spikes, and a spike longer than the small decoded-frame queue empties it →
`rebuffers`/`dropped` (the "choppy and slow" the session already counts). Every serious
player carries a compressed read-ahead cache for exactly this (mpv's demuxer cache
defaults to ~150 MiB forward); we are the outlier.

**Both playback inputs are exposed.** The video producer and the audio decoder each
open their own `FfInput` over the same file, and each reads the **full interleaved
container** (matroskadec fetches every block payload through AVIO regardless of stream
selection — see review disposition 1). The audio clock is the session's pacing master,
so an audio-input SMB stall freezes playback exactly like a video one. The ring
therefore covers **both** inputs.

This is **not** the forbidden kind of caching: a bounded, RAM-only ring of *compressed
bytes for the currently-playing file*, dropped on teardown with the rest of the session
state. No disk, no trace, no cross-file retention. It is the video-mode instance of the
prefetch ring: parked on a playing film, the most predictable next work is the next few
seconds of stream.

## Non-goals (explicit)

- **No on-disk cache of any kind** (Second Directive; would need opt-in + clearing).
- **No single-shared-demux refactor** (video+audio through one input). It is the only
  true traffic-halver for MKV (see disposition 1) — measure after this task; only
  architect it if the smoothed-but-doubled traffic still starves real links.
- **No discard in the audio decoder** (dispositions 1+2): on MKV it saves no bytes and
  it risks the deliberate `stream_index=-1` seek workaround (`audio_decoder.rs` —
  seeking by the audio index caused a measured **73 s** SMB linear scan; Matroska Cues
  index the video track). A mov-family-only audio discard (where `mov.c` genuinely
  skips unread samples) is documented future work, not this task.
- **No cold-seek fix.** First read after a jump is a network round-trip no matter what.
- **No adaptive/bitrate-aware sizing.** One budget, env-tunable, measured.

## Design

### Slice 1 — Read-stall diagnostics (the confirmed instrumentation gap)

The Session route has **no** read-latency signal (the `sb-read diag` twin lives only in
the parked sample-buffer presenter). Add two, both honest about what they measure:

- **`demux stall diag`** (producer): a pure, unit-tested 2 s window accumulator
  (`ReadStats`: reads, avg/max ms, >20 ms, >40 ms; injected "now", no sleeps in tests)
  folding the wall time of each `packet.read` call. This measures **what the session
  feels** (a demux call may hide zero or many source reads), labeled accordingly.
  Behind `PB_VIDEO_DIAG=1`:
  `[pb-video] demux stall diag: 2.0s — N reads, avg X.Xms max Y.Yms, >20ms=a >40ms=b`.
- **`src read diag`** (ring filler, Slice 2): the same window shape folding real
  `read_at` latencies + bytes/s + window occupancy — the **true source-latency**
  signal, measured where the I/O happens.

### Slice 2 — The read-ahead ring (both playback Path inputs)

New module `pb-decode/src/ffmpeg/readahead.rs`:

- `ByteSource` seam: `read_at(offset, &mut buf) -> io::Result<usize>` + `len()`.
  Real impl = `std::fs::File` positioned reads (`FileExt::read_at` on Unix,
  `seek_read` on Windows — small cfg shim). Tests use fake sources (latency
  injection, short reads, errors, EOF).
- `Ring`: one `Mutex<State>` + two `Condvar`s (data-available, space-available).
  State: ring buffer (modular indexing), window `[win_off, win_off+valid)`, consumer
  `pos`, **fill epoch**, sticky error, EOF mark, shutdown flag, reposition request.
- **Filler thread** (one per playback input): fills forward in ≤1 MiB source reads
  **outside the lock**, tagged with the epoch it captured; on re-lock a result whose
  epoch is stale (a reposition superseded it) is **discarded** — stale bytes/EOF/
  errors can never land in the new window (disposition 5). Short reads are normal
  (loop; only `Ok(0)` at `len` is EOF). When full, waits for the consumer to advance
  (bytes behind `pos − keep_behind` are reclaimable); a reposition restarts the window
  at the target and bumps the epoch. Thread-spawn failure ⇒ fall back to the direct
  open path (never fail playback for a missing nicety).
- **AVIO callbacks** (`ra_read`/`ra_seek`, the `mem_read`/`mem_seek` shape): check the
  shared `InterruptState` (cancel flags + watchdog) **at entry** and each 25 ms
  wait slice — a cancel is honored even when data is plentiful (disposition 9),
  returning `AVERROR_EXIT`. Serve from the window; block on the data condvar while
  the filler catches up. Short backward seeks (demuxer header/cue hops) hit
  `keep_behind`; far seeks reposition. `SEEK_CUR`/`SEEK_END`/`AVSEEK_SIZE`/
  `AVSEEK_FORCE` and overflow/range checks mirror `mem_seek` exactly. Callbacks never
  panic (C boundary): `catch_unwind` belt.
- **Engagement:** `FfInput::open_playback(input, cancel)`, used by exactly three call
  sites — the producer's `Reader::open`, `VideoDemuxer::open` (also covers the parked
  sample-buffer route), and the **audio decoder's** opens (all its opens are playback;
  it compiles on the Windows `ffprobe` build, so Windows SMB movie audio gets the
  same smoothing). `Path` inputs get the ring; `Bytes` inputs keep `MemCursor`.
  Probe / poster / details / cues stay on the direct open. **No size threshold**
  (disposition 6): capacity = `min(cap, file_len)`, so a 200 KiB fixture costs a
  200 KiB ring and the integration tests genuinely exercise the ring path.
- **Budget:** `cap` = 64 MiB per ring (≈8–10 s of this film's heaviest stretches; two
  rings while a film plays ⇒ ≤128 MiB, within "spend the hardware"). `PB_READAHEAD_MB`
  overrides per-ring, clamped to [4, 1024]; `0` disables the ring entirely (the A/B +
  revert lever). `keep_behind` = `min(8 MiB, cap/4)` so small caps keep forward room
  (disposition 12). RAM-only, freed with the input.

### Slice 3 — Scoped `AVDiscard` (hygiene, honestly labeled)

`discard_all_except(ctx, keep_index)` helper; called **only** where the kept stream is
the video stream: `Reader::open` (producer) and `VideoDemuxer::open`. On MKV this
saves demux-side parse/alloc/queue work, **not** network bytes (disposition 1); on
mov-family it also skips unread samples. Default-stream seek selection is safe here
because the kept stream *is* the video stream. The audio decoder and cues are
explicitly excluded (see non-goals). Correctness bar: the kept stream's packet
sequence (pts/dts/size/key **and payload checksum**) is identical with and without
discard.

### Hardening rider — `VideoSession` Drop cancels (disposition 8)

`VideoSession::stop()` flips the shared cancel flag but plain drop does not; a dropped
session leaves the producer to discover the disconnect only after its current blocking
read. Add `impl Drop for VideoSession` setting the cancel flag (idempotent with
`stop`), plus a test asserting the flag reads true after drop.

### Teardown order + honesty about blocked reads

`FfInput::drop`: close the format context → signal ring shutdown + notify → free AVIO
buffer/context → drop the ring cursor (its `Arc` keeps shared state alive for a
detached filler mid-read) → free `InterruptState` **last** (the ring callbacks hold a
pointer to it; same pinning rule as today). A filler blocked inside an OS read on a
*dead* server cannot be interrupted — but that is exactly today's exposure for a
blocked direct read inside libav (the interrupt callback can't preempt a syscall
either). The ring narrows it (≤1 MiB requests) and never blocks teardown on it
(detached thread, `Arc`-owned state, no join).

## Tests (TDD — each lands red→green)

`ReadStats`: window rollover at 2 s, bucket counters, reset, no mid-window emission.

Ring (fake sources, deterministic):
1. Sequential reads across wrap boundaries return exactly the source bytes.
2. A read blocks while the source is slow, then completes with correct data.
3. Short backward seek within `keep_behind` served without re-reading (call counts).
4. Far forward seek repositions; reads resume correctly at the target.
5. **A superseding reposition discards a stale in-flight fill** (epoch protocol —
   controllable-latency fake source).
6. Cancel unblocks a waiting read promptly (bounded) — and is honored at entry when
   data is available.
7. Source error surfaces and is sticky; short reads (`0 < n < want`) are transparent.
8. EOF at the end; `AVSEEK_SIZE`/`SEEK_CUR`/`SEEK_END` parity with `mem_seek`;
   out-of-range/overflow seeks rejected.
9. The window never exceeds capacity under churn; `keep_behind` clamps under small
   `cap`.

Integration (real fixtures — these DO exercise the ring now):
10. `open_playback` over `longgop.mkv` (Path): stream facts + first-N packets
    (pts/dts/size/key + payload checksum) identical to a direct open; a mid-file
    `demux_seek` lands identically.
11. Discard parity on `multitrack.mkv`: kept-stream packet sequence (incl. checksum)
    == the kept-stream subsequence of a no-discard control; audio-decoder suite stays
    green untouched (no discard there).
12. Open/drop/reopen teardown loops over `open_playback` (allocator-level
    double-free/leak coverage, as the existing io.rs tests do).
13. `VideoSession` drop sets the shared cancel flag.

## Validation (owner)

- **Air over the real share**, `PB_TRACE=1 PB_VIDEO_DIAG=1`, the GitS heavy section,
  A/B `PB_READAHEAD_MB=0` vs default: `src read diag` shows the SMB spikes; `demux
  stall diag` >40 ms count and `session diag` `rebuf`/`dropped` deltas should collapse
  to ~0 with the ring on. mpv over the same share is the external control.
- **Local SSD regression check** (disposition 7): same A/B on the Studio locally —
  open→first-frame feel, `session diag` stays at 0/0, no visible startup cost from
  the eager fill (a 64 MiB NVMe read is ~tens of ms, concurrent with probe).
- Traffic note: MKV total network bytes stay ~2× realtime by design this task (two
  inputs); the ring smooths, discard does not shrink MKV bytes. Shared-demux is the
  future lever if that still starves a real link.

## Risks / traps

- **Condvar discipline:** one mutex, two condvars, all waits time-boxed (25 ms) — a
  logic bug degrades to polling, never a deadlock.
- **`ffmpeg_next` API:** stream discard needs the raw `AVStream` pointer; keep it
  inside the one helper.
- **Windows compiles this tree** (`ffprobe`: audio decoder + probes + now the ring):
  run the Mac→Windows cross-check (`cargo check -p pb-decode --target
  x86_64-pc-windows-msvc`) before push. Windows behavior (SMB movie audio through the
  ring) goes on the handoff ledger as cross-platform debt.
- **Probe traffic is untouched** (disposition 10): `avformat_find_stream_info` runs at
  open before any discard/selection and reads what it reads (bounded by probesize).
  Accepted; not this task's problem.
- **Don't regress local playback:** correctness via tests 1/10; perf via the local A/B
  and the `PB_READAHEAD_MB=0` lever.

## Codex review dispositions (2026-07-23, codex-cli 0.144.6 — 13 findings)

1. **Accepted (critical):** matroskadec reads SimpleBlock payloads through AVIO before
   the discard check → discard saves no MKV bytes. Slice 3 demoted to hygiene; the
   "traffic halver" claim removed; shared-demux named as the only real halver.
2. **Accepted (critical):** video-discard in the *audio* decoder could break its
   deliberate `-1` default-stream seek (the 73 s SMB linear-scan workaround). Audio
   discard dropped entirely.
3. **Accepted (high):** the audio input reads the full interleaved stream and drives
   the master clock → it gets the ring too (was wrongly excluded in v1).
4. **Accepted-with-honesty (high):** detached-filler teardown is unbounded only on a
   dead server, same as today's direct path; bounded requests + `Arc` state; no join.
5. **Accepted (high):** fill-epoch protocol added; stale fills discarded; test 5.
6. **Accepted (high):** the 16 MiB threshold made integration tests vacuous — removed;
   cap scales to file length, fixtures exercise the ring.
7. **Accepted (high):** local-SSD A/B added to validation; `0`-knob is the lever.
8. **Accepted (high):** `VideoSession` gets a cancel-on-Drop + test.
9. **Accepted (medium):** interrupt checked at `ra_read` entry, not just while waiting.
10. **Accepted (medium):** probe-traffic limitation documented as out of scope.
11. **Accepted (medium):** there is no `FfAudioDecoder::set_track` — track switches
    build a fresh decoder via `open_track`; plan text fixed (moot anyway: no audio
    discard).
12. **Accepted (medium):** env clamps, `keep_behind` scaling, short reads, superseding
    repositions, seek-whence parity, spawn-failure fallback — all specified + tested.
13. **Accepted (medium):** diagnostics renamed/split: `demux stall diag` (felt) vs
    `src read diag` (true source latency at the filler).

## Handoff

- **Verified (macOS, this session, 2026-07-23):**
  - All 18 `readahead`/`read_stats` unit tests; the demux **parity tests**
    (ring+discard vs the pre-#133 control: byte-identical packets incl. payloads,
    identical seek landings, on `longgop.mkv` + `multitrack.mkv`); io.rs
    ring-engagement + teardown-loop tests; `VideoSession` drop-cancel test.
  - Full `pb-decode --features ffvideo` suite **green with `PB_NO_HWACCEL=1`**
    (359 passed — the ring exercised under every open; see flake note below for
    why VT-on full-suite runs aren't the yardstick today). Producer/demux/io/
    audio groups green with VT on. `pb-decode` default-features suite green
    (242). `pb-app-core` green except two **pre-existing** flakes (below).
  - Workspace clippy (`-D warnings`, pb-app excluded on macOS) + fmt clean.
  - `scripts/build-swift-host.sh --ffvideo` assembles the app.
  - The one `cfg(windows)` line (`FileExt::seek_read`) type-checked against
    `x86_64-pc-windows-msvc` via a scratch crate (std-only, exact mirror).
- **Not verified:** Windows build/behavior (the `ffprobe` cross-check needs
  `ml64.exe`, impossible from a Mac — full check owed); Windows SMB movie audio
  through the ring; Linux Session route (same code, untested); **the owner's SMB
  A/B on the Air** (subtask 4) and the local-SSD regression A/B.
- **Cross-platform debt:** `io.rs`/`readahead.rs`/`audio_decoder.rs` compile in
  the Windows `ffprobe` build — the Windows session must build + confirm movie
  audio (AC-3/DTS fixtures) after pulling.
- **Pre-existing test flakes (NOT this task — do not attribute):** on this
  Studio, full-suite runs flake in two ways **on clean main too** (verified by
  stashing): (a) `pb-decode` producer tests mass-timeout stuck in
  `VTDecompressionSessionCreateWithOptions` → `pthread_once` gate
  (`VTCopyRenderIDArrayForIORegistryKey`) when many tests create VT sessions at
  once — never happens with `PB_NO_HWACCEL=1`; thread-stack sample captured
  2026-07-23; (b) two `pb-app-core` probe-deadline tests
  (`a_real_video_probes_off_thread…`, `copy_details_mid_probe…`) fail under
  full parallel load, pass in isolation. Filed as task #134.
- **Claimed:** macOS session (plan + implementation), 2026-07-23 — implementation
  landed; releasing the claim with this commit.
- **Revert levers:** `PB_READAHEAD_MB=0` disables the ring at runtime; discard is
  one helper call per open site (comment marks each).
