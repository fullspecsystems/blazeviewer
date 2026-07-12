import AVFoundation
import AppKit

/// The native macOS video player (task 79.9). On macOS the whole media pipeline is
/// `AVPlayer` + `AVPlayerLayer` — system decode, color, HDR, audio, timing, buffering,
/// and seeking — the single media authority. The Rust core keeps only a passive proxy,
/// commands this via `PlayVideo`/`PauseVideo`/`ResumeVideo`/`StopVideo`, and is fed the
/// player's authoritative state back through the `nativeVideo*` callbacks on `CoreModel`
/// so its play/pause/replay dispatch + policy see real state. See
/// `.taskmaster/plans/79.9-macos-video-playback.md`.
///
/// It attaches its `AVPlayerLayer` as a sublayer of the Metal canvas (hidden), reveals it
/// on the **first displayable frame** and only then starts playback (poster holds until
/// then — no black flash, no audio-before-picture), parks the last frame at end-of-stream,
/// and tears down fully — player paused, every observer removed, layer detached — on stop.
@MainActor
final class NativeVideoPlayer {
    /// Identity from the core; a command/callback for a different session is ignored.
    let sessionId: UInt64

    private let player: AVPlayer
    private let item: AVPlayerItem
    private let playerLayer: AVPlayerLayer
    private weak var canvas: MetalCanvasNSView?
    private weak var model: CoreModel?

    private var readyObs: NSKeyValueObservation?
    private var statusObs: NSKeyValueObservation?
    private var timeControlObs: NSKeyValueObservation?
    private var endObs: NSObjectProtocol?
    /// Periodic time observer driving the info-line scrubber's position (~5 Hz; SwiftUI
    /// animates the knob between updates). Removed on stop/deinit (it retains the player).
    private var timeObserver: Any?
    private var revealed = false
    private var reportedOpened = false
    private var ended = false
    /// The core's scale mode (0 Fit / 1 Fill / 2 Original), mirrored onto the layer so
    /// video honors 8/9/0 like a still. Zoom/pan/rotation parity is a later increment.
    private var scaleMode: UInt8

