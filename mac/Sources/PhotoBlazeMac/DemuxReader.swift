import AVFoundation
import CoreMedia
import Foundation
import PbMacFfi
import VideoToolbox

/// Facts read once when the demuxer opens, handed back to the main actor.
struct DemuxOpen: Sendable {
    var ok: Bool
    var width: UInt32
    var height: UInt32
    var durationSecs: Double
    var fps: Double
    var hasAudio: Bool
    var doviProfile: UInt8
}

/// Exclusive owner of the Rust demux-only packet-source pointer (video-overhaul
/// Phase 3), the twin of [`OwnedAudioDecoder`]. Every demux operation — open, read,
/// seek, free — runs on `queue`, a dedicated serial reader queue, so the pointer
/// never touches the main actor and there is never a concurrent read and seek.
/// swift-bridge hands out a raw `usize`; this class is its sole owner and frees it
/// **exactly once** in `deinit`.
///
/// It also owns the compressed-packet → `CMSampleBuffer` feed: the display layer's
/// `requestMediaDataWhenReady(on:)` runs its block on `queue`, so pulling a packet
/// (a pointer call) and enqueuing the sample buffer both happen on the reader
/// thread — no cross-thread packet copy, and the compressed queue stays bounded by
/// the renderer's `isReadyForMoreMediaData` appetite (plan §3A backpressure).
final class DemuxReader: @unchecked Sendable {
    private let queue = DispatchQueue(
        label: "ca.fullspec.photoblaze.sample-buffer-demux", qos: .userInitiated)
    /// Nonzero once opened; `0` = unopened / open failed. Touched only on `queue`
    /// (and in `deinit`, when no other reference remains).
    private var ptr: UInt = 0

    private var formatDesc: CMVideoFormatDescription?
    private var tbNum: Int32 = 0
    private var tbDen: Int32 = 0
    /// One frame's duration in stream units, for synthesizing DTS on packets the
    /// container leaves without one (common on B-frame H.264 in MKV: the leading
    /// reorder-priming packets carry no DTS). `0` when fps is unknown.
    private var frameDurUnits: Int64 = 0
    /// Last DTS emitted (stream units), so a synthesized DTS stays monotonic across
    /// the mix of missing and present decode timestamps. `Int64.min` = none yet.
    private var lastDtsUnits: Int64 = Int64.min
    /// The first packet's PTS (stream units), subtracted from every timestamp so the
    /// presentation timeline is 0-based — matching the audio feeder's 0-based PCM
    /// clock, so both share one synchronizer origin. Set once, constant for the
    /// session (a seek re-anchors the synchronizer, not this origin). `Int64.min`
    /// until the first packet.
    private var origin: Int64 = Int64.min

    // Feed context (set on `queue` in startFeeding; read on `queue` in the block).
    private var feedLayer: AVSampleBufferDisplayLayer?
    private var onFirstFrame: (@Sendable (CMTime) -> Void)?
    private var onEnd: (@Sendable () -> Void)?
    private var onError: (@Sendable () -> Void)?
    private var firstFrameSent = false
    private var requesting = false
    private var done = false

    /// Open the demuxer for `sessionId` over the container `PlaySampleBuffer`
    /// stashed Rust-side, off the main actor. Builds the `CMVideoFormatDescription`
    /// from the hvcC/avcC atom (+ the DoVi `dvvC`/`dvcC` box when present). `then`
    /// runs back on the main actor with the opened facts (`ok == false` on any
    /// failure → the presenter falls back to the Session route).
    func open(sessionId: UInt64, then: @escaping @Sendable (DemuxOpen?) -> Void) {
        queue.async { [weak self] in
            guard let self else {
                DispatchQueue.main.async { then(nil) }
                return
            }
            let p = open_stashed_demux(sessionId)
            self.ptr = p
            guard p != 0, self.buildFormatDescription(p) else {
                DispatchQueue.main.async {
                    then(DemuxOpen(ok: false, width: 0, height: 0, durationSecs: 0,
                                   fps: 0, hasAudio: false, doviProfile: 0))
                }
                return
            }
            // One frame's stream-unit duration (tbNum/tbDen set by buildFormatDescription),
            // for DTS synthesis on leading packets that lack a decode timestamp.
            let f = demux_fps(p)
            self.frameDurUnits =
                (f > 0 && self.tbDen > 0 && self.tbNum > 0)
                ? Int64((1.0 / f) * Double(self.tbDen) / Double(self.tbNum) + 0.5)
                : 0
            let out = DemuxOpen(
                ok: true,
                width: demux_width(p),
                height: demux_height(p),
                durationSecs: demux_duration_secs(p),
                fps: demux_fps(p),
                hasAudio: demux_has_audio(p),
                doviProfile: demux_dovi_profile(p))
            DispatchQueue.main.async { then(out) }
        }
    }

