# Seek robustness — fix the pre-roll clock, and make seeking regression-testable

> Status: **PLAN — not started** · Owner: JD
> Scope: the **macOS sample-buffer route** (MKV/WebM), which is what `97f80bb4` changed.
> AVPlayer (MP4/MOV) owns its own clock and is not implicated — *confirm that first*
> (§0), because it splits the diagnosis in half.

## The report (owner, 2026-07-15)

Three symptoms, all on seek:

1. **Seeking pauses playback.** It was quick before; now it stops.
2. **The scrubber flashes**: click ahead → jumps to the target → snaps back to where it
   was → lands on the target again.
3. **Audio sometimes disappears entirely** after a seek.

They are almost certainly **one bug wearing three hats**, and the hat is the clock.

## Where this came from — and where it did *not*

`97f80bb4` ("forward seek jumped BACK to the keyframe", 2026-07-15) rewrote the
sample-buffer seek. Before it, `demux_seek` landed on the keyframe *at or before* the
target and the clock anchored **there**, so `→` sent the film backwards. The fix: decode
from the keyframe forward, mark every frame before the target `DoNotDisplay`, and anchor
the clock at the **target**.

`.taskmaster/current-status.md` (rev 6) flagged it, verbatim:

> **Forward seek fix is UNVERIFIED end-to-end.** If a forward seek shows a brief smear
> before settling, the pre-roll's reference frames are being dropped and the clock needs
> holding differently.

That prediction has now come true, with interest.

