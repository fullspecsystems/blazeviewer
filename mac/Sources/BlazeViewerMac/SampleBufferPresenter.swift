import AVFoundation
import AppKit
import CoreMedia
import PbMacFfi
import PbSeek
import VideoToolbox

/// The macOS **sample-buffer video presenter** (video-overhaul Phase 3): plays a
/// container AVFoundation can't demux (MKV/WebM) by letting FFmpeg (in Rust) demux
/// it into compressed access units, wrapping each in a `CMSampleBuffer`, and handing
/// decode to VideoToolbox via an `AVSampleBufferDisplayLayer` under an
/// `AVSampleBufferRenderSynchronizer`. This is the Dolby-Vision / HDR end-state for
/// those containers: system decode + correct color, where the FFmpeg **Session**
/// route can only render an HDR10-compatible base layer through the GPU shader.
///
/// It slots in beside `NativeVideoPlayer` (the `AVPlayer` route) behind the same
/// ad-hoc facade: the same construction shape, the same command methods, and it
/// reports state back through the identical `nativeVideo*` callbacks on `CoreModel`
/// — so the Rust core drives it through the shared `Native` proxy with no third
/// state machine. The display layer drops into the SAME `MetalCanvasNSView`
/// container slot the `AVPlayerLayer` uses (both are `CALayer`s), reusing the
/// reveal-on-first-frame, letterbox, clipping, transform, and teardown machinery.
///
/// Backpressure: the display layer's `requestMediaDataWhenReady(on:)` callback IS
/// the flow-control signal — the reader pulls one compressed packet per iteration
/// only while `isReadyForMoreMediaData`, so the clip is never preloaded and the
/// compressed queue stays bounded by the renderer's appetite (plan §3A).
///
/// Scope note (0C gate): video playback + pause/resume/stop/scale/relayout are
/// wired; **audio** (§3C) and **seek/frame-step** (§3D) are follow-on slices —
/// stubbed here so the routing is total. The gate this proves is *DoVi/HDR visual
/// correctness on the physical display*, which is owner-verified.
@MainActor
final class SampleBufferPresenter {
    /// Identity from the core; a command/callback for a different session is ignored.
    let sessionId: UInt64

    private let displayLayer = AVSampleBufferDisplayLayer()
    private let audioRenderer = AVSampleBufferAudioRenderer()
    private let synchronizer = AVSampleBufferRenderSynchronizer()
    private weak var canvas: MetalCanvasNSView?
    private weak var model: CoreModel?

    /// The core's scale mode (0 Fit / 1 Fill / 2 Original), mirrored onto the layer.
    private var scaleMode: UInt8
    private let muted: Bool
    /// Session-only resume position (task #94.2) — applied via an initial seek once
    /// seek lands (slice E); `0` = play from the start.
    private let startSecs: Double

    /// The owned off-main demux pointer + feed loop (mirrors `OwnedAudioDecoder`).
    private let reader = DemuxReader()
    /// The owned off-main audio decoder + feed loop, on the same synchronizer (§3C).
    private let audioFeeder = AudioSampleFeeder()

    private var revealed = false
    private var reportedOpened = false
    private var ended = false
    private var failedOut = false
    private var audioStarted = false
    private var statusObs: NSKeyValueObservation?
    private var timeObserver: Any?
    private var durationSecs: Double = 0
    private var fps: Double = 0
    /// The seek currently in flight (plan §H1c). Its `epoch` gates every callback — the
    /// video anchor, the audio completion, the reader completion — so a stale seek can't
    /// re-anchor the clock after a newer one has taken over (the scrubber issues up to
    /// ~16/s, so this is expected traffic). Its `target`/`source` label the §0 trace. Starts
    /// as the initial feed (epoch 0, play); rebuilt per seek in `performSeek`.
    private var seek: SeekContext = .initial(desiredRate: 1.0)
    /// The user's **actual play intent** — `1` playing, `0` paused — updated ONLY by explicit
    /// commands (play/pause/step/EOS), never by a seek's clock-hold. The post-seek anchor
    /// restores THIS, not `synchronizer.rate`.
    ///
    /// ⚠ The §0 trace (2026-07-15) named this bug: a scrubber *click* fires two seeks to the
    /// same target (`onChanged` then `onEnded`); the first sets `synchronizer.rate = 0` to
    /// hold the clock, and the second — reading `synchronizer.rate` to capture the pre-seek
    /// rate — saw that **hold** and captured "paused", so the seek settled at rate 0. Reading
    /// a preserved intent instead of the transient held rate is the fix (it also makes a
    /// pause pressed *during* a seek stick, since the anchor reads this live).
    private var desiredPlaybackRate: Float = 1.0

