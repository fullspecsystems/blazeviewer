import AVFoundation
import AppKit
import CoreMedia
import PbMacFfi
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
    /// The rate to apply at the next decode anchor (first frame after open or seek):
    /// `1` playing, `0` paused. A seek captures the pre-seek rate here so the
    /// post-seek re-anchor restores it.
    private var pendingRate: Float = 1.0
    /// Bumped on every seek so a superseded seek's completion can't re-anchor the
    /// clock after a newer one has taken over (generation-safe, §3D).
    private var seekEpoch: UInt64 = 0

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

        // Audio (§3C): open the FFmpeg decoder over the same container and feed the
        // audio renderer on the shared synchronizer. It buffers (enqueued 0-based)
        // until the first video frame starts the clock — so audio never leads the
        // picture. Silent when the clip has no openable audio track.
        if result.hasAudio {
            audioFeeder.open(sessionId: sessionId) { [weak self] ok in
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

        // Resume position (task #94.2). The core remembers where you left a clip and hands
        // it back as `startSecs`; this route accepted it and **never used it**, so every
        // MKV restarted from zero while MP4s resumed correctly — the resume was being
        // recorded fine, this end just dropped it on the floor (owner, 2026-07-15).
        //
        // Seeking BEFORE feeding, rather than playing from 0 and seeking after, means the
        // first frame revealed is already the resume frame — the wgpu poster holds until
        // then, so there is no jump to frame 0 and back. `<= 0.5` plays from the start,
        // matching NativeVideoPlayer's rule.
        if startSecs > 0.5, durationSecs <= 0 || startSecs < durationSecs {
            reader.seekBeforeStart(seconds: startSecs)
        }

        // Drive the layer from renderer readiness; reveal + start the clock on the
        // first enqueued frame.
        reader.startFeeding(
            into: displayLayer,
            onFirstFrame: { [weak self] anchor in
                MainActor.assumeIsolated { self?.onDecodeAnchor(anchor) }
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
    private func onDecodeAnchor(_ anchor: CMTime) {
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
        synchronizer.setRate(pendingRate, time: anchor)
        model?.nativeVideoStateChanged(sessionId, state: pendingRate != 0 ? 2 : 3)
    }

    private func onEnded() {
        guard !ended else { return }
        ended = true
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
        synchronizer.rate = 0
        model?.nativeVideoStateChanged(sessionId, state: 3) // Paused
    }

    func resume() {
        if ended {
            // Replay: set the play rate first so the replay seek captures "was
            // playing" and re-anchors at rate 1 on completion (avoids the async
            // seek landing at rate 0 and overriding a resume set here).
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
        performSeek(toSeconds: max(0.0, min(1.0, fraction)) * durationSecs, report: nil)
    }

    /// Seek by a signed millisecond delta (arrow keys). Reports completion tagged
    /// with the core's `generation` so a superseded seek is told from a clean landing.
    func seek(byMilliseconds deltaMs: Int64, generation: UInt64) {
        let cur = max(0, CMTimeGetSeconds(synchronizer.currentTime()))
        performSeek(toSeconds: cur + Double(deltaMs) / 1000.0, report: generation)
    }

    /// Frame-step (`,`/`.`): pause, then nudge by one frame via a tiny seek. The
    /// display layer buffers ahead while paused, so a forward step usually lands
    /// from the buffer; backward re-seeks to the keyframe and decodes forward.
    func step(forward: Bool) {
        synchronizer.rate = 0
        let cur = max(0, CMTimeGetSeconds(synchronizer.currentTime()))
        let frame = fps > 0 ? 1.0 / fps : 1.0 / 30.0
        performSeek(toSeconds: cur + (forward ? frame : -frame), report: nil)
        model?.nativeVideoStateChanged(sessionId, state: 3) // Paused
    }

    /// Generation-safe seek (§3D): hold the clock, flush + re-seek both renderers,
    /// re-feed from the keyframe, then re-anchor the synchronizer at the target and
    /// restore the pre-seek rate — but only if a newer seek hasn't superseded this
    /// one. `report` (when set) tags the core's seek-completion callback.
    private func performSeek(toSeconds secs: Double, report: UInt64?) {
        let target = max(0.0, min(durationSecs > 0 ? durationSecs : secs, secs))
        ended = false
        seekEpoch &+= 1
        let epoch = seekEpoch
        // Restore this rate at the post-seek decode anchor (the accurate re-anchor
        // happens in onDecodeAnchor when the first post-seek frame lands — at its DTS,
        // so a negative-DTS keyframe isn't dropped as late, same as initial playback).
        pendingRate = synchronizer.rate != 0 ? 1.0 : 0.0
        synchronizer.rate = 0 // hold the clock while both renderers reflush
        audioFeeder.seek(seconds: target, renderer: audioRenderer)
        reader.seek(seconds: target, layer: displayLayer) { [weak self] landedPTS in
            MainActor.assumeIsolated {
                guard let self, let g = report else { return }
                let current = epoch == self.seekEpoch
                self.model?.nativeVideoSeekCompleted(
                    self.sessionId, generation: g, finished: current && landedPTS != nil)
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
