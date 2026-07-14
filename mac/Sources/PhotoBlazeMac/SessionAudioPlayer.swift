import AVFoundation
import PBCatch
import PbMacFfi

/// Exclusive owner of the Rust session-audio decoder pointer (plan §7/1E). Every
/// decoder operation — open, read, seek, free — runs on `queue`, a dedicated
/// serial feeder queue, so there is never a concurrent read and seek and the
/// pointer **never touches the main actor** (R5: heavy TrueHD/seek/refill work off
/// the UI + pump). swift-bridge 0.1.59 can't return an owned opaque type, so the
/// Rust side hands out a raw `usize`; this class is its sole owner and frees it
/// **exactly once** in `deinit`.
///
/// Callbacks are delivered back on the main actor in FIFO order (one
/// `DispatchQueue.main.async` per result), so buffers schedule in decode order.
final class OwnedAudioDecoder: @unchecked Sendable {
    /// The serial feeder queue that owns `ptr`. `userInitiated` — audio refill is
    /// latency-sensitive, but it is off the main thread.
    private let queue = DispatchQueue(
        label: "ca.fullspec.photoblaze.session-audio", qos: .userInitiated)
    /// Nonzero once opened; `0` = unopened / open failed. Touched **only** on `queue`
    /// (and in `deinit`, when no other reference — hence no queue work — remains).
    private var ptr: UInt = 0

    /// Open the decoder for `sessionId` over the container `StartVideoAudio`
    /// stashed Rust-side, off the main actor. `then` runs back on the main actor
    /// with `(ok, rate, channels)`; rate/channels are `0` on failure.
    func open(sessionId: UInt64, then: @escaping @Sendable (Bool, UInt32, UInt32) -> Void) {
        queue.async { [weak self] in
            guard let self else {
                DispatchQueue.main.async { then(false, 0, 0) }
                return
            }
            let p = open_stashed_session_audio(sessionId)
            self.ptr = p
            let ok = p != 0
            let rate = ok ? session_audio_rate(p) : 0
            let channels = ok ? session_audio_channels(p) : 0
            DispatchQueue.main.async { then(ok, rate, channels) }
        }
    }

    /// Decode up to `maxFrames` interleaved f32 frames plus the post-read stream
    /// state (`0` Ok, `1` Eof, `2` Failed — R12), delivered together on the main
    /// actor. The `RustVec` is copied into a Swift array on `queue` and freed there.
    func read(_ maxFrames: UInt32, then: @escaping @Sendable ([Float], UInt8) -> Void) {
        queue.async { [weak self] in
            guard let self, self.ptr != 0 else {
                DispatchQueue.main.async { then([], 2) }
                return
            }
            let rv = session_audio_read(self.ptr, maxFrames)
            let n = Int(rv.len())
            let samples =
                n > 0 ? Array(UnsafeBufferPointer(start: rv.as_ptr(), count: n)) : []
            let state = session_audio_state(self.ptr)
            DispatchQueue.main.async { then(samples, state) }
        }
    }

    /// Seek off the main actor; `then` gets the new clock anchor (seconds) — the
    /// host's post-seek scheduling epoch.
    func seek(_ seconds: Double, then: @escaping @Sendable (Double) -> Void) {
        queue.async { [weak self] in
            guard let self, self.ptr != 0 else {
                DispatchQueue.main.async { then(seconds) }
                return
            }
            let anchor = session_audio_seek(self.ptr, seconds)
            DispatchQueue.main.async { then(anchor) }
        }
    }

    deinit {
        // Free exactly once, on the feeder queue (libav requires external
        // synchronization; the pointer must never be touched off `queue`). By the
        // time `deinit` runs, refcount is 0 → no read/seek closure (all `[weak
        // self]`) can still reference `ptr`, and any in-flight one already ran to
        // completion before the last reference dropped. Capturing `ptr` by value
        // into the free closure races nothing.
        let p = ptr
        guard p != 0 else { return }
        let q = queue
        q.async { session_audio_free(p) }
    }
}

/// The **session-video audio sink** (task #84 plan §7): plays the audio track of a
/// session-backed (FFmpeg) video — the containers `AVPlayer` can't open, so there
/// is no system player to lean on. The Rust side owns demux/decode behind an
/// [`OwnedAudioDecoder`] driven on its own feeder queue (plan §7/1E — no longer on
/// `@MainActor`); this class owns the `AVAudioEngine` + `AVAudioPlayerNode` output
/// and the **played-position clock**, on the main actor.
///
/// Streaming, constant-memory: ~250 ms `AVAudioPCMBuffer`s are decoded off-main and
/// scheduled on the player node, topped up from each buffer's completion callback
/// (plus a pump-driven belt-and-suspenders top-up). The engine converts the
/// source's native rate to the device; the Rust decoder caps channels at stereo
/// (5.1/7.1 folds down there), because the engine graph only reliably takes
/// mono/stereo **standard** (deinterleaved) formats — an interleaved or 6-channel
/// connect throws an NSException (the owner's MKV crash, 2026-07-12).
///
/// Open is now **asynchronous** (off the main actor): until it completes the clock
/// reports `Opening`; an open/graph failure reports `Failed` and playback degrades
/// to silent. Every AVAudioEngine call that can throw an *Objective-C* exception
/// (connect, start, play, scheduleBuffer — Swift `catch` can't see those) runs
/// inside the `PBCatchException` shim.
///
/// The clock: `playerNode.playerTime(forNodeTime:)` gives the sample time actually
/// **rendered** (it freezes on pause and resets on `stop()`, i.e. on seek), minus
/// the output's `presentationLatency` — the honest "what's coming out of the
/// speaker" position, anchored at the media position of the first buffer scheduled
/// since the last (re)anchor. Samples flow to the core ~4×/s from `CoreModel.pump()`.
@MainActor
final class SessionAudioPlayer {
    /// Identity from the core; a command/sample for a different session is ignored
    /// core-side, so this only needs to ride along.
    let sessionId: UInt64