    init(
        sessionId: UInt64, scaleMode: UInt8, muted: Bool, startSecs: Double,
        canvas: MetalCanvasNSView, model: CoreModel
    ) {
        self.sessionId = sessionId
        self.scaleMode = scaleMode
        self.muted = muted
        self.startSecs = startSecs
        self.canvas = canvas
        self.model = model

        displayLayer.videoGravity = .resizeAspect
        canvas.attachVideoSublayer(displayLayer) // hidden until the first frame
        synchronizer.addRenderer(displayLayer)
        synchronizer.addRenderer(audioRenderer) // audio shares the video's clock (§3B)
        audioRenderer.isMuted = muted
        relayout()

        // Safety net (§3F): if the display layer fails to decode (an unsupported
        // stream the demux self-probe didn't catch), report a recoverable failure
        // so the core falls back to the Session route rather than showing nothing.
        statusObs = displayLayer.observe(\.status, options: [.new]) { [weak self] layer, _ in
            // KVO for `status` isn't guaranteed on the main thread — read the layer
            // state here, then hop to the main actor for the model callback.
            let status = layer.status
            let err = layer.error?.localizedDescription
            let sid = self?.sessionId ?? 0
            pbTrace("sample-buffer video \(sid): display status=\(status.rawValue) err=\(err ?? "-")")
            guard status == .failed else { return }
            let msg = err ?? "Sample-buffer decode failed"
            DispatchQueue.main.async {
                guard let self, !self.failedOut else { return }
                self.failedOut = true
                pbTrace("sample-buffer video \(self.sessionId): display layer FAILED — \(msg)")
                self.model?.nativeVideoFailed(self.sessionId, error: msg, recoverable: true)
            }
        }

        // ~20 Hz position → the info-line scrubber (the core keeps no video clock;
        // the synchronizer's timebase is the source, like the AVPlayer route).
        timeObserver = synchronizer.addPeriodicTimeObserver(
            forInterval: CMTime(seconds: 0.05, preferredTimescale: 600), queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.publishProgress() }
        }

