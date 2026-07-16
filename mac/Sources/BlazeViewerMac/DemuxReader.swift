import AVFoundation
import CoreMedia
import Foundation
import PbMacFfi
import PbSeek
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
        label: "ca.fullspec.blazeviewer.sample-buffer-demux", qos: .userInitiated)
    /// Nonzero once opened; `0` = unopened / open failed. Touched only on `queue`
    /// (and in `deinit`, when no other reference remains).
    private var ptr: UInt = 0

    private var formatDesc: CMVideoFormatDescription?
    private var tbNum: Int32 = 0
    private var tbDen: Int32 = 0
    /// The first packet's PTS (stream units), subtracted from every timestamp so the
    /// presentation timeline is 0-based — matching the audio feeder's 0-based PCM
    /// clock, so both share one synchronizer origin. Set once, constant for the
    /// session (a seek re-anchors the synchronizer, not this origin). `Int64.min`
    /// until the first packet.
    private var origin: Int64 = Int64.min

    /// The pure seek-frame classification (plan §T1). Every "is this pre-roll? does this
    /// frame anchor, at what time?" decision goes through this, so it is unit-tested in
    /// `PbSeek` rather than buried in the feed loop untestable, which is how `97f80bb4`
    /// shipped a backwards forward-seek.
    private let seekPolicy = SeekFramePolicy()

    // Feed context (set on `queue` in startFeeding; read on `queue` in the block).
    private var feedLayer: AVSampleBufferDisplayLayer?
    /// Fires on the first *displayable* frame with the epoch of the feed that produced it,
    /// so the presenter can reject a stale anchor from a superseded seek before it moves
    /// the clock (plan §H1c). `0` = the initial feed.
    private var onFirstFrame: (@Sendable (CMTime, UInt64) -> Void)?
    private var onEnd: (@Sendable () -> Void)?
    private var onError: (@Sendable () -> Void)?
    /// The epoch of the feed currently in flight (`0` = initial). A seek stamps its epoch
    /// here and `provide` fires `onFirstFrame` with it (plan §H1c).
    private var feedEpoch: UInt64 = 0
    private var firstFrameSent = false
    /// While a seek's **pre-roll** is being fed: the requested target, in the same 0-based
    /// presentation timeline `cmTime` produces. `nil` = not seeking (initial playback).
    ///
    /// ⚠ This is what makes a forward seek work at all. `demux_seek` lands on the keyframe
    /// **at or before** the target — so a 2 s hop inside a 5 s GOP lands on the keyframe you
    /// are already in, and anchoring the clock there sends the film *backwards* to 5/10/15 s
    /// while looking like a glitch that "does nothing" (owner, 2026-07-15). A backward seek
    /// always crosses into an earlier keyframe, which is why only forward looked broken.
    ///
    /// The frames from that keyframe up to the target must still be **decoded** — the frame
    /// at the target references them — but must not be **shown**. So they are fed with
    /// `kCMSampleAttachmentKey_DoNotDisplay` and the clock is anchored at the target, not at
    /// the keyframe.
    private var seekTargetSecs: Double?
    private var requesting = false
    private var done = false
    private var framesFed = 0 // diagnostics only (PB_TRACE)

    // §0 seek-scoped diagnostics — reset per seek so the trace reads one seek, not a
    // lifetime tally. (v1's §0 could not work: `framesFed` is a lifetime counter, so
    // "look for a run of preroll=true" printed almost nothing.) All touched only on
    // `queue`; emitted behind `pbTrace` (PB_TRACE), so they compile to nothing hot.
    private var diagPrerollCount = 0
    private var diagLastPrerollPtsSecs = Double.nan
    private var diagFirstPacketLogged = false
    private var diagSawNotReady = false
    private var diagProvideCalls = 0
    private var diagSeekStart = DispatchTime.now()

    /// Reset the per-seek diagnostic tally (plan §0). Called on `queue` whenever a fresh
    /// feed epoch begins (a seek re-arm, or the initial feed).
    private func resetSeekDiag() {
        diagPrerollCount = 0
        diagLastPrerollPtsSecs = .nan
        diagFirstPacketLogged = false
        diagSawNotReady = false
        diagProvideCalls = 0
        diagSeekStart = DispatchTime.now()
    }

    // Playback-smoothness diagnostics (PB_TRACE): per-window `demux_read_packet` latency, so a
    // steady-state stutter can be pinned on SMB read spikes (the reader starving the display
    // layer) vs decode/present. Reads are normally instant (the OS/SMB client has the bytes
    // buffered); a cache miss that must fetch over the network blocks the reader thread, and
    // with only the layer's queue as a buffer, a block longer than the queue depth drops a
    // frame. Reset each window; emitted ~every 2 s of feeding. Touched only on `queue`.
    private var pbWinStart = DispatchTime.now()
    private var pbWinReads = 0
    private var pbWinReadNanos: UInt64 = 0
    private var pbWinMaxReadNanos: UInt64 = 0
    private var pbWinSlow20 = 0 // reads > 20 ms
    private var pbWinSlow40 = 0 // reads > 40 ms (≈ a 24 fps frame interval — a missed deadline)

    /// Fold one `demux_read_packet` wall-time into the current window; emit + reset ~every 2 s.
    /// Called only when `pbTraceEnabled`.
    private func recordReadLatency(_ nanos: UInt64) {
        pbWinReads += 1
        pbWinReadNanos &+= nanos
        if nanos > pbWinMaxReadNanos { pbWinMaxReadNanos = nanos }
        let ms = Double(nanos) / 1e6
        if ms > 20 { pbWinSlow20 += 1 }
        if ms > 40 { pbWinSlow40 += 1 }
        let elapsed =
            Double(DispatchTime.now().uptimeNanoseconds &- pbWinStart.uptimeNanoseconds) / 1e9
        guard elapsed >= 2.0 else { return }
        let avgMs = Double(pbWinReadNanos) / Double(pbWinReads) / 1e6
        pbTrace(
            String(
                format:
                    "sb-read diag: %.1fs — %d reads (feed %.1f/s), avg %.1fms max %.1fms, slow>20ms=%d slow>40ms=%d",
                elapsed, pbWinReads, Double(pbWinReads) / elapsed, avgMs,
                Double(pbWinMaxReadNanos) / 1e6, pbWinSlow20, pbWinSlow40))
        pbWinStart = DispatchTime.now()
        pbWinReads = 0
        pbWinReadNanos = 0
        pbWinMaxReadNanos = 0
        pbWinSlow20 = 0
        pbWinSlow40 = 0
    }

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
            let out = DemuxOpen(
                ok: true,
                width: demux_width(p),
                height: demux_height(p),
                durationSecs: demux_duration_secs(p),
                fps: demux_fps(p),
                hasAudio: demux_has_audio(p),
                doviProfile: demux_dovi_profile(p))
            pbTrace(
                "sb-demux opened: codec=\(demux_codec(p)) \(out.width)x\(out.height) "
                    + "nal=\(demux_nal_length_size(p)) lp=\(demux_length_prefixed(p)) "
                    + "audio=\(out.hasAudio) fps=\(out.fps) tb=\(self.tbNum)/\(self.tbDen)")
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
        // Fix "zero" HERE, from the container — not from whichever packet arrives first.
        //
        // ⚠ They are the same thing only when feeding starts at the top of the file. A
        // resume (task #94.2) starts at a keyframe minutes in, and taking the origin from
        // that packet would redefine zero as the resume point: the clock, the scrubber, and
        // the subtitles would all shift by the resume offset, and the seek pre-roll would
        // measure every frame as before its own target and hide the whole film.
        // `i64.min` (no declared start_time) still falls back to the first packet below.
        origin = demux_start_time_units(p)
        return true
    }

    /// Position the demuxer before the first feed — the resume position (task #94.2).
    ///
    /// Separate from [`seek(seconds:layer:then:)`] because there is nothing to flush or
    /// re-anchor yet: the layer has never been fed and the clock has never started. All it
    /// shares is `seekTargetSecs`, which does the real work either way — the landed
    /// keyframe is at/before the target, so the frames up to it must be decoded but not
    /// shown, and the clock must anchor at the target rather than at the keyframe.
    func seekBeforeStart(seconds: Double) {
        queue.async { [weak self] in
            guard let self, self.ptr != 0 else { return }
            demux_seek(self.ptr, max(0, seconds))
            guard demux_state(self.ptr) != 2 else { return } // open failed; feed from 0
            self.seekTargetSecs = max(0, seconds)
        }
    }

    /// Start (or restart) feeding `layer` from renderer readiness. The block runs on
    /// `queue`; the callbacks fire the presenter's reveal / EOS / error, on the main
    /// actor. Idempotent — a second call while already requesting is a no-op.
    func startFeeding(
        into layer: AVSampleBufferDisplayLayer,
        onFirstFrame: @escaping @Sendable (CMTime, UInt64) -> Void,
        onEnd: @escaping @Sendable () -> Void,
        onError: @escaping @Sendable () -> Void
    ) {
        queue.async { [weak self] in
            guard let self else { return }
            self.feedLayer = layer
            self.onFirstFrame = onFirstFrame
            self.onEnd = onEnd
            self.onError = onError
            self.feedEpoch = 0 // the initial feed (a seekBeforeStart resume is still epoch 0)
            self.resetSeekDiag()
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
        diagProvideCalls += 1
        if diagSawNotReady {
            // The direct H1 discriminator (plan §0): the readiness callback DID re-invoke
            // provide after the queue reported full — the layer is draining, i.e. NOT
            // starved. If this line never appears after the "not-ready" line below while a
            // seek is still held with no anchor, that is the starvation signature.
            pbTrace(
                "sb-seek diag epoch=\(feedEpoch): provide re-invoked (#\(diagProvideCalls)) after not-ready")
        }
        while layer.isReadyForMoreMediaData {
            guard ptr != 0, !done, let fmt = formatDesc else {
                layer.stopRequestingMediaData()
                requesting = false
                return
            }
            let t0 = pbTraceEnabled ? DispatchTime.now().uptimeNanoseconds : 0
            let rv = demux_read_packet(ptr)
            if pbTraceEnabled { recordReadLatency(DispatchTime.now().uptimeNanoseconds &- t0) }
            let n = Int(rv.len())
            if n == 0 {
                let state = demux_state(ptr)
                done = true
                layer.stopRequestingMediaData()
                requesting = false
                // A seek that hit EOS without ever reaching its target (near the end of a
                // clip, or a stream whose last frames precede it) would otherwise never
                // anchor — leaving the clock held at rate 0 forever, i.e. a frozen picture
                // with no way out. Anchor at the target anyway and let EOS handle the rest.
                if let t = seekPolicy.eosAnchor(
                    seekTarget: seekTargetSecs, firstFrameSent: firstFrameSent)
                {
                    firstFrameSent = true
                    seekTargetSecs = nil
                    let anchor = CMTime(seconds: t, preferredTimescale: 600)
                    let ep = feedEpoch
                    pbTrace("sb-seek diag epoch=\(ep): EOS before target — forcing anchor at \(t)s")
                    onFirstFrame.map { cb in DispatchQueue.main.async { cb(anchor, ep) } }
                }
                if state == 1 {
                    onEnd.map { cb in DispatchQueue.main.async { cb() } }
                } else if state == 2 {
                    onError.map { cb in DispatchQueue.main.async { cb() } }
                }
                return
            }
            let pts = demux_packet_pts(ptr)
            let dts = demux_packet_dts(ptr) // always valid — Rust synthesizes missing DTS
            let dur = demux_packet_duration(ptr)
            if origin == Int64.min {
                origin = pts != Int64.min ? pts : (dts != Int64.min ? dts : 0)
            }
            if !diagFirstPacketLogged {
                diagFirstPacketLogged = true
                // The real first post-seek packet timestamp (the landed keyframe) — recorded
                // rather than fabricated (plan §T4), so a human can see whether the demuxer
                // took the normal (≤ target) or fallback (> target) branch.
                pbTrace(
                    "sb-seek diag epoch=\(feedEpoch): first post-seek packet pts=\(pts) dts=\(dts) "
                        + "key=\(demux_packet_is_key(ptr)) target=\(seekTargetSecs.map { String($0) } ?? "nil")")
            }
            guard let sb = makeSampleBuffer(rv, count: n, fmt: fmt, pts: pts, dts: dts, dur: dur)
            else {
                pbTrace("sb-demux: makeSampleBuffer FAILED (\(n) bytes, pts=\(pts) dts=\(dts))")
                continue
            }
            let ptsSecs = CMTimeGetSeconds(cmTime(pts))
            let dtsSecs = CMTimeGetSeconds(cmTime(dts))
            // The pure policy decides pre-roll (decode-only, keyframe → target) and whether
            // this is the first displayable frame that anchors the clock (plan §T1).
            let decision = seekPolicy.decide(
                seekTarget: seekTargetSecs, ptsSecs: ptsSecs, dtsSecs: dtsSecs,
                firstFrameSent: firstFrameSent)
            if decision.doNotDisplay {
                Self.markDoNotDisplay(sb)
                diagPrerollCount += 1
                diagLastPrerollPtsSecs = ptsSecs
            }
            layer.enqueue(sb)
            framesFed += 1
            if framesFed <= 3 || framesFed % 300 == 0 {
                pbTrace(
                    "sb-demux fed #\(framesFed) pts=\(pts) dts=\(dts) key=\(demux_packet_is_key(ptr)) preroll=\(decision.doNotDisplay) status=\(layer.status.rawValue)")
            }
            // Anchor on the first frame that will actually be SEEN — so the pre-roll is
            // already enqueued (and decoding) before the clock starts.
            if let anchorSecs = decision.anchorSecs {
                firstFrameSent = true
                let wasSeek = seekTargetSecs != nil
                seekTargetSecs = nil
                // A seek anchors at the target (quantized, as before). The initial feed
                // anchors at the EXACT decode time `cmTime(dts)` — a B-frame stream's
                // opening IDR has a DTS earlier than its PTS (here synthesized negative),
                // and starting the clock at the PTS makes those frames "late" so the layer
                // drops the IDR (no picture). Using the exact CMTime avoids a 1/600 requant.
                let anchor =
                    wasSeek ? CMTime(seconds: anchorSecs, preferredTimescale: 600) : cmTime(dts)
                let ep = feedEpoch
                let elapsedMs =
                    Double(DispatchTime.now().uptimeNanoseconds &- diagSeekStart.uptimeNanoseconds)
                    / 1e6
                pbTrace(
                    "sb-seek diag epoch=\(ep): ANCHOR at \(anchorSecs)s (wasSeek=\(wasSeek) landed pts=\(ptsSecs)) "
                        + "preroll=\(diagPrerollCount) lastPrerollPts=\(diagLastPrerollPtsSecs) "
                        + "elapsed=\(String(format: "%.1f", elapsedMs))ms")
                onFirstFrame.map { cb in DispatchQueue.main.async { cb(anchor, ep) } }
            }
        }
        // Fell out of the loop because the layer stopped wanting data (a `done`/`ptr==0`
        // exit returned above). If a seek is still holding the clock with no anchor yet,
        // this is the starvation signature (plan §0/H1): the pre-roll filled the renderer
        // queue and the held clock cannot drain it. The follow-up "provide re-invoked" line
        // (top of this function) tells starvation (never re-invoked) from a normal refill.
        if !firstFrameSent, seekTargetSecs != nil, !diagSawNotReady {
            diagSawNotReady = true
            pbTrace(
                "sb-seek diag epoch=\(feedEpoch): isReadyForMoreMediaData=false, preroll=\(diagPrerollCount), "
                    + "NO anchor yet — H1 starvation if provide does not re-fire")
        }
    }

    /// Mark one sample **decode-only**: VideoToolbox decodes it (later frames reference
    /// it) and the layer never shows it. The seek pre-roll's whole reason for existing.
    private static func markDoNotDisplay(_ sb: CMSampleBuffer) {
        guard
            let arr = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: true),
            CFArrayGetCount(arr) > 0
        else { return }
        let dict = unsafeBitCast(CFArrayGetValueAtIndex(arr, 0), to: CFMutableDictionary.self)
        CFDictionarySetValue(
            dict,
            Unmanaged.passUnretained(kCMSampleAttachmentKey_DoNotDisplay).toOpaque(),
            Unmanaged.passUnretained(kCFBooleanTrue).toOpaque())
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

    /// Seek to `seconds` (keyframe at/before), flush the renderer, and re-feed from the new
    /// position under generation `epoch`. `then(ok)` reports on the main actor whether the
    /// **demux command** succeeded — NOT that the target frame is visible (plan §H1c: that
    /// is `onFirstFrame`, which the presenter epoch-gates). The old return fabricated a
    /// `CMTime(target)` the caller only ever tested for nil-ness, so this is a plain Bool.
    func seek(
        seconds: Double, epoch: UInt64, layer: AVSampleBufferDisplayLayer,
        then: @escaping @Sendable (Bool) -> Void
    ) {
        queue.async { [weak self] in
            guard let self, self.ptr != 0 else {
                DispatchQueue.main.async { then(false) }
                return
            }
            layer.flush()
            self.feedEpoch = epoch
            self.resetSeekDiag()
            pbTrace("sb-seek diag epoch=\(epoch): seek→\(max(0, seconds))s (demux command issued)")
            demux_seek(self.ptr, seconds)
            if demux_state(self.ptr) == 2 {
                DispatchQueue.main.async { then(false) }
                return
            }
            // Re-feed from the landed keyframe. `seekTargetSecs` is what makes the frames
            // between it and the target decode-only, and what the clock anchors at — see
            // that field. Without it a forward seek walks BACKWARD to the keyframe.
            self.seekTargetSecs = max(0, seconds)
            self.firstFrameSent = false
            self.done = false
            self.armFeeding()
            DispatchQueue.main.async { then(true) }
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
