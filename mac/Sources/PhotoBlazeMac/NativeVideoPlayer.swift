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
    private var revealed = false
    private var reportedOpened = false
    private var ended = false

    init(url: URL, muted: Bool, sessionId: UInt64, canvas: MetalCanvasNSView, model: CoreModel) {
        self.sessionId = sessionId
        self.canvas = canvas
        self.model = model
        item = AVPlayerItem(url: url)
        player = AVPlayer(playerItem: item)
        player.isMuted = muted
        // Hold the last frame at EOS (parity: end-of-stream parks; `P` replays).
        player.actionAtItemEnd = .pause
        playerLayer = AVPlayerLayer(player: player)
        canvas.attachVideoLayer(playerLayer) // hidden until the first frame

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
            guard let self else { return }
            self.ended = true
            pbTrace("native video \(self.sessionId): EOS — parking last frame")
            self.model?.nativeVideoEnded(self.sessionId)
        }
        pbTrace("native video \(sessionId): opening \(url.lastPathComponent) muted=\(muted)")
    }

    /// Tell the core the clip opened (duration + audio presence), once.
    private func reportOpened() {
        guard !reportedOpened else { return }
        reportedOpened = true
        let d = item.duration
        let durationMs: Int64 = d.isNumeric ? Int64((CMTimeGetSeconds(d) * 1000).rounded()) : -1
        let hasAudio = item.tracks.contains { $0.assetTrack?.mediaType == .audio }
        model?.nativeVideoOpened(sessionId, durationMs: durationMs, hasAudio: hasAudio)
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