    init(
        url: URL, muted: Bool, sessionId: UInt64, scaleMode: UInt8, canvas: MetalCanvasNSView,
        model: CoreModel
    ) {
        self.sessionId = sessionId
        self.scaleMode = scaleMode
        self.canvas = canvas
        self.model = model
        item = AVPlayerItem(url: url)
        player = AVPlayer(playerItem: item)
        player.isMuted = muted
        // Hold the last frame at EOS (parity: end-of-stream parks; `P` replays).
        player.actionAtItemEnd = .pause
        playerLayer = AVPlayerLayer(player: player)
        canvas.attachVideoLayer(playerLayer) // hidden until the first frame
        relayout() // apply the initial scale mode (Fit/Fill now; Original once ready)

        // Reveal on the first displayable frame, then start playback — the poster
        // (drawn by wgpu into the Metal layer) shows until this fires.
        readyObs = playerLayer.observe(\.isReadyForDisplay, options: [.initial, .new]) {
            [weak self] layer, _ in
            MainActor.assumeIsolated {
                guard let self, layer.isReadyForDisplay, !self.revealed else { return }
                self.revealed = true
                layer.isHidden = false
                self.player.play()
                pbTrace("native video \(self.sessionId): revealed + playing")
            }
        }
        // Report opened facts once ready; surface a decode/open failure.
        statusObs = item.observe(\.status, options: [.new]) { [weak self] item, _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                switch item.status {
                case .readyToPlay:
                    self.reportOpened()
                case .failed:
                    let msg = item.error?.localizedDescription ?? "Video playback failed"
                    pbTrace("native video \(self.sessionId): FAILED \(msg)")
                    self.model?.nativeVideoFailed(self.sessionId, error: msg)
                default:
                    break
                }
            }
        }
        // Playback state → the core proxy (only once revealed, so the pre-roll paused
        // state doesn't masquerade as a user pause).
        timeControlObs = player.observe(\.timeControlStatus, options: [.new]) {
            [weak self] player, _ in
            MainActor.assumeIsolated {
                guard let self, self.revealed else { return }
                self.model?.nativeVideoStateChanged(self.sessionId, state: stateCode(player))
            }
        }
        endObs = NotificationCenter.default.addObserver(
            forName: .AVPlayerItemDidPlayToEndTime, object: item, queue: .main
        ) { [weak self] _ in
            // Delivered on `.main` (per `queue:`), so assume main-actor isolation — the
            // same bridge the KVO observers above use to touch `self`'s @MainActor state.
            MainActor.assumeIsolated {
                guard let self else { return }
                self.ended = true
                pbTrace("native video \(self.sessionId): EOS — parking last frame")
                self.model?.nativeVideoEnded(self.sessionId)
            }
        }
        // Drive the info-line scrubber's position (the core keeps no video clock, so the
        // player is the source). ~20 Hz so the knob tracks the true playhead directly — no
        // animation glide (which would lag a sample behind and snap forward on pause).
        timeObserver = player.addPeriodicTimeObserver(
            forInterval: CMTime(seconds: 0.05, preferredTimescale: 600), queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.publishProgress() }
        }
        pbTrace("native video \(sessionId): opening \(url.lastPathComponent) muted=\(muted)")
    }

    /// Push the current position/duration/playing to the model for the scrubber row.
    private func publishProgress() {
        let dur = item.duration
        let total = dur.isNumeric ? max(0, CMTimeGetSeconds(dur)) : 0
        let pos = max(0, CMTimeGetSeconds(player.currentTime()))
        model?.updateVideoProgress(
            sessionId,
            elapsed: pos.isFinite ? pos : 0,
            total: total.isFinite ? total : 0,
            playing: player.timeControlStatus == .playing)
    }

    /// Seek to `fraction` (0…1) of the duration — the info-line scrubber. Precise enough
    /// for scrubbing; AVPlayer coalesces a rapid series (a new seek supersedes a pending).
    func seek(toFraction fraction: Double) {
        let dur = item.duration
        guard dur.isNumeric, CMTimeGetSeconds(dur) > 0 else { return }
        ended = false
        let secs = max(0.0, min(1.0, fraction)) * CMTimeGetSeconds(dur)
        player.seek(
            to: CMTime(seconds: secs, preferredTimescale: 600),
            toleranceBefore: CMTime(seconds: 0.25, preferredTimescale: 600),
            toleranceAfter: CMTime(seconds: 0.25, preferredTimescale: 600))
        publishProgress() // immediate feedback; the observer catches up
    }

    /// Tell the core the clip opened (duration + audio presence), once.
    private func reportOpened() {
        guard !reportedOpened else { return }
        reportedOpened = true
        let d = item.duration
        let durationMs: Int64 = d.isNumeric ? Int64((CMTimeGetSeconds(d) * 1000).rounded()) : -1
        let hasAudio = item.tracks.contains { $0.assetTrack?.mediaType == .audio }
        model?.nativeVideoOpened(sessionId, durationMs: durationMs, hasAudio: hasAudio)
        relayout() // presentationSize is known now → Original can size 1:1
    }

    /// Mirror the core's scale mode (8/9/0) onto the layer. Re-lays-out on a real change.
    func setScaleMode(_ mode: UInt8) {
        guard mode != scaleMode else { return }
        scaleMode = mode
        relayout()
    }

    /// Place the `AVPlayerLayer` per the scale mode — coordinate-safe: Fit/Fill fill the
    /// canvas (aspect / aspect-fill); Original sizes the layer to the video's native pixel
    /// dimensions and *centers* it (symmetric, so no top/bottom-origin ambiguity). Zoom/
    /// pan/rotation parity (off-center placement) is a later increment.
    func relayout() {
        guard let canvas else { return }
        let bounds = canvas.bounds
        let scale = canvas.window?.backingScaleFactor ?? 2.0
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        defer { CATransaction.commit() }
        playerLayer.contentsScale = scale
        switch scaleMode {
        case 1: // Fill — cover the canvas, cropping the overflow
            playerLayer.videoGravity = .resizeAspectFill
            playerLayer.frame = bounds
        case 2: // Original — native 1:1, centered (clipped by the canvas if larger)
            let ps = player.currentItem?.presentationSize ?? .zero
            if ps.width > 0, ps.height > 0 {
                let w = ps.width / scale
                let h = ps.height / scale
                playerLayer.videoGravity = .resize // fill the exact 1:1 frame
                playerLayer.frame = CGRect(
                    x: (bounds.width - w) / 2, y: (bounds.height - h) / 2, width: w, height: h)
            } else {
                playerLayer.videoGravity = .resizeAspect
                playerLayer.frame = bounds
            }
        default: // Fit — contain, letterboxed
            playerLayer.videoGravity = .resizeAspect
            playerLayer.frame = bounds
        }
    }

    func setMuted(_ muted: Bool) {
        player.isMuted = muted
    }

    func pause() {
        player.pause()
    }

    /// Resume — or replay when parked at EOS (seek to zero first, then play).
    func resume() {
        if ended {
            ended = false
            player.seek(to: .zero)
        }
        player.play()
    }

    /// Tear down fully: pause, drop every observer, detach the layer. Idempotent.
    func stop() {
        player.pause()
        readyObs?.invalidate()
        readyObs = nil
        statusObs?.invalidate()
        statusObs = nil
        timeControlObs?.invalidate()
        timeControlObs = nil
        if let timeObserver {
            player.removeTimeObserver(timeObserver)
            self.timeObserver = nil
        }
        if let endObs {
            NotificationCenter.default.removeObserver(endObs)
            self.endObs = nil
        }
        canvas?.detachVideoLayer()
        pbTrace("native video \(sessionId): stopped + torn down")
    }

    deinit {
        // Safety net if `stop()` wasn't called; observer removal is thread-safe.
        readyObs?.invalidate()
        statusObs?.invalidate()
        timeControlObs?.invalidate()
        if let timeObserver {
            player.removeTimeObserver(timeObserver)
        }
        if let endObs {
            NotificationCenter.default.removeObserver(endObs)
        }
    }
}

/// Map `AVPlayer.timeControlStatus` to the core's native-video state code
/// (1 Buffering / 2 Playing / 3 Paused — matches `AppCore::native_video_state_changed`).
private func stateCode(_ player: AVPlayer) -> UInt8 {
    switch player.timeControlStatus {
    case .playing: return 2
    case .waitingToPlayAtSpecifiedRate: return 1
    case .paused: return 3
    @unknown default: return 0
    }
}
