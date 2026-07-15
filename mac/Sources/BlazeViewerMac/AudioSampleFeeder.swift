import AVFoundation
import CoreMedia
import Foundation
import PbMacFfi

/// Feeds decoded audio into an `AVSampleBufferAudioRenderer` on the sample-buffer
/// presenter's synchronizer (video-overhaul Phase 3, §3C) — the audio twin of
/// [`DemuxReader`]. It owns the Rust FFmpeg audio-decoder pointer (the same
/// `session_audio_*` seam the Session route's `OwnedAudioDecoder` uses) on a
/// dedicated serial queue, so the pointer never touches the main actor and there
/// is never a concurrent read and seek.
///
/// The renderer's `requestMediaDataWhenReady(on:)` runs its block on that queue,
/// so pulling PCM (a pointer call) and enqueuing the sample buffer both happen on
/// the feeder thread. Because the audio renderer and the video display layer share
/// ONE `AVSampleBufferRenderSynchronizer`, A/V stay in sync off a single clock —
/// no separate audio-engine timeline (unlike the Session route).
///
/// PCM is FFmpeg-decoded interleaved float32, channel-capped at stereo by the Rust
/// decoder (5.1/7.1 folds down) — a documented downmix, honestly reported; TrueHD
/// object audio is not preserved. Timestamps are 0-based sample counts, matching
/// the video's re-based (origin-subtracted) timeline.
final class AudioSampleFeeder: @unchecked Sendable {
    private let queue = DispatchQueue(
        label: "ca.fullspec.blazeviewer.sample-buffer-audio", qos: .userInitiated)
    private var ptr: UInt = 0

    private var rate: Int32 = 0
    private var channels: Int = 0
    private var format: CMAudioFormatDescription?
    /// Running presentation frame count → the next buffer's PTS (in `rate` units).
    private var ptsFrames: Int64 = 0

    private var feedRenderer: AVSampleBufferAudioRenderer?
    private var requesting = false
    private var done = false
    /// A seek issued **before the decoder finished opening** — held here and applied at
    /// open, rather than silently dropped (plan §H2': the old `guard ptr != 0 else return`
    /// lost the seek and nothing retried it). The most recent one wins. `.1` is the epoch.
    private var pendingSeek: (seconds: Double, epoch: UInt64)?

    /// Frames pulled per decode call (a few tens of ms at 48 kHz).
    private static let framesPerPull: UInt32 = 4096

    /// Open the FFmpeg audio decoder for `sessionId` over the stashed container,
    /// off the main actor, and build the LPCM format description. `then(hasAudio)`
    /// runs back on the main actor — `false` when the clip has no (openable) audio,
    /// so the presenter simply plays silent.
    ///
    /// `startSecs > 0` resumes the audio at that position (task #94.2). This is the fix
    /// for the resume-audio bug (plan §H2', new today): `startSecs` seeked only the video
    /// reader, so a resumed MKV played its picture minutes in while the audio sat 0-based in
    /// the past and was discarded. Applying the resume **here at open** — the single moment
    /// the decoder pointer becomes valid — is the natural, race-free place, and it also
    /// applies any user seek that arrived while the decoder was still opening (`pendingSeek`).
    func open(sessionId: UInt64, startSecs: Double = 0, then: @escaping @Sendable (Bool) -> Void) {
        queue.async { [weak self] in
            guard let self else {
                DispatchQueue.main.async { then(false) }
                return
            }
            let p = open_stashed_session_audio(sessionId)
            self.ptr = p
            guard p != 0 else {
                DispatchQueue.main.async { then(false) }
                return
            }
            self.rate = Int32(bitPattern: session_audio_rate(p))
            self.channels = Int(session_audio_channels(p))
            guard self.rate > 0, self.channels > 0, self.buildFormat() else {
                DispatchQueue.main.async { then(false) }
                return
            }
            // Resume position, then any seek that landed before the decoder opened (a user
            // scrub during the open window) — the later one wins, so it goes last. No
            // renderer flush needed: feeding hasn't been armed yet (startFeeding is next).
            if startSecs > 0 {
                self.rebaseClock(to: startSecs)
            }
            if let pending = self.pendingSeek {
                self.pendingSeek = nil
                self.rebaseClock(to: pending.seconds)
            }
            DispatchQueue.main.async { then(true) }
        }
    }

