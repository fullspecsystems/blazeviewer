# Seek robustness — fix the pre-roll clock, and make seeking regression-testable

> Status: **PLAN v2 — §0 trace run; H1 (pre-roll starvation) REFUTED; the real pause bug (a
> desired-rate capture race) FIXED (2026-07-15, owner-verification pending). Residual: a ~2.3 s
> cold-seek freeze over SMB (S3 territory, not a deadlock).** · Owner: JD
> Scope: the **macOS sample-buffer route** (MKV/WebM), which is what `97f80bb4` changed.
> AVPlayer (MP4/MOV) owns its own clock and is not implicated — *prove that in §0*, because
> it splits the diagnosis in half.
>
> ## Progress (2026-07-15, session 1) — commits `360bf885`, `5433066a`, `a6b28544` on `main`
>
> **Done + verified (build/test green, NOT owner-verified on a display yet):**
> - **§0 instrumentation** — seek-scoped diagnostics, reset per seek. Emits (behind `PB_TRACE`):
>   first post-seek packet pts/dts, pre-roll count + last pre-roll pts, the first
>   `isReadyForMoreMediaData==false`, and **whether `provide` re-fires** — the direct H1
>   discriminator. (`DemuxReader.provide`/`seek`; `SampleBufferPresenter.performSeek`.)
> - **T1** — pure `SeekFramePolicy` extracted to a dependency-free **`mac/PbSeek`** package,
>   19 unit tests (`swift test --package-path mac/PbSeek`). `DemuxReader` now calls it.
> - **H1c** — `SeekContext` + epoch gate on every callback (`onDecodeAnchor`, audio completion,
>   reader completion). A stale seek can no longer re-anchor the clock.
> - **H2' (the shippable half)** — the resume-audio bug (audio now resumes with the picture at
>   open) + a seek issued before the audio decoder opens is **held and applied at open**, not
>   dropped; audio seek returns an epoch-tagged completion.
> - **H3** — scrubber pending-target pin (no more flash-back after a click-seek).
> - **T3/T4** — checked-in `longgop.mkv`/`shortgop.mkv` fixtures + demux seek-contract tests
>   (`cargo test -p pb-decode --features ffvideo`, 12 pass). **T5** — both run on the mac CI lane.
>
> **§0 VERDICT (2026-07-15, trace on Ad Astra, the 10.4 s-GOP MKV) — H1 REFUTED, root cause
> found and fixed.** The trace killed the pre-roll-starvation hypothesis outright: `provide`
> **re-fires** after `isReadyForMoreMediaData==false` (the display layer drains the
> DoNotDisplay pre-roll even at rate 0), and the `ANCHOR` **does** fire — no deadlock. The
> pause was a **desired-rate capture race**: a scrubber *click* issues two seeks to the same
> target (`onChanged` then `onEnded`, ~1 ms apart); the first sets `synchronizer.rate = 0` to
> hold the clock, and the second captured its post-seek rate from that **held** `synchronizer.rate`
> and read "paused", so the seek settled at rate 0 (= frozen picture **and** silence — the
> plan's "one bug, three hats", the hat being the clock's *rate capture*, not its drain). Fix
> (`SampleBufferPresenter`, this session): a preserved `desiredPlaybackRate` intent, updated
> only by play/pause/step/EOS and read **live at the anchor** — so consecutive seeks can't
> latch rate 0, and a pause pressed mid-seek sticks. **Owner-verification pending.**
>
> Residual (measured, NOT the pause): the first seek to a cold position costs ~2.3 s — SMB
> demux seek + a 154-frame pre-roll decode — a one-time freeze *before* playback resumes.
> That is the S3 "bound the pre-roll" / network-seek territory below, now clearly separable
> from the pause and no longer the headline.
>
> **NOT done (plan order 3, 5, 8):**
> - **T2 — renderer-harness proof gate** (does a compressed fixture decode headlessly?).
> - **S3 pre-roll budget / the ~2.3 s cold-seek freeze** (`SeekFramePolicy.effectiveTarget`'s
>   `budgetSecs`, `nil` today) — the residual above; needs corpus measurement. Not a deadlock.
> - **H1b clock-strategy spike (S1/S2/S3)** — was the presumed deadlock fix; **the trace shows
>   there is no deadlock to drain**, so this is demoted to the S3 pre-roll-budget optimization.
>
> **Repro corpus identified (`/Volumes/Media/Movies`, 2026-07-15):** the 184 `*.mkv` there are
> all 1080p BluRay **H.264 with a ~10.4 s GOP** (x264 default 250-frame keyint @ 23.976 fps —
> keyframes at 0 / 10.427 / 20.854 …) — *longer* than the synthetic `longgop.mkv`, and on an
> SMB share, so they stress the long pre-roll **and** the network-seek latency at once. Any one
> (e.g. `Ad.Astra.2019.…x264-Geek.mkv`) is a §0 trace subject. Run with `PB_TRACE=1`, open it,
> seek, and read the `sb-seek diag epoch=…` lines against the §0 verdict table.
>
> **v1 was not execution-ready.** A review found three ways its fix could ship while seeking
> stayed broken, and every finding was re-verified against the tree before this rewrite.
> Recorded honestly, because each one is a trap the next reader could fall into too:
>
> | v1 said | Reality |
> |---|---|
> | `setRate(pendingRate, time: target)` unblocks the pre-roll | `pendingRate` is **0** when paused, and `step()` *pauses before seeking* — so it fixes playing seeks and leaves paused seeks and `,`/`.` deadlocked, which is where the fix would have looked done |
> | §0's trace confirms H1 | `framesFed` is a **lifetime** counter (`<= 3 \|\| % 300`), never reset per seek — the trace would print almost nothing, and absence of an anchor cannot tell starvation from a demux stall |
> | H2 = video anchors at `target`, audio at its own landing | Rust's `FfAudioDecoder::seek` **returns the target**, not an independent landing — rebasing both to `target` is a no-op. The real audio risks are elsewhere (§H2') |
> | `demux_seek(t)` never lands after `t` (T4) | `demux.rs:236` **retries with `i64::MAX`** when nothing exists at/before the target — landing *after* it is a documented branch, not a violation |
>
> The through-line: **v1 asserted contracts the code does not honour.** Verify against the
> tree, not the comments.