    /// Build the format description on `queue`. Returns false for an unsupported
    /// codec / Annex-B (non-length-prefixed) stream / missing extradata — the
    /// clip then routes to the Session fallback.
    private func buildFormatDescription(_ p: UInt) -> Bool {
        let codec = demux_codec(p)
        guard demux_length_prefixed(p) else { return false }
        let extradata = Self.data(demux_extradata(p))
        guard !extradata.isEmpty else { return false }
        let codecType: CMVideoCodecType
        let atomKey: String
        switch codec {
        case 0:
            codecType = kCMVideoCodecType_H264
            atomKey = "avcC"
        case 1:
            codecType = kCMVideoCodecType_HEVC
            atomKey = "hvcC"
        default:
            return false // VideoToolbox can't sample-decode this codec
        }
        var atoms: [String: Data] = [atomKey: extradata]
        // Attach the Dolby Vision configuration box (dvvC/dvcC) so VideoToolbox
        // engages the DoVi path instead of decoding the plain HDR10 base layer.
        let doviAtom = demux_dovi_atom(p)
        if doviAtom.len() == 4 {
            let name = String(bytes: Self.data(doviAtom), encoding: .ascii) ?? ""
            let box = Self.data(demux_dovi_box(p))
            if !name.isEmpty, !box.isEmpty {
                atoms[name] = box
            }
        }
        let ext: [CFString: Any] = [
            kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms: atoms
        ]
        var fmt: CMVideoFormatDescription?
        let status = CMVideoFormatDescriptionCreate(
            allocator: kCFAllocatorDefault,
            codecType: codecType,
            width: Int32(bitPattern: demux_width(p)),
            height: Int32(bitPattern: demux_height(p)),
            extensions: ext as CFDictionary,
            formatDescriptionOut: &fmt)
        guard status == noErr, let fmt else { return false }
        formatDesc = fmt
        tbNum = demux_time_base_num(p)
        tbDen = demux_time_base_den(p)
        return true
    }

    /// Start (or restart) feeding `layer` from renderer readiness. The block runs on
    /// `queue`; the callbacks fire the presenter's reveal / EOS / error, on the main
    /// actor. Idempotent — a second call while already requesting is a no-op.
    func startFeeding(
        into layer: AVSampleBufferDisplayLayer,
        onFirstFrame: @escaping @Sendable (CMTime) -> Void,
        onEnd: @escaping @Sendable () -> Void,
        onError: @escaping @Sendable () -> Void
    ) {
        queue.async { [weak self] in
            guard let self else { return }
            self.feedLayer = layer
            self.onFirstFrame = onFirstFrame
            self.onEnd = onEnd
            self.onError = onError
            self.armFeeding()
        }
    }

    /// (Re)arm `requestMediaDataWhenReady` on `queue`. Must be called on `queue`.
    private func armFeeding() {
        guard !requesting, let layer = feedLayer else { return }
        requesting = true
        done = false
        layer.requestMediaDataWhenReady(on: queue) { [weak self] in
            self?.provide(into: layer)
        }
    }

    /// The feed loop — runs on `queue`. Pull compressed packets and enqueue sample
    /// buffers while the renderer wants data; stop at EOF/error (parking the last
    /// frame) or when torn down.
    private func provide(into layer: AVSampleBufferDisplayLayer) {
        while layer.isReadyForMoreMediaData {
            guard ptr != 0, !done, let fmt = formatDesc else {
                layer.stopRequestingMediaData()
                requesting = false
                return
            }
            let rv = demux_read_packet(ptr)
            let n = Int(rv.len())
            if n == 0 {
                let state = demux_state(ptr)
                done = true
                layer.stopRequestingMediaData()
                requesting = false
                if state == 1 {
                    onEnd.map { cb in DispatchQueue.main.async { cb() } }
                } else if state == 2 {
                    onError.map { cb in DispatchQueue.main.async { cb() } }
                }
                return
            }
            let pts = demux_packet_pts(ptr)
            let dts = effectiveDts(pts: pts, rawDts: demux_packet_dts(ptr))
            let dur = demux_packet_duration(ptr)
            if origin == Int64.min {
                origin = pts != Int64.min ? pts : (dts != Int64.min ? dts : 0)
            }
            guard let sb = makeSampleBuffer(rv, count: n, fmt: fmt, pts: pts, dts: dts, dur: dur)
            else { continue }
            layer.enqueue(sb)
            if !firstFrameSent {
                firstFrameSent = true
                let first = cmTime(pts)
                onFirstFrame.map { cb in DispatchQueue.main.async { cb(first) } }
            }
        }
    }