**Ruled out — the audio-track work (#99).** Checked before blaming it, not after: the
audio branch is *purely additive* to `SampleBufferPresenter`, touches **zero** of
`performSeek` / `onDecodeAnchor` / `pendingRate` / `seekTargetSecs`, and its decoder change
(`open_track(…, None)`) falls straight through to the old `select_audio_stream` call.
`seek`/`read`/discard are untouched. The seek path on today's `main` is byte-for-byte what
`97f80bb4` left.

## The mechanism (read this before touching anything)

The seek, as it stands today:

```
performSeek(target):
    pendingRate = (rate != 0) ? 1 : 0
    synchronizer.rate = 0                    ← THE CLOCK IS HELD
    audioFeeder.seek(target)                 → flush, session_audio_seek, re-base ptsFrames
    reader.seek(target):
        layer.flush(); demux_seek(target)    → lands on the keyframe AT OR BEFORE target
        seekTargetSecs = target; firstFrameSent = false; armFeeding()

provide()  (on the reader queue, driven by layer.requestMediaDataWhenReady):
    while layer.isReadyForMoreMediaData:
        preroll = pts < target - 0.001
        if preroll: markDoNotDisplay(sb)
        layer.enqueue(sb)
        if !firstFrameSent && !preroll:
            onFirstFrame(anchor = target) → onDecodeAnchor:
                synchronizer.setRate(pendingRate, time: anchor)   ← THE ONLY UNHOLD
```

**`onDecodeAnchor` is the only thing that ever restores the rate**, and it only fires on the
first frame whose `pts >= target`. Everything before that is pre-roll.

### H1 — pre-roll starvation deadlock (primary hypothesis; explains all three)

`AVSampleBufferDisplayLayer` only reports `isReadyForMoreMediaData == false` once its
internal queue fills, and it only drains that queue **as the clock advances**. With
`synchronizer.rate == 0`, it never drains.

So if the pre-roll (keyframe → target) is **deeper than the layer's queue**, the feed loop
stalls before ever reaching a `pts >= target` frame. The anchor never fires. The rate is
never restored. **Playback is paused forever, and the only thing that could un-pause it is
the thing that starved.**

This predicts every symptom, and predicts them *together*:

- **(1) pause** — the rate is never restored.
- **(3) audio gone** — the audio renderer is on the *same* synchronizer. Rate 0 = silence.
  Not a separate bug.
- **(2) scrubber flash** — `synchronizer.currentTime()` stays at the **pre-seek** position
  for the whole pre-roll (rate 0 doesn't move or reset it). The scrubber drops `dragFraction`
  on release and shows `videoFraction`, which is still the old spot — so it snaps back, and
  only jumps to the target when/if the anchor lands. The flash *is* the stall, made visible.

It is **GOP-length dependent**, which explains "sometimes": a 1 s GOP fits in the queue and
works; a 5 s GOP at 24 fps is ~120 frames of pre-roll and does not.

### H2 — anchor/audio-base mismatch (secondary; survives even if H1 is fixed)

`97f80bb4` changed the **video** anchor to `target` but left audio re-basing to where the
**audio decoder** landed (`AudioSampleFeeder.seek`: `ptsFrames = landed * rate`). Audio seeks
are near-exact and video's target is exact, so these usually agree — but they are now two
independent notions of "where we are", and nothing asserts they match. Any drift enqueues
audio at timestamps the synchronizer has already passed → dropped → silence.

### H3 — the flash is partly unavoidable latency

Even with H1 fixed, a long-GOP pre-roll takes real time to decode. If the clock stays held
throughout, the scrubber will always snap back for that duration. **The scrubber must not
report the pre-seek position while a seek is in flight** — that is a UI-truth bug independent
of the decode.

## §0 — Confirm the split before fixing (one run, ~2 minutes)

Do not skip this. It is cheap and it can invalidate the whole diagnosis.

```sh
PB_TRACE=1 "target/swift-host/debug/Blaze Viewer.app/Contents/MacOS/Blaze Viewer" 2>&1 \
  | grep -E "sb-demux|seek anchor"
```

Seek once forward on a long-GOP MKV (Ad Astra), and read:

| Observation | Verdict |
|---|---|
| A run of `preroll=true`, **no** `seek anchor at target` line | **H1 confirmed** — starvation deadlock |
| `seek anchor at target` fires, but audio still drops | **H2** — anchor/audio-base mismatch |
| Reproduces on **MP4** too | Diagnosis is **wrong** — AVPlayer owns its own clock and shares none of this code |

Also record: does it reproduce on a **short-GOP** file? H1 predicts *no*. That asymmetry is
the strongest single piece of evidence available, and it costs one more seek.

## The fix (shape, pending §0)

**If H1: stop holding the clock through the pre-roll.** Anchor at the target *immediately*
in `performSeek` — `setRate(pendingRate, time: target)` before feeding — so the layer has a
clock, drains, and keeps asking for data. The pre-roll frames are already `DoNotDisplay` and
already in the past, so they are consumed and discarded as fast as VideoToolbox decodes
them; the first displayable frame is at the target and shows on time. The picture holds the
last frame meanwhile (the existing reveal rule).

Two risks to design against, both real:

- **Audio may start before the picture catches up** on a long pre-roll. Options: keep the
  *audio renderer* muted/held until the first displayable frame while the clock runs; or
  bound the pre-roll (below).
- **Pre-roll longer than real time.** If decode can't outrun the clock, the first displayable
  frame arrives late and is dropped — the smear the status doc predicted. A **pre-roll budget**
  is the honest answer: if `target - keyframe` exceeds ~N seconds, accept landing at the
  keyframe (or seek to the *next* keyframe) rather than promising an exact landing we cannot
  decode in time. Sub-GOP accuracy is not worth a stall.

**H2 regardless:** make one side the authority. The audio feeder should re-base to the
**same** value the video anchors at, not to its own landing — or the two must be asserted
equal within a tolerance and reconciled. Two independent "where we are" values is the defect,
whatever the numbers currently do.

**H3 regardless:** the scrubber must show the **seek target** while a seek is in flight, not
`videoFraction`. `VideoScrubber` already pins to `dragFraction` during a drag; it needs to
keep pinning until the seek lands rather than dropping to the player's stale clock on
release. This kills the flash even if the decode stays slow.

## Regression testing — the actual deliverable

The seek bug shipped because **nothing could have caught it**: the decision logic lives inside
a Swift feed loop, entangled with `AVSampleBufferDisplayLayer`, and there is no Swift test
target. That is the thing to fix, not just the symptom.

### T1 — Extract the pre-roll/anchor decision into a pure function (highest value)

Today the rule is smeared across `DemuxReader.provide`. It is *pure logic* wearing I/O:

> given (`seekTarget`, frame `pts`, `dts`, `firstFrameSent`) → (is this pre-roll? do we
> anchor? at what time?)

Extract it — ideally into **`pb-app-core`** (Rust, where the test culture already is; the
reader can call it over the existing FFI), or failing that a pure Swift struct with no
AVFoundation types. Then the interesting cases become ordinary unit tests:

- forward seek inside a GOP → pre-roll marked, anchor at target (**the `97f80bb4` bug**)
- backward seek → lands before target, same rule
- seek to a target *before* the first keyframe → anchors immediately, no pre-roll
- **seek past EOS** → must anchor anyway (the existing "never leave the clock held" guard)
- initial playback (`seekTarget == nil`) → anchors at **DTS**, not PTS (the negative-DTS
  B-frame rule — currently a comment, should be a test)
- a pre-roll longer than the budget → whatever §0's fix decides

### T2 — A headless Swift harness (new capability; **proven to work**)

`swift script.swift <fixture>` drives real AVFoundation objects outside the app. This is not
speculative: it is exactly how the **`propertyList` bug** was found today — a 100 %-dead
AVPlayer audio path that had passed review, compiled clean, and would have shipped. It took
twenty minutes and one purpose-built fixture.

Build `mac/Tests/seek-harness.swift` to drive a real `AVSampleBufferRenderSynchronizer` +
`AVSampleBufferDisplayLayer` + `DemuxReader` against a fixture and assert the properties that
actually matter:

- after `seek(to: T)`, **`synchronizer.rate` returns to its pre-seek value within N seconds**
  (this is symptom 1, directly — and it is a *timeout*, which is what a deadlock looks like)
- **`currentTime()` reaches ≈T** and does not sit at the old position
- audio and video anchor to the **same** time (H2)
- no frame with `pts < T` is ever displayed (the pre-roll contract)

Run it against **both** a short-GOP and a **long-GOP** fixture — the long one is the whole
point, since H1 is GOP-dependent and a short fixture would pass while the bug is live.

> ⚠ The harness must import the reader, which links Rust. If that proves awkward from a bare
> script, the fallback is a `swift-testing`/XCTest target in `mac/Package.swift` — more setup,
> same idea. **Do not let the packaging question kill the harness**; it is the only thing that
> can test the real thing.

### T3 — Fixtures, built not found

Generate with `ffmpeg` and check in (they are tiny), rather than depending on
`/Volumes/Media`, which CI and a fresh clone do not have:

```sh
# long GOP (~5 s) — the H1 repro; keyint 120 @ 24 fps
ffmpeg -f lavfi -i testsrc=size=320x240:rate=24:duration=30 -f lavfi -i sine=frequency=440:duration=30 \
  -c:v libx264 -g 120 -keyint_min 120 -sc_threshold 0 -c:a aac -shortest longgop.mkv
# short GOP (~0.5 s) — the control; should pass before AND after
ffmpeg -f lavfi -i testsrc=size=320x240:rate=24:duration=30 -f lavfi -i sine=frequency=440:duration=30 \
  -c:v libx264 -g 12 -keyint_min 12 -sc_threshold 0 -c:a aac -shortest shortgop.mkv
```

A frame-accurate visual check needs a **burnt-in timecode** (`drawtext`), so a seek to T can
be asserted against the picture rather than trusted.

### T4 — Keep the Rust half honest

`demux.rs` already has `seek_returns_to_readable_keyframe`. Extend it to state the property
the Swift side *depends on* and currently only assumes: **`demux_seek(t)` lands at or before
`t`, never after.** The whole pre-roll design rests on that; nothing asserts it.

## Order of work

1. **§0** — one traced seek. Confirms H1/H2, or refutes the lot. Do not start at step 2.
2. **T1** — extract the decision function + tests. Valuable whatever §0 says, and it makes
   the fix reviewable instead of a guess.
3. **The fix** — per §0's verdict.
4. **T3 → T2** — fixtures, then the harness. The harness is what stops this recurring.
5. **T4** — the Rust seek property.
6. Re-run §0's trace on the corpus (Ad Astra, Grey's Anatomy) and **have the owner confirm by
   ear and eye** — audio continuity and the scrubber are both things a test can only
   approximate.

## Notes carried in

- **Don't drive the app from a tool session while the owner is testing** — `pkill` kills their
  window, and a tool-launched bare binary comes up windowless (`--pb-open` is the workaround).
  The §0 trace is the owner's to run, or must be coordinated.
- The **winit/Windows** shell shares none of this: its video is the Session route, where audio
  is the master clock. Any fix here is macOS-only, and the Session route's own seek is a
  separate question.
- Two bugs in the #99 audio work were the *same shape* as H1 — **state cached at a moment when
  the facts were not in yet** (a flyout disabled before a video existed; a tick resolved against
  rows that had not been built). The subtitle picker has never had either, because it derives
  everything from the catalog at draw time. When fixing the seek, prefer **deriving at the
  moment of truth** over caching at the moment of the event.