## The report (owner, 2026-07-15)

Three symptoms, all on seek:

1. **Seeking pauses playback.** It was quick before; now it stops.
2. **The scrubber flashes**: click ahead → jumps to the target → snaps back to where it
   was → lands on the target again.
3. **Audio sometimes disappears entirely** after a seek.

Probably **one bug wearing three hats**, and the hat is the clock — but see §0: that is a
hypothesis, not a finding.

## Where this came from — and where it did *not*

`97f80bb4` ("forward seek jumped BACK to the keyframe", 2026-07-15) rewrote the
sample-buffer seek. Before it, `demux_seek` landed on the keyframe *at or before* the target
and the clock anchored **there**, so `→` sent the film backwards. The fix: decode from the
keyframe forward, mark every frame before the target `DoNotDisplay`, anchor at the **target**.

`.taskmaster/current-status.md` (rev 6) flagged it, verbatim:

> **Forward seek fix is UNVERIFIED end-to-end.** If a forward seek shows a brief smear
> before settling, the pre-roll's reference frames are being dropped and the clock needs
> holding differently.

**Ruled out — the audio-track work (#99).** Checked before blaming it: the audio branch is
*purely additive* to `SampleBufferPresenter`, touches **zero** of `performSeek` /
`onDecodeAnchor` / `pendingRate` / `seekTargetSecs`, and its decoder change
(`open_track(…, None)`) falls through to the old `select_audio_stream` call. The seek path on
today's `main` is byte-for-byte what `97f80bb4` left.

## The mechanism

```
performSeek(target):
    pendingRate = (rate != 0) ? 1 : 0        ← 0 WHEN PAUSED, and step() pauses first
    synchronizer.rate = 0                    ← the clock is held
    audioFeeder.seek(target)                 → flush, session_audio_seek, re-base ptsFrames
                                               ⚠ silently returns if ptr == 0 (audio not open yet)
    reader.seek(target):
        layer.flush(); demux_seek(target)    → keyframe at/before target (usually — see T4)
        seekTargetSecs = target; firstFrameSent = false; armFeeding()
        then(landed)                         ⚠ fires HERE — "demux command done", not "frame visible"
                                               and this is the ONLY epoch-gated callback

provide()  (reader queue, driven by layer.requestMediaDataWhenReady):
    while layer.isReadyForMoreMediaData:
        preroll = pts < target - 0.001
        if preroll: markDoNotDisplay(sb)
        layer.enqueue(sb)
        if !firstFrameSent && !preroll:
            onFirstFrame(anchor = target) → onDecodeAnchor:
                synchronizer.setRate(pendingRate, time: anchor)   ← the ONLY unhold, and NOT epoch-gated
```

Three separate defects live in that listing. They must be fixed together; any one alone
leaves a broken case that looks fixed.

### H1 — pre-roll starvation (hypothesis, **not yet measured**)

`onDecodeAnchor` is the only thing that restores the rate, and it only fires on the first
frame with `pts >= target`. `AVSampleBufferDisplayLayer` reports
`isReadyForMoreMediaData == false` once its internal queue fills, and drains as samples are
decoded/displayed. If the pre-roll (keyframe → target) outgrows that queue while the clock is
held, the feed stalls before reaching the target, the anchor never fires, and **the only thing
that could un-pause playback is the thing that starved.**

Predicts all three symptoms together: (1) the rate is never restored; (3) audio is on the
*same synchronizer*, so rate 0 = silence — not a separate bug; (2) `currentTime()` stays at
the pre-seek position for the whole stall, so the scrubber snaps back to it. GOP-dependent,
which is the "sometimes".

> ⚠ **Apple documents readiness as internal-queue occupancy that changes as samples are
> decoded/displayed. It does *not* guarantee that a stopped clock is the sole reason a queue
> cannot drain.** H1 stays a hypothesis until §0 measures it.

### H1b — the paused case, which H1's obvious fix does not reach

`pendingRate` is **0** for a paused seek, and `step(forward:)` sets `rate = 0` *before*
calling `performSeek`. So "anchor at the target and let it drain" — v1's fix — still hands the
layer a **non-advancing clock** for paused scrubbing and for `,`/`.`. It would have shipped
looking correct on the one case anyone tested.

The concepts must be separated:

- **`desiredRateAfterSeek`** (0 or 1) — the user's intent, preserved across the seek.
- **A pre-roll drain strategy that works regardless of desired rate** — the clock cannot be
  both "held so nothing shows" and "running so the queue drains".
- **A precise transition that parks at the landed frame** when the desired rate is 0.

Candidate strategies (the §"clock-strategy spike" A/Bs these; do not pick one on paper):

- **S1 — run the clock through the pre-roll, then re-park.** `setRate(1, time: …)` to drain,
  then `setRate(0, time: landed)` on the first displayable frame. Risk: a paused step briefly
  runs the clock; audio must be held/muted through it.
- **S2 — drain without the clock.** If the layer can be made to consume `DoNotDisplay`
  samples with the rate at 0, no strategy is needed — **this is exactly what §0 must
  measure**, and if true, H1 is wrong and the bug is elsewhere.
- **S3 — bound the pre-roll.** If `target - keyframe` exceeds a budget, land at the keyframe
  (or the next one) instead of promising accuracy we cannot decode in time. Sub-GOP accuracy
  is not worth a stall. Needed as a backstop under S1 regardless.

**Mandatory cases before choosing:** playing seek · paused seek · frame-step forward ·
frame-step backward · seek after EOS.

### H1c — the landing callback is not generation-gated (independent bug)

`seekEpoch` gates only `reader.seek`'s completion — which fires right after `demux_seek`,
meaning *"the demux command finished"*, **not** *"the target frame is visible"*. The callback
that actually moves the clock, `onDecodeAnchor`, carries **no epoch** and unconditionally
sets rate and time.

The scrubber issues **up to ~16 seeks/second** (`seekInterval = 0.06`). A stale anchor
overwriting a newer seek is not a corner case; it is the expected traffic. This is a live bug
today, independent of H1, and it would survive any pre-roll fix.

**Introduce a `SeekContext`** — `{ epoch, target, desiredRateAfterSeek, source }` — carried
through the video anchor, the audio completion, EOS, failure, and the UI landing. **Every**
callback rejects a stale epoch *before* touching the clock, the renderers, or the scrubber.

### H2' — audio coordination (v1's H2 was the wrong mismatch)

v1 claimed video anchors at `target` while audio re-bases to its own landing. It does not:
`FfAudioDecoder::seek` **forward-discards to the requested target and returns that target**
(`audio_decoder.rs:411`). Rebasing both to `target` would change almost nothing.

The real risks, all verified:

- **Audio and video seek on independent queues with no readiness barrier.** Nothing makes the
  clock wait for the audio flush/seek to complete.
- **A seek before the audio decoder opens is silently discarded** — `AudioSampleFeeder.seek`
  is `guard self.ptr != 0 else { return }`. The seek is simply lost; nothing retries it.
- **`startSecs` seeks the video reader only** — and this one is worth pulling out on its own:

  > ### ⚠ The resume-audio bug — separately shippable, and new **today**
  >
  > `audioFeeder.open` enqueues **0-based** (`SampleBufferPresenter.swift:152`). Then
  > `startSecs` seeks **only `reader`** (`:178` — the video). Then the first frame anchors the
  > clock at **`startSecs`**. So on a resumed film the audio is sitting ~`startSecs` *in the
  > past* and the renderer discards it; it must grind forward through the whole gap before a
  > sample is ever in-window.
  >
  > **Introduced by `3b12ec69` (2026-07-15) — the MKV-resume fix, same day as `97f80bb4` but a
  > different commit.** Before it, this route never resumed at all: video *and* audio both
  > started at 0, so they agreed. The resume fix moved the picture and left the sound behind.
  >
  > It therefore explains **audio loss on a *resumed* MKV** — and nothing else. It is not the
  > seek regression, and it cannot explain any A/V problem older than today (those belong to
  > the overhaul's R2/R4/R5 — `.taskmaster/docs/video-playback-overhaul.md`).
  >
  > Fix independently of the seek work: seek the audio feeder to `startSecs` too — or, since
  > audio may not be open yet, **queue it as a pending seek applied at open** (which is the
  > same machinery H2' needs anyway, so build it once).

- **Audio seek failure is not reported** to the presenter as a typed result.

**Replace H2 with a coordination contract:** `AudioSampleFeeder.seek` returns an
epoch-tagged completion/result; a seek issued before audio opens is **held and applied at
open**, not dropped; the clock/unmute waits on an explicit A/V readiness condition; failures
surface.

### H3 — the scrubber has no landing lifecycle

Fractional scrub seeks have **no render-landing callback at all** — the reader completion
means "demux command done". So "pin until the seek lands" has nothing to land on.

Define pending UI state as `(session, epoch, targetFraction)`:

- set/updated on each fractional seek;
- while pending, progress publications **cannot** overwrite the displayed target;
- cleared only on the current visual landing, failure, EOS, or teardown;
- a superseding seek **replaces** it.

Keep AVPlayer's scrubber behaviour separate unless §0 shows it needs the same treatment.

## §0 — Instrumentation first, then measure (nothing is fixed before this)

v1's §0 could not have worked: `framesFed` is a **lifetime** counter (`framesFed <= 3 ||
framesFed % 300 == 0`), never reset per seek, so "look for a run of `preroll=true`" would
print nothing. And an absent anchor cannot distinguish starvation from a demux stall, a
renderer failure, or a superseded seek.

**Add seek-scoped diagnostics** (one line per seek, reset per seek):

- epoch · target · `desiredRateAfterSeek` · source (arrow / scrub / step / resume)
- first post-seek keyframe PTS **and** DTS
- pre-roll frame count · last pre-roll PTS
- the first `isReadyForMoreMediaData == false`, and **whether the callback is ever invoked
  again** ← *this is the direct H1 discriminator*
- audio seek start / completion / result
- anchor · EOS · failure · elapsed seek time

Then one seek on a long-GOP MKV tells us:

| Observation | Verdict |
|---|---|
| pre-roll count climbs → `isReady == false` → **callback never re-invoked** → no anchor | **H1 confirmed** — starvation |
| callback re-invoked, anchor fires, audio still gone | **H2'** — coordination / the `startSecs` resume bug |
| anchor fires but a *stale* one lands last during scrubbing | **H1c** — epoch gating |
| reproduces on **MP4** | Diagnosis **wrong** — AVPlayer shares none of this code |
| **short-GOP** file is fine, long-GOP is not | Strong H1 corroboration (costs one extra seek) |

## Testing — the actual deliverable

The bug shipped because **nothing could have caught it**: the decision logic lives inside a
Swift feed loop entangled with `AVSampleBufferDisplayLayer`, and there is no Swift test target.

### T1 — Extract a pure `SeekFramePolicy` — **in Swift**

Given (`seekTarget`, `pts`, `dts`, `firstFrameSent`) → (pre-roll? anchor? at what time?).

> v1 wanted this in Rust (`pb-app-core`). **Rejected:** it is macOS-specific
> `AVSampleBufferDisplayLayer` policy, not shared navigation semantics, and it would put an
> **FFI call on every compressed frame** of the decode path to serve exactly one platform.
> Keep it as a pure Swift struct over primitive numbers, in a testable Swift target, with
> AVFoundation conversion at the edge.

Cases: forward seek inside a GOP (the `97f80bb4` bug) · backward seek · target before the
first keyframe · **seek past EOS** (must anchor anyway) · initial playback (`seekTarget ==
nil`) anchors at **DTS**, not PTS (the negative-DTS B-frame rule — currently a comment) ·
pre-roll beyond budget.

> ⚠ **This unit test cannot catch the deadlock.** It validates *classification*; the deadlock
> is *progress*, and only the harness (T2) can see it. Do not let a green T1 read as "seek
> works".

### T2 — Renderer harness, behind a **proof gate**

The `propertyList` script proved AVFoundation is drivable from a script — it did **not** prove
a display layer and synchronizer will decode and drain **without a live layer tree**.

**Gate: before building the harness, prove one compressed fixture decodes and exposes a landed
pixel/timestamp headlessly.** If that fails, the harness needs a window and the design changes.

Then assert what matters:

- after `seek(to: T)`, **`rate` returns to `desiredRateAfterSeek` within N seconds** ← a
  *timeout*, which is what a deadlock looks like
- `currentTime()` reaches ≈T and does not sit at the old position
- audio and video anchor to the **same** time
- **no frame with `pts < T` is ever displayed** — ⚠ *define how this is observed;* clock
  position alone does not prove it

Prefer a real SwiftPM test target + `swift test` over a permanent loose script.

> **Deprecation:** macOS 14 is the floor, and Apple has deprecated the display-layer
> readiness/enqueue APIs in favour of `AVSampleBufferVideoRenderer` /
> `sampleBufferRenderer`. This bug fix must **not** become that migration — but the harness
> must **deliberately choose** whether it tests the legacy app path or the replacement, and
> say so, or it will test something the app does not do.

### T3 — Fixtures, generated not borrowed

Checked in (they are tiny), not `/Volumes/Media`, which CI and a fresh clone lack:

```sh
# long GOP (~5 s) — the H1 repro
ffmpeg -f lavfi -i testsrc=size=320x240:rate=24:duration=30 -f lavfi -i sine=frequency=440:duration=30 \
  -c:v libx264 -g 120 -keyint_min 120 -sc_threshold 0 -c:a aac -shortest longgop.mkv
# short GOP (~0.5 s) — the control; must pass before AND after
ffmpeg -f lavfi -i testsrc=size=320x240:rate=24:duration=30 -f lavfi -i sine=frequency=440:duration=30 \
  -c:v libx264 -g 12 -keyint_min 12 -sc_threshold 0 -c:a aac -shortest shortgop.mkv
```

Burn in a timecode (`drawtext`) so a seek to T is asserted against the **picture**, not trusted.

> ⚠ **320×240 fixtures validate correctness; they cannot choose the pre-roll budget.** That
> needs measurement on representative **1080p/4K H.264 and HEVC** corpus files.

### T4 — The Rust demux contract, as it actually is

v1 asserted `demux_seek(t)` never lands after `t`. **False:** `demux.rs:236` retries with
`i64::MAX` when nothing exists at/before the target (a start-offset edge), which is a
deliberate branch. The real contract:

- **normal:** keyframe at or before target;
- **fallback:** no earlier keyframe → the first usable keyframe, which **may be after** target;
- **expose or record the actual first post-seek packet timestamp** instead of `reader.seek`
  fabricating `CMTime(target)` — the pre-roll rule and the anchor both depend on the real one;
- test **both** branches.

`seek_returns_to_readable_keyframe` today only seeks to **zero** — it does not exercise the
property the Swift side leans on.

### T5 — CI can't run any of this yet

`mac-swift` builds `--no-ffvideo` (`ci.yml:219`) — deliberate, historical. **The demux harness
cannot run there.** Adding a macOS lane built with `ffvideo` is part of this work, not an
afterthought; without it the harness rots the first time someone forgets to run it. (Note the
runner reality: `mac-swift`/`linux-gate` currently queue forever — **no macOS self-hosted
runner is registered** — so this lands green only after that is fixed.)

## Acceptance gates

Before owner verification, all of:

- playing **and paused** seeks on short- and long-GOP H.264 MKV
- rapid **A→B→C** scrubbing, **only C** allowed to anchor
- frame-step forward **and** backward while paused
- initial resume at nonzero `startSecs` **with audio**
- **seek issued before audio open completes**
- seek near EOS · exactly on a keyframe · before the first usable keyframe
- audio-seek failure and video-decode failure both **clear pending state**
- a **measured** pre-roll policy on 1080p/4K H.264 + HEVC corpus files

## Order of work

1. ✅ **Instrumentation** (§0) — seek-scoped diagnostics. Nothing is fixed before this. *(done,
   `360bf885`)*
2. ✅ **`SeekContext` state machine + pure Swift `SeekFramePolicy` tests** (H1c, T1) — a live bug
   today, and it makes the fix reviewable rather than a guess. *(done, `360bf885`)*
3. 🔲 **Renderer-harness proof gate** (T2) — before committing to the harness design. *(owner-gated;
   not started — the deprecation choice (legacy display-layer vs `AVSampleBufferVideoRenderer`)
   and the headless-decode proof are still open.)*
4. ◐ **A/V coordination** (H2') — the readiness contract, the pending-seek-before-open, and the
   `startSecs` audio resume. *(the pending-seek-before-open and the resume-audio fix landed in
   `360bf885`; the explicit A/V readiness barrier that holds the clock/unmute until both
   renderers are ready is entangled with the clock strategy → deferred to step 5.)*
5. 🔲 **Clock-strategy A/B spike** (H1b) — S1/S2/S3 against all four mandatory cases. **Do not
   pick the implementation on paper.** *(owner-gated — THE headline deadlock fix; blocked on
   step 1's trace over the `/Volumes/Media/Movies` long-GOP corpus.)*
6. ✅ **UI pending target** (H3). *(done, `360bf885`)*
7. ✅ **Fixtures + CI lane** (T3, T5). *(done, `5433066a` + `a6b28544`; ⚠ fixtures have no
   burned-in timecode — this box's ffmpeg lacks `drawtext` — so the §T2 visual harness must
   regenerate them on a libfreetype build. The generator script does it automatically.)*
8. 🔲 **Corpus measurement + owner verification** — audio continuity and the scrubber are things
   a test can only approximate. *(owner-gated.)*

### Resume here (session 2)
Run the **§0 trace** (order 1) on a `/Volumes/Media/Movies` long-GOP MKV — `PB_TRACE=1`, seek,
read the `sb-seek diag` lines against the §0 verdict table — to **confirm or refute H1**. That
verdict is the gate for the **clock-strategy spike** (order 5), which is the actual deadlock
fix. Everything else already landed. Don't drive the app from a tool session while the owner is
testing (`pkill` kills their window; a tool-launched bare binary is windowless — use `--pb-open`).

## Notes carried in

- **Don't drive the app from a tool session while the owner is testing** — `pkill` kills their
  window, and a tool-launched bare binary comes up windowless (`--pb-open` is the workaround).
  §0's trace is the owner's to run, or must be coordinated.
- The **winit/Windows** shell shares none of this: its video is the Session route, where audio
  *is* the master clock. Any fix here is macOS-only.
- Two bugs in the #99 audio work were the same shape as H1 — **state cached at a moment when
  the facts were not in yet** (a flyout disabled before a video existed; a tick resolved
  against rows not yet built). The subtitle picker has had neither, because it derives from the
  catalog at draw time. Prefer **deriving at the moment of truth** over caching at the moment
  of the event.
- **The lesson of v1:** every contract it asserted (`demux_seek` never overshoots; audio
  reports its own landing; the trace prints per-seek) was contradicted by the code. Read the
  implementation, not the comment.