    /// Wrap one compressed access unit in a `CMSampleBuffer` with the format
    /// description + PTS/DTS/duration timing (in the stream time base).
    private func makeSampleBuffer(
        _ rv: RustVec<UInt8>, count n: Int, fmt: CMVideoFormatDescription,
        pts: Int64, dts: Int64, dur: Int64
    ) -> CMSampleBuffer? {
        var bb: CMBlockBuffer?
        var status = CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault, memoryBlock: nil, blockLength: n,
            blockAllocator: kCFAllocatorDefault, customBlockSource: nil,
            offsetToData: 0, dataLength: n, flags: 0, blockBufferOut: &bb)
        guard status == kCMBlockBufferNoErr, let bb else { return nil }
        status = CMBlockBufferReplaceDataBytes(
            with: UnsafeRawPointer(rv.as_ptr()), blockBuffer: bb,
            offsetIntoDestination: 0, dataLength: n)
        guard status == kCMBlockBufferNoErr else { return nil }

        var timing = CMSampleTimingInfo(
            duration: dur > 0 ? cmDuration(dur) : .invalid,
            presentationTimeStamp: cmTime(pts),
            decodeTimeStamp: cmTime(dts))
        var size = n
        var sb: CMSampleBuffer?
        status = CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault, dataBuffer: bb, formatDescription: fmt,
            sampleCount: 1, sampleTimingEntryCount: 1, sampleTimingArray: &timing,
            sampleSizeEntryCount: 1, sampleSizeArray: &size, sampleBufferOut: &sb)
        guard status == noErr, let sb else { return nil }
        return sb
    }

    /// A stream-unit timestamp → `CMTime` in the 0-based (origin-subtracted)
    /// presentation timeline (or `.invalid` for the `i64::MIN` sentinel).
    private func cmTime(_ units: Int64) -> CMTime {
        guard units != Int64.min, tbDen > 0 else { return .invalid }
        let base = origin == Int64.min ? 0 : origin
        return CMTime(value: (units - base) * Int64(tbNum), timescale: tbDen)
    }

    private func cmDuration(_ units: Int64) -> CMTime {
        guard tbDen > 0 else { return .invalid }
        return CMTime(value: units * Int64(tbNum), timescale: tbDen)
    }

    /// The decode timestamp to feed the sample buffer. Passes a real DTS through;
    /// for a packet the container leaves without one (the leading reorder-priming
    /// packets of a B-frame stream), synthesizes a **monotonic** DTS so the display
    /// layer gets a consistent decode timeline — `AVSampleBufferDisplayLayer` fails
    /// to decode a B-frame stream when DTS is present on some samples and missing on
    /// others (the Grey's-Anatomy H.264 MKV: first two packets `dts=N/A`). Seeds a
    /// generously-negative ramp from the PTS so it stays below the first real DTS
    /// (over-negative is harmless — the layer decodes ahead of the presentation clock).
    private func effectiveDts(pts: Int64, rawDts: Int64) -> Int64 {
        let fd = frameDurUnits > 0 ? frameDurUnits : 1
        if rawDts != Int64.min {
            lastDtsUnits = rawDts
            return rawDts
        }
        let synth =
            lastDtsUnits == Int64.min
            ? (pts == Int64.min ? 0 : pts) - 60 * fd // seed well below any real DTS
            : lastDtsUnits + fd
        lastDtsUnits = synth
        return synth
    }

    /// Seek to `seconds` (keyframe at/before), flush the renderer, and re-feed from
    /// the new position. `then` gets the first landed PTS on the main actor (or `nil`
    /// on failure) so the presenter re-anchors the synchronizer.
    func seek(
        seconds: Double, layer: AVSampleBufferDisplayLayer,
        then: @escaping @Sendable (CMTime?) -> Void
    ) {
        queue.async { [weak self] in
            guard let self, self.ptr != 0 else {
                DispatchQueue.main.async { then(nil) }
                return
            }
            layer.flush()
            demux_seek(self.ptr, seconds)
            self.lastDtsUnits = Int64.min // re-seed DTS synth for the post-seek keyframe run
            if demux_state(self.ptr) == 2 {
                DispatchQueue.main.async { then(nil) }
                return
            }
            // Re-feed from the seek target; the next enqueued frame is the anchor.
            self.firstFrameSent = false
            self.done = false
            self.armFeeding()
            // The landed PTS is the requested target (the renderer starts at the
            // next keyframe ≤ it; close enough for the synchronizer anchor).
            let landed = CMTime(seconds: max(0, seconds), preferredTimescale: 600)
            DispatchQueue.main.async { then(landed) }
        }
    }

    /// Stop feeding and free the demuxer. Called from the presenter's `stop()`.
    func stop(layer: AVSampleBufferDisplayLayer) {
        queue.async { [weak self] in
            guard let self else { return }
            self.done = true
            self.requesting = false
            layer.stopRequestingMediaData()
            layer.flush()
            self.feedLayer = nil
            self.onFirstFrame = nil
            self.onEnd = nil
            self.onError = nil
        }
    }

    private static func data(_ rv: RustVec<UInt8>) -> Data {
        let n = Int(rv.len())
        guard n > 0 else { return Data() }
        return Data(bytes: UnsafeRawPointer(rv.as_ptr()), count: n)
    }

    deinit {
        let p = ptr
        guard p != 0 else { return }
        queue.async { demux_free(p) }
    }
}