    /// Position the decoder at `seconds` and re-base the PTS clock to the landed spot, so
    /// the next enqueued buffer's timestamp matches the video timeline. Must run on `queue`
    /// with `ptr != 0`. Does **not** flush a renderer or arm feeding — the caller owns that
    /// (open hasn't armed yet; `seek` flushes because it may be mid-playback).
    private func rebaseClock(to seconds: Double) {
        let landed = session_audio_seek(self.ptr, seconds)
        self.ptsFrames = Int64((landed.isFinite ? landed : seconds) * Double(self.rate))
    }

    private func buildFormat() -> Bool {
        var asbd = AudioStreamBasicDescription(
            mSampleRate: Float64(rate),
            mFormatID: kAudioFormatLinearPCM,
            // Interleaved (no NonInterleaved flag) packed float32.
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: UInt32(4 * channels),
            mFramesPerPacket: 1,
            mBytesPerFrame: UInt32(4 * channels),
            mChannelsPerFrame: UInt32(channels),
            mBitsPerChannel: 32,
            mReserved: 0)
        var fmt: CMAudioFormatDescription?
        let status = CMAudioFormatDescriptionCreate(
            allocator: kCFAllocatorDefault, asbd: &asbd,
            layoutSize: 0, layout: nil, magicCookieSize: 0, magicCookie: nil,
            extensions: nil, formatDescriptionOut: &fmt)
        guard status == noErr, let fmt else { return false }
        format = fmt
        return true
    }

    /// Start (or restart) feeding `renderer` from renderer readiness. Idempotent.
    func startFeeding(into renderer: AVSampleBufferAudioRenderer) {
        queue.async { [weak self] in
            guard let self else { return }
            self.feedRenderer = renderer
            self.armFeeding()
        }
    }

    private func armFeeding() {
        guard !requesting, let renderer = feedRenderer, ptr != 0, format != nil else { return }
        requesting = true
        done = false
        renderer.requestMediaDataWhenReady(on: queue) { [weak self] in
            self?.provide(into: renderer)
        }
    }

    private func provide(into renderer: AVSampleBufferAudioRenderer) {
        while renderer.isReadyForMoreMediaData {
            guard ptr != 0, !done, let fmt = format else {
                renderer.stopRequestingMediaData()
                requesting = false
                return
            }
            let rv = session_audio_read(ptr, Self.framesPerPull)
            let n = Int(rv.len())
            if n == 0 {
                // Empty: EOF, a transient stall, or failure — the state getter
                // disambiguates. EOF/failure stops the feed (video keeps its clock);
                // a transient stall (state 3 Buffering) just yields this tick.
                let state = session_audio_state(ptr)
                if state == 1 || state == 2 {
                    done = true
                    renderer.stopRequestingMediaData()
                    requesting = false
                }
                return
            }
            if let sb = makeSampleBuffer(rv, count: n, fmt: fmt) {
                renderer.enqueue(sb)
            }
        }
    }

    private func makeSampleBuffer(
        _ rv: RustVec<Float>, count n: Int, fmt: CMAudioFormatDescription
    ) -> CMSampleBuffer? {
        let frames = n / max(1, channels)
        guard frames > 0 else { return nil }
        let byteCount = n * 4
        var bb: CMBlockBuffer?
        var status = CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault, memoryBlock: nil, blockLength: byteCount,
            blockAllocator: kCFAllocatorDefault, customBlockSource: nil,
            offsetToData: 0, dataLength: byteCount, flags: 0, blockBufferOut: &bb)
        guard status == kCMBlockBufferNoErr, let bb else { return nil }
        status = CMBlockBufferReplaceDataBytes(
            with: UnsafeRawPointer(rv.as_ptr()), blockBuffer: bb,
            offsetIntoDestination: 0, dataLength: byteCount)
        guard status == kCMBlockBufferNoErr else { return nil }