    private let engine = AVAudioEngine()
    private let node = AVAudioPlayerNode()
    private let decoder = OwnedAudioDecoder()

    /// Built once open lands (the source rate/channels aren't known until then).
    private var format: AVAudioFormat?
    private var rate: Double = 0
    private var channels: UInt32 = 0

    /// Media position (seconds) of the first sample scheduled since the last
    /// (re)anchor — `playerTime.sampleTime` counts from there.
    private var epochSecs: Double = 0
    /// Buffers scheduled and not yet completed.
    private var inFlight = 0
    /// Read requests dispatched to the feeder queue and not yet returned — counted
    /// alongside `inFlight` so the lookahead target is respected across the async gap.
    private var reading = 0
    /// The decoder is drained; when the last buffer completes the clock reports Ended.
    private var sourceDrained = false
    private var paused = true
    private var failed = false
    /// The decoder opened and the engine is running — until then the clock is Opening.
    private var opened = false
    private var muted: Bool
    /// Bumped on every seek; a read/seek completion from an older generation is
    /// dropped (post-seek staleness — never schedule superseded audio).
    private var seekGen: UInt64 = 0
    /// A seek requested before open completed — applied once the decoder is up.
    private var pendingSeek: Double?

    /// ~250 ms per buffer, 3 in flight → ~750 ms of scheduled lookahead. (Tunable
    /// against corpus/seek data per the plan; not hard-locked here.)
    private var chunkFrames: UInt32 { rate > 0 ? UInt32(rate / 4) : 4800 }
    private static let targetInFlight = 3

    /// Kicks off the decoder open **off the main actor** and returns immediately.
    /// The clock reports `Opening` until `finishOpen` lands; a no-audio / open /
    /// graph failure reports `Failed` and the session plays silently on its
    /// monotonic clock. (No longer a failable init — failure is async.)
    init(sessionId: UInt64, muted: Bool) {
        self.sessionId = sessionId
        self.muted = muted
        decoder.open(sessionId: sessionId) { [weak self] ok, rate, channels in
            // Delivered on the main thread by OwnedAudioDecoder — assert isolation.
            MainActor.assumeIsolated {
                self?.finishOpen(ok: ok, rate: rate, channels: channels)
            }
        }
    }

    /// Stand the engine up once the decoder has opened (main actor). Builds the
    /// standard stereo/mono format from the source rate, assembles + starts the
    /// graph (shielded), then applies any deferred seek / play state.
    private func finishOpen(ok: Bool, rate: UInt32, channels: UInt32) {
        guard !failed else { return }  // stopped during open
        // Standard = deinterleaved Float32 — the ONE format family the engine graph
        // accepts everywhere. The Rust cap guarantees channels ≤ 2.
        guard ok, rate > 0, channels > 0, channels <= 2,
            let format = AVAudioFormat(
                standardFormatWithSampleRate: Double(rate),
                channels: AVAudioChannelCount(channels))
        else {
            failed = true
            return
        }
        self.format = format
        self.rate = Double(rate)
        self.channels = channels
        node.volume = muted ? 0 : 1
        // Graph assembly + start, shielded: AVFAudio reports failure by NSException
        // (device in flux, aggregate-device weirdness, format refusal).
        let graphError = PBCatchException { [engine, node, format] in
            engine.attach(node)
            engine.connect(node, to: engine.mainMixerNode, format: format)
            try? engine.start()
        }
        if graphError != nil || !engine.isRunning {
            pbTrace("session audio \(sessionId): graph failed")
            failed = true
            return
        }
        opened = true
        // A seek issued during the open gap wins over a plain fill.
        if let target = pendingSeek {
            pendingSeek = nil
            applySeek(target)
            return
        }
        topUp()
        if !paused { startNode() }
    }

    /// Pull-and-schedule until the lookahead target is met. Main-actor; issues
    /// reads to the feeder queue (`reading` tracks the in-flight ones so we don't
    /// over-issue across the async gap). Called from finishOpen, buffer
    /// completions, seeks, and the pump.
    func topUp() {
        guard opened, !failed, !sourceDrained else { return }
        while inFlight + reading < Self.targetInFlight {
            reading += 1
            let gen = seekGen
            decoder.read(chunkFrames) { [weak self] samples, state in
                MainActor.assumeIsolated {
                    self?.onRead(samples: samples, state: state, gen: gen)
                }
            }
        }
    }