        pbTrace("sample-buffer video \(sessionId): opening")
        // Open the demuxer off the main actor; on success it has built the format
        // description and can start feeding the layer.
        reader.open(sessionId: sessionId) { [weak self] result in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.onOpened(result)
            }
        }
    }

    /// Open result → report opened facts + start the feed, or fail (fall back to
    /// the Session route). `nil` / unsupported = a classified, recoverable failure.
    private func onOpened(_ result: DemuxOpen?) {
        guard let result, result.ok else {
            let msg = "This video can't be played by the sample-buffer decoder"
            pbTrace("sample-buffer video \(sessionId): open FAILED — falling back")
            model?.nativeVideoFailed(sessionId, error: msg, recoverable: true)
            return
        }
        durationSecs = result.durationSecs
        fps = result.fps
        let durationMs: Int64 = result.durationSecs > 0
            ? Int64((result.durationSecs * 1000).rounded()) : -1
        if !reportedOpened {
            reportedOpened = true
            model?.nativeVideoOpened(sessionId, durationMs: durationMs, hasAudio: result.hasAudio)
        }
        relayout() // dimensions known → Original can size 1:1
        pbTrace(
            "sample-buffer video \(sessionId): opened \(result.width)x\(result.height) "
                + "dovi=\(result.doviProfile)")

        // The resume position (task #94.2) shared by BOTH renderers. The core remembers
        // where you left a clip and hands it back as `startSecs`; `<= 0.5` plays from the
        // start (matches NativeVideoPlayer). Nil = play from zero.
        let resumeTarget: Double? =
            (startSecs > 0.5 && (durationSecs <= 0 || startSecs < durationSecs)) ? startSecs : nil

        // Audio (§3C): open the FFmpeg decoder over the same container and feed the
        // audio renderer on the shared synchronizer. It buffers (enqueued 0-based)
        // until the first video frame starts the clock — so audio never leads the
        // picture. Silent when the clip has no openable audio track.
        //
        // `startSecs: resumeTarget` is the fix for the resume-audio bug (plan §H2', new
        // today): the old code seeked only the video reader (below), so a resumed MKV
        // played its picture minutes in while the audio sat 0-based in the past and was
        // discarded. The audio decoder now resumes to the same spot at open.
        if result.hasAudio {
            audioFeeder.open(sessionId: sessionId, startSecs: resumeTarget ?? 0) {
                [weak self] ok in
                MainActor.assumeIsolated {
                    guard let self, ok, !self.audioStarted else { return }
                    self.audioStarted = true
                    self.audioFeeder.startFeeding(into: self.audioRenderer)
                    // Tell the core which track the decoder's policy actually chose, so
                    // Playback ▸ Audio can tick it (task #99). Reported, never guessed.
                    self.reportActiveAudioTrack()
                }
            }
        }

        // Seek the video reader BEFORE feeding, so the first frame revealed is already the
        // resume frame — the wgpu poster holds until then, so there is no jump to frame 0
        // and back. Accepted here but **never used** before (owner, 2026-07-15), which is
        // why MKVs "forgot" while MP4s resumed.
        if let resumeTarget {
            reader.seekBeforeStart(seconds: resumeTarget)
        }

        // Drive the layer from renderer readiness; reveal + start the clock on the
        // first enqueued frame.
        reader.startFeeding(
            into: displayLayer,
            onFirstFrame: { [weak self] anchor, epoch in
                MainActor.assumeIsolated { self?.onDecodeAnchor(anchor, epoch: epoch) }
            },
            onEnd: { [weak self] in
                MainActor.assumeIsolated { self?.onEnded() }
            },
            onError: { [weak self] in
                MainActor.assumeIsolated {
                    guard let self else { return }
                    self.model?.nativeVideoFailed(
                        self.sessionId, error: "Video decode failed", recoverable: true)
                }
            })
    }

    /// Anchor the synchronizer at a **decode-time** anchor — the DTS of the first
    /// frame after open or after a seek — and apply the pending rate. The DTS (not
    /// the PTS) is the anchor because a B-frame stream's opening IDR decodes before
    /// it presents (a negative synthesized DTS here); anchoring at the PTS would make
    /// those frames "late" and the display layer would drop the IDR (no picture). The
    /// first anchor also reveals the layer (poster → video, no flash).
    private func onDecodeAnchor(_ anchor: CMTime, epoch: UInt64) {
        // Epoch gate (plan §H1c): a stale anchor from a superseded seek must not touch the
        // clock, the renderers, or the reveal. The newest seek's anchor (or the initial
        // feed's, epoch 0) is the only one allowed through; it does the reveal, so gating
        // never strands the poster — it just defers the reveal to the settled seek.
        guard epoch == seek.epoch else {
            pbTrace(
                "sample-buffer video \(sessionId): stale anchor epoch=\(epoch) (current \(seek.epoch)) — ignored")
            return
        }
        if !revealed {
            revealed = true
            // Unhide BOTH the display layer and its container: attachVideoSublayer
            // hides the inner layer too (belt-and-suspenders), and revealVideoLayer
            // only unhides the container — so without this the container shows its
            // opaque black over a still-hidden display layer (black, no video). This
            // mirrors NativeVideoPlayer.revealAndPlay's `layer.isHidden = false`.
            displayLayer.isHidden = false
            canvas?.revealVideoLayer()
            pbTrace("sample-buffer video \(sessionId): revealed")
        }
        // Restore the user's LIVE intent, not a value captured at seek-issue time — so a
        // pause pressed while the seek was settling sticks, and the click double-seek can't
        // latch rate 0 (see `desiredPlaybackRate`).
        let rate = desiredPlaybackRate
        synchronizer.setRate(rate, time: anchor)
        model?.nativeVideoStateChanged(sessionId, state: rate != 0 ? 2 : 3)
    }

    private func onEnded() {
        guard !ended else { return }
        ended = true
        desiredPlaybackRate = 0 // EOS parks; a scrub-back stays paused, replay (resume) restores
        synchronizer.rate = 0 // park the last frame (AVSampleBufferDisplayLayer holds it)
        pbTrace("sample-buffer video \(sessionId): EOS — parking last frame")
        model?.nativeVideoEnded(sessionId)
    }

    private func publishProgress() {
        guard revealed else { return }
        let pos = max(0, CMTimeGetSeconds(synchronizer.currentTime()))
        model?.updateVideoProgress(
            sessionId,
            elapsed: pos.isFinite ? pos : 0,
            total: durationSecs.isFinite ? durationSecs : 0,
            playing: synchronizer.rate != 0)
    }

    // MARK: - Command facade (mirrors NativeVideoPlayer)

    func pause() {
        desiredPlaybackRate = 0 // the user's intent — the anchor restores THIS, not the hold
        synchronizer.rate = 0
        model?.nativeVideoStateChanged(sessionId, state: 3) // Paused
    }

    func resume() {
        desiredPlaybackRate = 1.0 // intent = play; the replay/anchor restores it
        if ended {
            // Replay: the replay seek's anchor reads `desiredPlaybackRate` (now 1), so it
            // re-anchors at rate 1 without racing an async seek landing.
            ended = false
            synchronizer.rate = 1.0
            seek(toFraction: 0)
        } else {
            synchronizer.rate = 1.0
        }
        model?.nativeVideoStateChanged(sessionId, state: 2) // Playing
    }

    func setMuted(_ muted: Bool) {
        audioRenderer.isMuted = muted
    }

    func setScaleMode(_ mode: UInt8) {
        guard mode != scaleMode else { return }
        scaleMode = mode
        relayout()
    }

    /// Seek to a fraction of the duration (the scrubber).
    func seek(toFraction fraction: Double) {
        guard durationSecs > 0 else { return }
        performSeek(
            toSeconds: max(0.0, min(1.0, fraction)) * durationSecs, source: .scrub, report: nil)
    }

    /// Seek by a signed millisecond delta (arrow keys). Reports completion tagged
    /// with the core's `generation` so a superseded seek is told from a clean landing.
    func seek(byMilliseconds deltaMs: Int64, generation: UInt64) {
        let cur = max(0, CMTimeGetSeconds(synchronizer.currentTime()))
        performSeek(toSeconds: cur + Double(deltaMs) / 1000.0, source: .arrow, report: generation)
    }

    /// Frame-step (`,`/`.`): pause, then nudge by one frame via a tiny seek. The
    /// display layer buffers ahead while paused, so a forward step usually lands
    /// from the buffer; backward re-seeks to the keyframe and decodes forward.
    func step(forward: Bool) {
        desiredPlaybackRate = 0 // a step leaves playback paused — the anchor must park, not run
        synchronizer.rate = 0
        let cur = max(0, CMTimeGetSeconds(synchronizer.currentTime()))
        let frame = fps > 0 ? 1.0 / fps : 1.0 / 30.0
        performSeek(toSeconds: cur + (forward ? frame : -frame), source: .step, report: nil)
        model?.nativeVideoStateChanged(sessionId, state: 3) // Paused
    }

    /// Generation-safe seek (§3D/§H1c): build a fresh `SeekContext` (new epoch, the target,
    /// the pre-seek rate as the desired-after rate, the source), hold the clock, and flush +
    /// re-seek both renderers under that epoch. The re-anchor happens in `onDecodeAnchor`
    /// when the first post-seek frame lands — and only if this seek is still current, so a
    /// superseded seek can't move the clock. `report` (when set) tags the core's
    /// seek-completion callback.
    private func performSeek(toSeconds secs: Double, source: SeekSource, report: UInt64?) {
        let target = max(0.0, min(durationSecs > 0 ? durationSecs : secs, secs))
        ended = false
        // The rate to restore after the seek settles is the user's PRESERVED intent — NOT
        // `synchronizer.rate`, which a prior in-flight seek may have already forced to 0 (the
        // click double-seek bug the §0 trace found). Carried for the trace; the anchor reads
        // the live `desiredPlaybackRate`.
        let desired = desiredPlaybackRate
        let epoch = seek.epoch &+ 1
        seek = SeekContext(
            epoch: epoch, target: target, desiredRateAfterSeek: desired, source: source)
        synchronizer.rate = 0 // hold the clock while both renderers reflush
        pbTrace(
            "sample-buffer video \(sessionId): seek epoch=\(epoch) → \(target)s source=\(source.rawValue) desiredRate=\(desired)")
        // Audio: epoch-tagged, and NOT dropped if the decoder is still opening (held +
        // applied at open). `applied == false` means "held for open" here, not a failure.
        audioFeeder.seek(seconds: target, epoch: epoch, renderer: audioRenderer) {
            [weak self] applied in
            MainActor.assumeIsolated {
                guard let self, epoch == self.seek.epoch else { return }
                pbTrace(
                    "sample-buffer video \(self.sessionId): audio seek epoch=\(epoch) applied=\(applied)")
            }
        }
        reader.seek(seconds: target, epoch: epoch, layer: displayLayer) { [weak self] ok in
            MainActor.assumeIsolated {
                guard let self, let g = report else { return }
                // `ok` is "demux command succeeded", not "frame visible" (plan §H1c); the
                // real landing is the epoch-gated `onDecodeAnchor`. This tells the core the
                // seek command finished so its generation bookkeeping can settle.
                let current = epoch == self.seek.epoch
                self.model?.nativeVideoSeekCompleted(
                    self.sessionId, generation: g, finished: current && ok)
            }
        }
    }

    // MARK: - Transform placement (parity with a still / the AVPlayer route)

    private struct PlacementKey: Equatable {
        let x, y, w, h: Float
        let rot: UInt8
        let scale, cw, ch: CGFloat
    }
    private var lastPlacement: PlacementKey?

    /// Place the display layer to match the core's still geometry (Fit/Fill/Original,
    /// zoom, pan, rotation) — identical math to `NativeVideoPlayer.applyPlacement`,
    /// on the display layer instead of the player layer.
    func relayout() {
        guard let canvas else { return }
        let bounds = canvas.bounds
        let scale = canvas.window?.backingScaleFactor ?? 2.0
        let p = model?.videoPlacement()
        if let p, p.valid, p.w > 0.5, p.h > 0.5 {
            applyPlacement(p, bounds: bounds, scale: scale)
        } else {
            lastPlacement = nil
            relayoutScaleMode(bounds: bounds, scale: scale)
        }
    }

    private func applyPlacement(_ p: VideoPlacementFfi, bounds: CGRect, scale: CGFloat) {
        let key = PlacementKey(
            x: p.x, y: p.y, w: p.w, h: p.h, rot: p.rotation,
            scale: scale, cw: bounds.width, ch: bounds.height)
        if lastPlacement == key { return }
        lastPlacement = key

        let footW = CGFloat(p.w) / scale
        let footH = CGFloat(p.h) / scale
        let centerX = (CGFloat(p.x) + CGFloat(p.w) / 2) / scale
        let centerY = bounds.height - (CGFloat(p.y) + CGFloat(p.h) / 2) / scale
        let swaps = p.rotation == 1 || p.rotation == 3
        let bw = swaps ? footH : footW
        let bh = swaps ? footW : footH
        let angle = -CGFloat(p.rotation) * .pi / 2

        CATransaction.begin()
        CATransaction.setDisableActions(true)
        defer { CATransaction.commit() }
        displayLayer.contentsScale = scale
        displayLayer.videoGravity = .resize // bounds already carry the displayed aspect
        displayLayer.anchorPoint = CGPoint(x: 0.5, y: 0.5)
        displayLayer.bounds = CGRect(x: 0, y: 0, width: bw, height: bh)
        displayLayer.position = CGPoint(x: centerX, y: centerY)
        displayLayer.transform = CATransform3DMakeRotation(angle, 0, 0, 1)
    }

    private func relayoutScaleMode(bounds: CGRect, scale: CGFloat) {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        defer { CATransaction.commit() }
        displayLayer.transform = CATransform3DIdentity
        displayLayer.anchorPoint = CGPoint(x: 0.5, y: 0.5)
        displayLayer.position = CGPoint(x: bounds.midX, y: bounds.midY)
        displayLayer.bounds = CGRect(x: 0, y: 0, width: bounds.width, height: bounds.height)
        displayLayer.contentsScale = scale
        switch scaleMode {
        case 1: displayLayer.videoGravity = .resizeAspectFill // Fill
        default: displayLayer.videoGravity = .resizeAspect // Fit / Original
        }
    }

    // MARK: - Audio track selection (task #99)

    /// The container stream index this route is **actually playing** (`nil` until the
    /// decoder has opened and answered).
    ///
    /// Cached as the **raw fact**, not resolved to a picker row, because the two are known
    /// at different times: the decoder answers the moment audio opens, while the rows need
    /// the track catalog, whose probe is still in flight then. Resolving early was the bug —
    /// it produced "nothing is playing" against an empty row list and stuck there, so the
    /// menu had no tick until you picked something. The row is derived at menu-open instead,
    /// when both halves exist.
    private(set) var activeAudioStream: Int?

    /// Re-read which stream the decoder is on, and cache it. Async (the feeder owns the
    /// pointer on its serial queue), which is the other reason this cannot be resolved
    /// inside the synchronous `menuNeedsUpdate`.
    func reportActiveAudioTrack() {
        audioFeeder.currentTrack { [weak self] stream in
            MainActor.assumeIsolated {
                self?.activeAudioStream = stream
            }
        }
    }

    /// Switch to the audio track at picker row `row`. Reports the outcome to the core, which
    /// toasts only on a **confirmed** switch.
    func selectAudioTrack(row: Int) {
        guard let model else { return }
        let stream = model.audioRowFfStream(row)
        guard stream >= 0 else {
            model.audioTrackSwitched(row: row, ok: false) // not a track this route can reach
            return
        }
        let at = synchronizer.currentTime().seconds
        audioFeeder.switchTrack(
            stream, at: at.isFinite ? max(0, at) : 0, renderer: audioRenderer
        ) { [weak self] ok in
            MainActor.assumeIsolated {
                guard let self, let model = self.model else { return }
                // Re-read what is playing FIRST — on a refusal that is the old track, and on
                // a stale pick the decoder falls back to its policy, so neither the tick nor
                // the toast may trust the request.
                self.audioFeeder.currentTrack { stream in
                    MainActor.assumeIsolated {
                        self.activeAudioStream = stream
                        model.reportActiveAudioStream(stream)
                        model.audioTrackSwitched(row: row, ok: ok)
                    }
                }
            }
        }
    }

    /// Tear down fully: stop the clock, stop feeding + free the demuxer, drop the
    /// observer, detach the layer. Idempotent.
    func stop() {
        synchronizer.rate = 0
        statusObs?.invalidate()
        statusObs = nil
        reader.stop(layer: displayLayer)
        audioFeeder.stop(renderer: audioRenderer)
        if let timeObserver {
            synchronizer.removeTimeObserver(timeObserver)
            self.timeObserver = nil
        }
        canvas?.detachVideoLayer()
        pbTrace("sample-buffer video \(sessionId): stopped + torn down")
    }

    deinit {
        // Safety net if `stop()` wasn't called; the reader/feeder free their own
        // pointers in their deinits.
        statusObs?.invalidate()
    }
}
