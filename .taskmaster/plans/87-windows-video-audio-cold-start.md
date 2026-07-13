# Task 87 — Windows video audio cold start

**Status:** planned / deferred (2026-07-13)  
**Scope:** Windows tier-2 video playback: Media Foundation audio decode to shared-mode WASAPI.

## Goal

Start source audio with the first moving video frame instead of advancing through roughly one
second of generated silence. Preserve steady-state A/V sync, seek/replay, mute, archive-byte
playback, and silent fallback when no endpoint exists.

## Reproduction

Owner clip:

```text
D:\Media\Pictures\2010\2010-01-05 - Dazed and Confused in Oslo\SANY0501.MP4
```

Test on the physical Windows console after the endpoint has been idle, then replay immediately.
Record RDP separately because redirected audio adds downstream buffering.

## Quick investigation (2026-07-13)

Established facts:

- The clip is H.264 29.97 fps plus AAC-LC stereo at 48 kHz. Both streams start at PTS 0;
  the first audio packet starts at 0 and lasts 21.33 ms.
- The source does not contain a one-second silent lead. Decoded audio exists in the first 100 ms
  (about -18.5 dBFS RMS), and FFmpeg `silencedetect` finds no leading digital silence.
- The existing opt-in MF probe decoded 40 PCM reads / 163,840 bytes from this file in about
  10 ms. A generic one-second Source Reader warm-up is not supported by current evidence.
  Production negotiates float at the endpoint rate, so trace that exact path before ruling it out.
- `run_engine` calls `Engine::fill` before publishing `ST_PAUSED`; `VideoSession` waits for
  Paused before normal playback. A blocking decoder open/first read should normally delay video
  too, not create video-first silence.
- `Engine::fill` stops after more than eight empty reads, zero-fills the rest of the requested
  WASAPI region, releases the whole region, and advances `frames_since_base` by its full size.
  Decode errors and EOS reach the same zero-fill path. Generated silence can therefore count as
  played media time, matching the symptom: video and clock advance together, then audio arrives
  without later drift.
- `MfAudioDecoder::next_chunk` discards sample timestamps and all Source Reader flags except EOS.
  A no-sample result is labelled a gap tick without proving it represents a real timeline gap.

The first task note's decoder-warm-up theory is therefore unconfirmed. Do not amplitude-gate the
fix: legitimate source PCM can be all zero. “Ready” must mean source-derived frames, or an explicit
timestamped source gap, were prerolled.

## Hypotheses to distinguish

1. Fabricated startup silence is accepted as preroll after no-sample reads, a suppressed error,
   or another partial-fill condition.
2. Source PCM is queued correctly, but the cold Windows endpoint presents it late. The hard-coded
   200 ms buffer request or endpoint power-up may contribute.
3. The Resume command arrives late. The current 20 ms poll bound cannot explain a full second, but
   it is cheap to timestamp.
4. The exact production float/resample first read is slow despite the PCM probe.

## Constraints

- Keep decode, endpoint setup, and waits off the winit event loop.
- Diagnostics are opt-in console/tracing only; never persist paths, PCM, or media-derived data.
- Do not treat amplitude as readiness.
- Do not keep the endpoint permanently awake without an accepted measured power/resource tradeoff.
- Decode failure is not EOS; generated underrun silence is not decoded media time.
- Preserve the current `VideoAudio` interface unless evidence requires a real Buffering state.

## Phase 0 — Instrument before changing behavior

Add an opt-in `PB_AUDIO_STARTUP_TRACE=1` monotonic timeline keyed by `session_id`. Record:

1. audio thread spawn, endpoint activation, mix format, Initialize duration, actual buffer frames,
   and default/device period;
2. MF reader open and media-type negotiation durations;
3. the first 32 ReadSample calls: duration, flags, timestamp, sample duration, bytes/frames, and
   result (PCM, no sample, EOS, error);
4. each startup fill: requested frames, source frames, explicit source-gap frames, generated zero
   frames, reason, padding, and elapsed time;
5. ST_PAUSED publication, Resume send/receive, IAudioClient::Start return, first render event,
   and first audio-clock advancement;
6. VideoSession's Playing transition and first video present.

Add a small opt-in WASAPI loopback harness to timestamp first actual endpoint output. Padding or
IAudioClock only proves Windows consumed frames, not that sound reached the output. Cross-correlate
the captured onset with reference-decoded PCM so the test detects dropped track-leading samples.

Run at least ten cold and ten warm starts for the owner clip, `color_with_tone.mp4`, one legacy
MJPEG/PCM AVI from the ae6f412 scenario, a real-silent-start fixture, a no-audio clip, and the
in-memory archive fixture.