    /// A feeder-queue read returned (main actor). Drop it if a seek superseded it,
    /// map its state (R12: Failed ≠ Eof), else deinterleave + schedule it.
    private func onRead(samples: [Float], state: UInt8, gen: UInt64) {
        reading -= 1
        guard gen == seekGen, !failed else { return }  // superseded / stopped
        if state == 2 {  // Failed — a decode error, distinct from a clean end (R12)
            failed = true
            return
        }
        if samples.isEmpty {  // Eof
            sourceDrained = true
            return
        }
        let ch = Int(channels)
        let frames = samples.count / max(ch, 1)
        guard ch > 0, frames > 0, let format,
            let buf = AVAudioPCMBuffer(
                pcmFormat: format, frameCapacity: AVAudioFrameCount(frames)),
            let dst = buf.floatChannelData
        else {
            failed = true
            return
        }
        // Deinterleave the decoded chunk into the standard format's planes.
        samples.withUnsafeBufferPointer { src in
            for c in 0..<ch {
                let plane = dst[c]
                for i in 0..<frames { plane[i] = src[i * ch + c] }
            }
        }
        buf.frameLength = AVAudioFrameCount(frames)
        let scheduleError = PBCatchException { [node] in
            node.scheduleBuffer(buf) { [weak self] in
                // The node's completion fires on an AVAudioEngine thread, NOT main.
                Task { @MainActor [weak self] in
                    guard let self else { return }
                    self.inFlight -= 1
                    self.topUp()
                }
            }
        }
        if scheduleError != nil {
            failed = true
            return
        }
        inFlight += 1
        topUp()
    }

    /// Session entered `Playing` (or replay after a seek) — start/resume rendering.
    /// Before open completes this just records intent; `finishOpen` starts the node.
    func resume() {
        paused = false
        guard opened, !failed else { return }
        startNode()
    }

    private func startNode() {
        if PBCatchException({ [node] in node.play() }) != nil { failed = true }
    }

    /// Session paused / rebuffering — freeze (keeps position; the clock freezes
    /// with `playerTime`).
    func pause() {
        paused = true
        guard opened else { return }
        node.pause()
    }

    /// Mute in place: volume only, so rendering (and the clock) keeps running and
    /// A/V sync is mute-independent — the same posture as the Windows player.
    func setMuted(_ muted: Bool) {
        self.muted = muted
        node.volume = muted ? 0 : 1
    }

    /// Seek: stop the node (flushes scheduled buffers AND resets its sample time to
    /// zero), bump the generation so in-flight reads are dropped, reposition the
    /// Rust decoder off-main, re-anchor the epoch at the landing, and refill. The
    /// core treats the next near-target clock sample as the ack. A seek before open
    /// is deferred to `finishOpen`.
    func seek(toSeconds target: Double) {
        guard !failed else { return }
        guard opened else {
            pendingSeek = target
            return
        }
        applySeek(target)
    }

    private func applySeek(_ target: Double) {
        node.stop()
        inFlight = 0
        reading = 0
        sourceDrained = false
        seekGen &+= 1  // invalidate in-flight reads and any older seek
        let gen = seekGen
        decoder.seek(target) { [weak self] anchor in
            MainActor.assumeIsolated {
                guard let self, gen == self.seekGen, !self.failed else { return }
                self.epochSecs = anchor
                self.topUp()
                if !self.paused { self.startNode() }
            }
        }
    }

    /// One clock sample: (state, played position in seconds). The position is the
    /// rendered sample time since the last anchor minus the output's presentation
    /// latency — the plan's "position actually played", not PCM bytes written.
    /// States: 0 Opening, 1 Playing, 2 Paused, 4 Ended, 5 Failed.
    func sample() -> (state: UInt8, positionSecs: Double) {
        if failed { return (5, epochSecs) }  // Failed
        if !opened { return (0, 0) }  // Opening — the async open is still in flight
        var played = 0.0
        if let nodeTime = node.lastRenderTime,
            let playerTime = node.playerTime(forNodeTime: nodeTime),
            playerTime.isSampleTimeValid
        {
            let latency = engine.outputNode.presentationLatency
            played = max(0, Double(playerTime.sampleTime) / rate - latency)
        }
        let position = epochSecs + played
        // Ended only on a CLEAN drain (Eof): a Failed read latched `failed` above,
        // so a corrupt tail reports Failed, never a silent Ended (R12).
        if sourceDrained && inFlight == 0 { return (4, position) }  // Ended
        return (paused ? 2 : 1, position)  // Paused : Playing
    }

    /// Full teardown: gate stragglers, stop rendering, drop the engine. The Rust
    /// decoder is freed by `OwnedAudioDecoder.deinit` when this player (its sole
    /// owner) deallocates — exactly once, on the feeder queue, after any in-flight
    /// read finishes. Never blocks the main actor.
    func stop() {
        failed = true  // gates any straggler completion / decoder callback
        node.stop()
        engine.stop()
    }
}
