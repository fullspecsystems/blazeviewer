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

    private var revealed = false
    private var reportedOpened = false
    private var ended = false
    private var timeObserver: Any?
    private var durationSecs: Double = 0

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
        relayout()

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

        // Drive the layer from renderer readiness; reveal + start the clock on the
        // first enqueued frame.
        reader.startFeeding(
            into: displayLayer,
            onFirstFrame: { [weak self] firstPTS in
                MainActor.assumeIsolated { self?.revealAndStart(at: firstPTS) }
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

    /// Unhide the container (poster → video, no flash) and start the synchronizer
    /// anchored at the first frame's PTS so the timeline matches the media clock.
    private func revealAndStart(at firstPTS: CMTime) {
        guard !revealed else { return }
        revealed = true
        synchronizer.setRate(1.0, time: firstPTS)
        canvas?.revealVideoLayer()
        model?.nativeVideoStateChanged(sessionId, state: 2) // Playing
        pbTrace("sample-buffer video \(sessionId): revealed + playing")
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
            // Replay: seek back to the start, then play (slice E supplies the real
            // flush+re-enqueue; for now re-feed from zero).
            ended = false
            seek(toFraction: 0)
        }
        synchronizer.rate = 1.0
        model?.nativeVideoStateChanged(sessionId, state: 2) // Playing
    }

    func setMuted(_ muted: Bool) {
        // Audio is a follow-on slice (§3C); no audio renderer yet to mute.
        _ = muted
    }

    func setScaleMode(_ mode: UInt8) {
        guard mode != scaleMode else { return }
        scaleMode = mode
        relayout()
    }

    /// Seek to a fraction of the duration. Placeholder for slice §3D (generation-safe
    /// flush + re-enqueue + re-anchor); wired so the scrubber routing is total.
    func seek(toFraction fraction: Double) {
        guard durationSecs > 0 else { return }
        let secs = max(0.0, min(1.0, fraction)) * durationSecs
        ended = false
        reader.seek(seconds: secs, layer: displayLayer) { [weak self] landedPTS in
            MainActor.assumeIsolated {
                guard let self, let landedPTS else { return }
                self.synchronizer.setRate(self.synchronizer.rate, time: landedPTS)
            }
        }
    }

    /// Seek by a signed millisecond delta (arrow keys). Placeholder (§3D) — reports
    /// completion so the proxy generation bookkeeping stays honest.
    func seek(byMilliseconds deltaMs: Int64, generation: UInt64) {
        let cur = max(0, CMTimeGetSeconds(synchronizer.currentTime()))
        let target = max(0.0, min(durationSecs, cur + Double(deltaMs) / 1000.0))
        ended = false
        reader.seek(seconds: target, layer: displayLayer) { [weak self] landedPTS in
            MainActor.assumeIsolated {
                guard let self else { return }
                if let landedPTS { self.synchronizer.setRate(self.synchronizer.rate, time: landedPTS) }
                self.model?.nativeVideoSeekCompleted(
                    self.sessionId, generation: generation, finished: landedPTS != nil)
            }
        }
    }

    /// Frame-step (§3D placeholder): no-op until the paused single-frame path lands.
    func step(forward: Bool) {
        _ = forward
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

    /// Tear down fully: stop the clock, stop feeding + free the demuxer, drop the
    /// observer, detach the layer. Idempotent.
    func stop() {
        synchronizer.rate = 0
        reader.stop(layer: displayLayer)
        if let timeObserver {
            synchronizer.removeTimeObserver(timeObserver)
            self.timeObserver = nil
        }
        canvas?.detachVideoLayer()
        pbTrace("sample-buffer video \(sessionId): stopped + torn down")
    }

    deinit {
        // The reader frees its pointer in its own deinit; nothing main-actor-bound
        // to release here beyond what `stop()` already handled.
    }
}