| Observation | Fix branch |
|---|---|
| Generated zeros precede source PCM | Phase 1A |
| PCM is queued but loopback begins late | Phase 1B |
| Resume arrives materially late | Phase 1C |
| First production ReadSample takes about 1 s | Phase 1D |

## Phase 1A — Source-aware preroll (expected branch)

1. Replace `next_chunk -> Option<Vec<f32>>` internally with typed results carrying timing:
   `Pcm { samples, pts, duration }`, `StreamTick { pts }`, and `End`. Propagate errors and inspect
   actual MF flags.
2. Decode into a bounded reusable staging buffer before acquiring a WASAPI render region; never
   hold GetBuffer across a potentially blocking decode.
3. Make fill return source frames, explicit-gap frames, generated underrun frames, and reason.
   Reset an empty-read guard after data; use a time/cancellation bound instead of a lifetime total
   of eight reads.
4. Publish ST_PAUSED only after enough source-timeline data is prerolled. All-zero source PCM
   qualifies; a real stream gap qualifies only for its timestamped duration.
5. Never advance the media clock for startup padding invented because data was unavailable. If a
   playing decoder underruns, pause/report buffering rather than running the master clock through
   fabricated media.
6. Apply the same rule to seek and replay: Reset, seek, source-aware preroll, then resume/ack.

## Phase 1B — Endpoint latency (only if loopback proves it)

1. Compare the requested 200 ms buffer with the actual shared-mode buffer and engine period.
2. A/B current IAudioClient setup against the default shared period and IAudioClient3 shared
   periods where available. Choose from measurements, not buffer-size intuition.
3. A/B a bounded open-time Start/Stop warm-up. Reject continuous silent rendering unless it is
   the only measured fix and its resource/power cost is accepted.
4. Keep the endpoint choice behind a small strategy seam until the corpus shows no underruns.

## Phase 1C — Command wake-up (only if proven)

Wake the audio thread with a command event included in its wait set so Resume, Pause, Seek, and
teardown do not wait for the 20 ms poll. This improves responsiveness but is not itself a credible
one-second explanation.

## Phase 1D — Decoder preroll (only if proven)

Keep a slow first production ReadSample inside paused-open/preroll and do not report ready early.
Consider parallel audio/video open only if it improves measured P-to-first-frame; do not add
speculative dwell prewarming or duplicate source reads.

## Tests (write first)

Extract preroll/accounting policy from COM/WASAPI and script these cases:

- more than eight no-sample results followed by PCM never mark fabricated silence ready;
- all-zero source PCM does mark ready;
- a timestamped stream tick inserts only its represented duration;
- successful PCM resets the empty-read guard;
- decode error becomes Failed, not EOS plus endless silence;
- partial chunks neither drop nor duplicate frames;
- generated underrun frames do not advance the media clock;
- mute outputs silence while source accounting and clock still advance;
- seek/replay clears old staged data and requires new-generation preroll.

Extend the opt-in Windows audio test with `PB_AUDIO_PLAY_CLIP=<path>` and structured onset
metrics. Cover path and VideoInput::Bytes, AAC, PCM, mute, pause/resume, seek/replay, no endpoint,
decoder failure, and no-audio. Keep CI headless and silent.

## Acceptance

- Ten local-console cold starts of SANY0501.MP4 queue no generated startup frames before source
  PCM. Loopback onset is within one measured engine period plus one display refresh of the first
  moving frame, with a 100 ms p95 hard ceiling.
- First output corresponds to source PTS 0 within one AAC packet (21.33 ms); no track start is lost.
- Warm p95 does not regress. P-to-first-moving-frame regresses by no more than one refresh.
- Steady-state A/V offset remains within 50 ms; pause/seek/replay do not add discontinuities.
- Legacy MJPEG/PCM AVI, archive bytes, mute, no-endpoint, and no-audio behavior remain correct.
- No event-loop blocking or passive disk write is introduced.

## Expected files

- `crates/pb-decode/src/mf_audio.rs`: typed reads, timestamps/flags, diagnostics.
- `crates/pb-app/src/wasapi_audio.rs`: tracing, preroll/fill/clock accounting, endpoint strategy.
- `crates/pb-app/src/video_audio.rs`: opt-in integration harness and thin state mapping.
- `crates/pb-app-core/src/video_session.rs`: only if real audio buffering must reach the core.
- `CHANGELOG.md`: add a user-facing Fixed entry when the fix lands, not for this plan.

## Non-goals

No Linux/PipeWire or macOS rewrite, codec replacement, DSP/mixer work, exclusive-mode WASAPI,
always-on endpoint keepalive, or unrelated steady-state playback redesign.