        var timing = CMSampleTimingInfo(
            duration: CMTime(value: 1, timescale: rate),
            presentationTimeStamp: CMTime(value: ptsFrames, timescale: rate),
            decodeTimeStamp: .invalid)
        var size = 4 * channels // bytes per frame; one size entry covers all frames
        var sb: CMSampleBuffer?
        status = CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault, dataBuffer: bb, formatDescription: fmt,
            sampleCount: frames, sampleTimingEntryCount: 1, sampleTimingArray: &timing,
            sampleSizeEntryCount: 1, sampleSizeArray: &size, sampleBufferOut: &sb)
        guard status == noErr, let sb else { return nil }
        ptsFrames += Int64(frames)
        return sb
    }

    /// Switch to audio stream `track`, resuming at `seconds` (task #99).
    ///
    /// The decoder re-opens **in place** (the Rust pointer stays valid), then this does the
    /// same dance as `seek` — flush, re-base the PTS clock, re-arm — plus the one step a
    /// seek never needs: **rebuild the format**. Rate and channel count differ between
    /// tracks (a 5.1 commentary beside a stereo main is the ordinary case), and feeding
    /// buffers described by the *old* format would mis-stride every frame: garbled audio, no
    /// error anywhere. That is the real hazard in this function, not the re-open.
    ///
    /// Cheap by design on this route: audio is **not** the master clock — the audio renderer
    /// and the video layer share one `AVSampleBufferRenderSynchronizer` — so the picture
    /// never stops and there is no clock to re-prime.
    ///
    /// `then(ok)` runs on the main actor. `ok == false` means the switch was refused and the
    /// **previous track is still playing**: the caller must not report a switch that did not
    /// happen (task #99's confirmed-switch rule).
    func switchTrack(
        _ track: Int, at seconds: Double, renderer: AVSampleBufferAudioRenderer,
        then: @escaping @Sendable (Bool) -> Void
    ) {
        queue.async { [weak self] in
            guard let self, self.ptr != 0 else {
                DispatchQueue.main.async { then(false) }
                return
            }
            guard session_audio_set_track(self.ptr, UInt(track)) else {
                DispatchQueue.main.async { then(false) } // old decoder still feeding
                return
            }
            renderer.flush()
            // The new track's own format — never the old one's.
            self.rate = Int32(bitPattern: session_audio_rate(self.ptr))
            self.channels = Int(session_audio_channels(self.ptr))
            guard self.rate > 0, self.channels > 0, self.buildFormat() else {
                DispatchQueue.main.async { then(false) }
                return
            }
            // Resume where the picture is, not at zero.
            let landed = session_audio_seek(self.ptr, seconds)
            self.ptsFrames = Int64((landed.isFinite ? landed : seconds) * Double(self.rate))
            self.done = false
            self.armFeeding()
            DispatchQueue.main.async { then(true) }
        }
    }

    /// Which audio stream is playing — the container's real index, or `nil` before the
    /// decoder opens. The picker's tick reads this, so it reports what you are *hearing*
    /// rather than what was requested.
    func currentTrack(then: @escaping @Sendable (Int?) -> Void) {
        queue.async { [weak self] in
            guard let self, self.ptr != 0 else {
                DispatchQueue.main.async { then(nil) }
                return
            }
            let i = session_audio_stream_index(self.ptr)
            DispatchQueue.main.async { then(i < 0 ? nil : Int(i)) }
        }
    }

    /// Seek the audio decoder to `seconds` under generation `epoch`, flush the renderer, and
    /// re-anchor the PTS clock so audio lines up with the video after a seek (§3D). `then(ok)`
    /// runs on the main actor: `true` = applied now; `false` = **held** because the decoder
    /// isn't open yet (applied at open) — so the presenter can wait on real A/V readiness
    /// instead of the seek being silently lost (plan §H2').
    func seek(
        seconds: Double, epoch: UInt64, renderer: AVSampleBufferAudioRenderer,
        then: @escaping @Sendable (Bool) -> Void
    ) {
        queue.async { [weak self] in
            guard let self else {
                DispatchQueue.main.async { then(false) }
                return
            }
            guard self.ptr != 0 else {
                // Not open yet — hold it (most recent wins) and apply at open, rather than
                // dropping the seek. This is the same machinery the resume-at-open uses.
                self.pendingSeek = (seconds, epoch)
                DispatchQueue.main.async { then(false) }
                return
            }
            renderer.flush()
            self.rebaseClock(to: seconds)
            self.done = false
            self.armFeeding()
            DispatchQueue.main.async { then(true) }
        }
    }

    func stop(renderer: AVSampleBufferAudioRenderer) {
        queue.async { [weak self] in
            guard let self else { return }
            self.done = true
            self.requesting = false
            renderer.stopRequestingMediaData()
            renderer.flush()
            self.feedRenderer = nil
        }
    }

    deinit {
        let p = ptr
        guard p != 0 else { return }
        queue.async { session_audio_free(p) }
    }
}
