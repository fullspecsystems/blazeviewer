import AVFoundation
import PbMacFfi

/// The **session-video audio sink** (task #84 plan §7): plays the audio track of a
/// session-backed (FFmpeg) video — the containers `AVPlayer` can't open, so there is
/// no system player to lean on. The Rust side owns demux/decode
/// (`video_audio_open`/`read`/`seek` on the core handle); this class owns the
/// `AVAudioEngine` + `AVAudioPlayerNode` output and the **played-position clock**.
///
/// Streaming, constant-memory: ~250 ms `AVAudioPCMBuffer`s are pulled on the main
/// actor (decoding a chunk is ~1 ms) and scheduled on the player node, topped up
/// from each buffer's completion callback (plus a pump-driven belt-and-suspenders
/// top-up). The engine converts the source's native rate/channels to the device.
///
/// The clock: `playerNode.playerTime(forNodeTime:)` gives the sample time actually
/// **rendered** (it freezes on pause and resets on `stop()`, i.e. on seek), minus
/// the output's `presentationLatency` — the honest "what's coming out of the
/// speaker" position the plan requires, anchored at the media position of the first
/// buffer scheduled since the last (re)anchor. Samples flow to the core ~4×/s from
/// `CoreModel.pump()`; the session uses them as the master clock while playing.
@MainActor
final class SessionAudioPlayer {
    /// Identity from the core; a command/sample for a different session is ignored
    /// core-side, so this only needs to ride along.
    let sessionId: UInt64

    private let engine = AVAudioEngine()
    private let node = AVAudioPlayerNode()
    private let format: AVAudioFormat
    private let rate: Double
    private let channels: UInt32
    private unowned let core: AppCoreHandle

    /// Media position (seconds) of the first sample scheduled since the last
    /// (re)anchor — `playerTime.sampleTime` counts from there.
    private var epochSecs: Double
    /// Buffers currently scheduled and not yet completed.
    private var inFlight = 0
    /// The decoder is drained; when the last buffer completes the clock reports Ended.
    private var sourceDrained = false
    private var paused = true
    private var failed = false

    /// ~250 ms per buffer, 3 in flight → ~750 ms of scheduled lookahead.
    private var chunkFrames: UInt32 { UInt32(rate / 4) }
    private static let targetInFlight = 3

    /// Opens the Rust decoder over the stashed container and stands the engine up
    /// **paused** (the core resumes audio with the video preroll). `nil` = no audio
    /// track / open failure — the caller reports a `Failed` clock sample and the
    /// session degrades to silent playback immediately.
    init?(core: AppCoreHandle, sessionId: UInt64, muted: Bool) {
        guard core.video_audio_open() else { return nil }
        let rate = core.video_audio_rate()
        let channels = core.video_audio_channels()
        guard rate > 0, channels > 0,
            let format = AVAudioFormat(
                commonFormat: .pcmFormatFloat32, sampleRate: Double(rate),
                channels: AVAudioChannelCount(channels), interleaved: true)
        else {
            core.video_audio_close()
            return nil
        }
        self.core = core
        self.sessionId = sessionId
        self.format = format
        self.rate = Double(rate)
        self.channels = channels
        self.epochSecs = 0
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: format)
        node.volume = muted ? 0 : 1
        do {
            try engine.start()
        } catch {
            pbTrace("session audio \(sessionId): engine start failed: \(error)")
            core.video_audio_close()
            return nil
        }
        topUp()
    }

    /// Pull-and-schedule until the lookahead target is met. Main-actor; called
    /// from init, buffer completions, seeks, and the pump.
    func topUp() {
        guard !failed, !sourceDrained else { return }
        while inFlight < Self.targetInFlight {
            let chunk = core.video_audio_read(chunkFrames)
            let sampleCount = chunk.len()
            if sampleCount == 0 {
                sourceDrained = true
                return
            }
            let frames = AVAudioFrameCount(sampleCount / Int(channels))
            guard let buf = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frames),
                let dst = buf.floatChannelData?[0]
            else {
                failed = true
                return
            }
            // Interleaved float32: channel 0's pointer is the packed buffer.
            dst.update(from: chunk.as_ptr(), count: sampleCount)
            buf.frameLength = frames
            inFlight += 1
            node.scheduleBuffer(buf) { [weak self] in
                Task { @MainActor [weak self] in
                    guard let self else { return }
                    self.inFlight -= 1
                    self.topUp()
                }
            }
        }
    }

    /// Session entered `Playing` (or replay after a seek) — start/resume rendering.
    func resume() {
        paused = false
        node.play()
    }

    /// Session paused / rebuffering — freeze (keeps position; the clock freezes
    /// with `playerTime`).
    func pause() {
        paused = true
        node.pause()
    }

    /// Mute in place: volume only, so rendering (and the clock) keeps running and
    /// A/V sync is mute-independent — the same posture as the Windows player.
    func setMuted(_ muted: Bool) {
        node.volume = muted ? 0 : 1
    }

    /// Seek: stop the node (flushes scheduled buffers AND resets its sample time
    /// to zero), reposition the Rust decoder, re-anchor the epoch at the landing,
    /// and refill. The core treats the next near-target clock sample as the ack.
    func seek(toSeconds target: Double) {
        node.stop()
        inFlight = 0
        sourceDrained = false
        epochSecs = core.video_audio_seek(target)
        topUp()
        if !paused { node.play() }
    }

    /// One clock sample: (state, played position in seconds). The position is the
    /// rendered sample time since the last anchor minus the output's presentation
    /// latency — the plan's "position actually played", not PCM bytes written.
    func sample() -> (state: UInt8, positionSecs: Double) {
        if failed { return (5, epochSecs) } // Failed
        var played = 0.0
        if let nodeTime = node.lastRenderTime,
            let playerTime = node.playerTime(forNodeTime: nodeTime),
            playerTime.isSampleTimeValid
        {
            let latency = engine.outputNode.presentationLatency
            played = max(0, Double(playerTime.sampleTime) / rate - latency)
        }
        let position = epochSecs + played
        if sourceDrained && inFlight == 0 && core.video_audio_at_eof() {
            return (4, position) // Ended — the tail actually rendered
        }
        return (paused ? 2 : 1, position) // Paused : Playing
    }

    /// Full teardown: stop rendering, drop the engine taps, close the Rust decoder.
    func stop() {
        failed = true // gates any straggler completion-driven topUp
        node.stop()
        engine.stop()
        core.video_audio_close()
    }
}
