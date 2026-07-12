import AVFoundation
import AppKit

/// The native macOS video player (task 79.9). On macOS the whole media pipeline is
/// `AVPlayer` + `AVPlayerLayer` — system decode, color, HDR, audio, timing, buffering,
/// and seeking — the single media authority (the Rust core keeps only a passive proxy
/// and commands this via `PlayVideo`/`StopVideo`). See
/// `.taskmaster/plans/79.9-macos-video-playback.md`.
///
/// This is the Phase-0A proof surface. It:
/// - attaches its `AVPlayerLayer` as a sublayer of the Metal canvas (hidden),
/// - reveals the layer on the **first displayable frame** and only then starts
///   playback (so audio never leads the picture, and the wgpu poster shows until the
///   frame is ready — no black/stale flash),
/// - parks the true last frame at end-of-stream (never re-shows the poster), and
/// - tears down fully — player paused, every observer removed, layer detached — on
///   `stop()`, so navigating away leaves nothing retained.
@MainActor
final class NativeVideoPlayer {
    /// Identity from the core; a `StopVideo` for a different session must not tear
    /// this one down (`CoreModel` checks it before calling `stop()`).
    let sessionId: UInt64

    private let player: AVPlayer
    private let item: AVPlayerItem
    private let playerLayer: AVPlayerLayer
    private weak var canvas: MetalCanvasNSView?

    private var readyObs: NSKeyValueObservation?
    private var statusObs: NSKeyValueObservation?
    private var endObs: NSObjectProtocol?
    private var revealed = false

    init(url: URL, muted: Bool, sessionId: UInt64, canvas: MetalCanvasNSView) {
        self.sessionId = sessionId
        self.canvas = canvas
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
        // Surface a decode/open failure (missing codec, unreadable file). The core
        // callback that returns to the poster + shows one error lands in 79.9 phase 2;
        // for the spike this traces so the failure path is observable.
        statusObs = item.observe(\.status, options: [.new]) { [weak self] item, _ in
            MainActor.assumeIsolated {
                guard let self, item.status == .failed else { return }
                pbTrace(
                    "native video \(self.sessionId): FAILED "
                        + (item.error?.localizedDescription ?? "unknown"))
            }
        }
        endObs = NotificationCenter.default.addObserver(
            forName: .AVPlayerItemDidPlayToEndTime, object: item, queue: .main
        ) { [weak self] _ in
            guard let self else { return }
            pbTrace("native video \(self.sessionId): EOS — parking last frame")
        }
        pbTrace("native video \(sessionId): opening \(url.lastPathComponent) muted=\(muted)")
    }

    func setMuted(_ muted: Bool) {
        player.isMuted = muted
    }

    /// Tear down fully: pause, drop every observer, detach the layer. Idempotent —
    /// safe to call on stop, replacement, or teardown.
    func stop() {
        player.pause()
        readyObs?.invalidate()
        readyObs = nil
        statusObs?.invalidate()
        statusObs = nil
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
        if let endObs {
            NotificationCenter.default.removeObserver(endObs)
        }
    }
}
