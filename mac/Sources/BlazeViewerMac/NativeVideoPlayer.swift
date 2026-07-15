import AVFoundation
import AppKit
import PbMacFfi

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
    /// Retained for an archive clip: the `AVAssetResourceLoader` only weakly holds its
    /// delegate, so the player must keep the bytes-serving loader alive for the session.
    private let resourceLoader: ArchiveVideoLoader?
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
    /// Session-only resume position (task #94.2): seek here before the first reveal so
    /// returning to a video lands where you left off, with no frame-0 flash (the poster
    /// holds until the seek lands). `0` = play from the start.
    private let startSecs: Double
    /// The core's scale mode (0 Fit / 1 Fill / 2 Original), mirrored onto the layer so
    /// video honors 8/9/0 like a still. Zoom/pan/rotation parity is a later increment.
    private var scaleMode: UInt8

    /// Classify a playback failure for the FFmpeg-fallback decision (task #84
    /// §8a level 2): demux/codec-shaped failures are worth retrying through the
    /// FFmpeg session; missing-file / permission / network / DRM failures are
    /// not — no other backend can fix those, so the error should surface now.
    /// Unknown errors default to eligible (worst case: one extra failed open
    /// before the same error surfaces from the session).
    static func isFallbackEligible(_ error: Error?) -> Bool {
        guard let e = error as NSError? else { return true }
        switch e.domain {
        case NSURLErrorDomain:
            return false // network/file-URL resolution — not a codec problem
        case NSCocoaErrorDomain:
            return ![
                NSFileReadNoSuchFileError, NSFileNoSuchFileError, NSFileReadNoPermissionError,
            ].contains(e.code)
        case AVFoundationErrorDomain:
            return ![
                AVError.contentIsProtected.rawValue,
                AVError.contentIsNotAuthorized.rawValue,
                AVError.applicationIsNotAuthorized.rawValue,
            ].contains(e.code)
        case NSPOSIXErrorDomain:
            return ![Int(ENOENT), Int(EACCES), Int(EPERM)].contains(e.code)
        default:
            return true
        }
    }

    /// Play a loose file by URL.
    convenience init(
        url: URL, muted: Bool, sessionId: UInt64, scaleMode: UInt8, canvas: MetalCanvasNSView,
        model: CoreModel, startSecs: Double = 0
    ) {
        self.init(
            item: AVPlayerItem(url: url), loader: nil, muted: muted, sessionId: sessionId,
            scaleMode: scaleMode, canvas: canvas, model: model, startSecs: startSecs)
    }

    /// Play an archive (ZIP/7z) entry from in-RAM `data` — no file URL, so an
    /// `AVAssetResourceLoaderDelegate` serves the bytes to a custom-scheme `AVURLAsset`
    /// on demand (never written to disk; privacy #2). `name` gives the real extension so
    /// the loader can resolve the content type AVPlayer needs.
    convenience init(
        data: Data, name: String, muted: Bool, sessionId: UInt64, scaleMode: UInt8,
        canvas: MetalCanvasNSView, model: CoreModel, startSecs: Double = 0
    ) {
        let loader = ArchiveVideoLoader(data: data, name: name)
        let asset = AVURLAsset(url: loader.url)
        asset.resourceLoader.setDelegate(loader, queue: loader.queue)
        self.init(
            item: AVPlayerItem(asset: asset), loader: loader, muted: muted, sessionId: sessionId,
            scaleMode: scaleMode, canvas: canvas, model: model, startSecs: startSecs)
    }

    init(
        item: AVPlayerItem, loader: ArchiveVideoLoader?, muted: Bool, sessionId: UInt64,
        scaleMode: UInt8, canvas: MetalCanvasNSView, model: CoreModel, startSecs: Double = 0
    ) {
        self.sessionId = sessionId
        self.scaleMode = scaleMode
        self.canvas = canvas
        self.model = model
        self.resourceLoader = loader
        self.item = item
        self.startSecs = startSecs
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
                // Resume position (task #94.2): seek FIRST, reveal + play on the seek's
                // completion so the layer shows the resume frame, not frame 0 — the wgpu
                // poster holds until then, so there's no jump. `<= 0.5` plays from the start.
                if self.startSecs > 0.5 {
                    self.player.seek(
                        to: CMTime(seconds: self.startSecs, preferredTimescale: 600),
                        toleranceBefore: CMTime(seconds: 0.25, preferredTimescale: 600),
                        toleranceAfter: CMTime(seconds: 0.25, preferredTimescale: 600)
                    ) { [weak self] _ in
                        MainActor.assumeIsolated {
                            guard let self else { return }
                            // `self.playerLayer`, not the `layer` the KVO handed us — they
                            // are the same object, but `seek`'s completion is @Sendable and
                            // escaping, so capturing a bare AVPlayerLayer there crosses a
                            // concurrency boundary with a non-Sendable type (a hard error
                            // in Swift 6). Reaching it through `self` keeps the crossing to
                            // the one thing that is already isolated.
                            self.revealAndPlay(self.playerLayer)
                        }
                    }
                } else {
                    self.revealAndPlay(layer)
                }
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
                    self.model?.nativeVideoFailed(
                        self.sessionId, error: msg,
                        recoverable: Self.isFallbackEligible(item.error))
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
        let src = resourceLoader == nil ? "file" : "archive-bytes"
        pbTrace("native video \(sessionId): opening (\(src)) muted=\(muted)")
    }

    /// Unhide the layer (poster → video, no black flash) and start playback. Called
    /// once, from the first-frame reveal — directly, or after the resume seek lands.
    private func revealAndPlay(_ layer: AVPlayerLayer) {
        layer.isHidden = false
        canvas?.revealVideoLayer()
        player.play()
        pbTrace("native video \(sessionId): revealed + playing")
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

    /// Seek by a signed millisecond delta from the current time — the ±2 s / Shift ±10 s
    /// arrow-seek (and hold-to-scrub). Clamps to `[0, duration]`. Reports completion tagged
    /// with the core's `generation` so a superseded seek (a newer press cancels this one:
    /// `finished == false`) is told apart from a clean landing, keeping the proxy's
    /// in-flight bookkeeping honest. A small tolerance keeps rapid held seeks responsive.
    func seek(byMilliseconds deltaMs: Int64, generation: UInt64) {
        let dur = item.duration
        guard dur.isNumeric else { return }
        let total = CMTimeGetSeconds(dur)
        ended = false
        let target = max(
            0.0, min(total, CMTimeGetSeconds(player.currentTime()) + Double(deltaMs) / 1000.0))
        player.seek(
            to: CMTime(seconds: target, preferredTimescale: 600),
            toleranceBefore: CMTime(seconds: 0.1, preferredTimescale: 600),
            toleranceAfter: CMTime(seconds: 0.1, preferredTimescale: 600)
        ) { [weak self] finished in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.model?.nativeVideoSeekCompleted(
                    self.sessionId, generation: generation, finished: finished)
            }
        }
        publishProgress()
    }

    /// Frame-step one frame (`,`/`.`). Stepping is scrubbing, not playback: pause first (the
    /// `AVPlayerItem` steps only while paused), then no-op when the item can't step that way.
    func step(forward: Bool) {
        if player.timeControlStatus != .paused { player.pause() }
        ended = false
        guard forward ? item.canStepForward : item.canStepBackward else { return }
        item.step(byCount: forward ? 1 : -1)
        publishProgress()
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
        reportActiveAudioTrack() // AVFoundation has made its automatic choice by now (#99)
    }

    // MARK: - Audio track selection (task #99)

    /// The asset's audible media-selection group — AVFoundation's own model of "the audio
    /// tracks you may choose between", and the only handle `select(_:in:)` accepts.
    private var audibleGroup: AVMediaSelectionGroup? {
        item.asset.mediaSelectionGroup(forMediaCharacteristic: .audible)
    }

    /// An option's identity as bytes, in the **same binary-plist encoding** the catalog
    /// stored (`option_identity` in pb-decode). Comparing these is how a row is matched to a
    /// live option without trusting an ordinal — the phase-0 spike proved the round-trip.
    ///
    /// ⚠ **`propertyList()` is a METHOD, not a property.** Writing `option.propertyList`
    /// compiles happily and hands `PropertyListSerialization` a *function reference*, which
    /// it rejects ("property lists cannot contain objects of type 'CFType'") — and `try?`
    /// swallows that, so every identity silently came back `nil`. Nil compares equal to nil,
    /// so the failure even looked like a match. That killed the whole AVPlayer path: no
    /// locator resolved, no MP4 track could be selected, and no row could tick. Measured
    /// against a real two-track MP4: `propertyList` → serialize error; `propertyList()` →
    /// 208 bytes that round-trip back to an `isEqual:` option.
    private func identity(of option: AVMediaSelectionOption) -> Data? {
        do {
            return try PropertyListSerialization.data(
                fromPropertyList: option.propertyList(), format: .binary, options: 0)
        } catch {
            // Never silently: a nil identity disables audio selection on this whole route,
            // and nil-compares-equal-to-nil made the last failure look like a match. If this
            // ever fires, the locator contract with pb-decode's `option_identity` has broken.
            pbTrace("audio: option identity failed to serialize (#99): \(error)")
            return nil
        }
    }

    /// Which picker row AVFoundation is **actually playing** (`-1` = unknown), by matching
    /// its current selection against the rows' stored property lists.
    ///
    /// Synchronous, unlike the sample-buffer route's decoder: `currentMediaSelection` is
    /// readable on the spot, so this can be asked at menu-open — which is exactly when it
    /// must be asked. It reports what the player *selected*, including AVFoundation's own
    /// automatic choice, rather than predicting it.
    ///
    /// Requires the picker rows to be built already (it compares against them), so callers
    /// must refresh first — `CoreModel.audioTrackRows()` owns that order.
    func currentAudioRow() -> Int {
        guard let model, let group = audibleGroup,
            let current = item.currentMediaSelection.selectedMediaOption(in: group),
            let want = identity(of: current)
        else {
            return -1
        }
        return model.audioRowMatching(plist: want)
    }

    /// Push the current selection to the core (used right after a switch, where the tick
    /// must move without waiting for the next menu open).
    func reportActiveAudioTrack() {
        model?.reportActiveAudioRow(currentAudioRow())
    }

    /// Switch to the audio track at picker row `row`.
    ///
    /// Cheap next to the sample-buffer route: `select(_:in:)` swaps the track on a playing
    /// item with no reader rebuild, no format to re-describe, and no re-seek — AVPlayer
    /// re-primes internally and owns the clock throughout.
    func selectAudioTrack(row: Int) {
        guard let model else { return }
        guard let group = audibleGroup,
            let plist = model.audioRowAvPlist(row),
            let obj = try? PropertyListSerialization.propertyList(
                from: plist, options: [], format: nil),
            let option = group.mediaSelectionOption(withPropertyList: obj)
        else {
            model.audioTrackSwitched(row: row, ok: false)
            return
        }
        item.select(option, in: group)
        // Confirm from the player rather than assuming the select took: re-read the current
        // selection and report THAT. A switch that silently didn't happen must not toast.
        reportActiveAudioTrack()
        let landed = item.currentMediaSelection.selectedMediaOption(in: group)
        let ok = landed.flatMap(identity(of:)) == plist
        model.audioTrackSwitched(row: row, ok: ok)
    }

    /// Mirror the core's scale mode (8/9/0) onto the layer. Re-lays-out on a real change.
    func setScaleMode(_ mode: UInt8) {
        guard mode != scaleMode else { return }
        scaleMode = mode
        relayout()
    }

    /// The last placement applied, to skip redundant CALayer writes when the pump ticks
    /// with no view change (steady playback). Raw px + rotation + scale + canvas size.
    private struct PlacementKey: Equatable {
        let x, y, w, h: Float
        let rot: UInt8
        let scale, cw, ch: CGFloat
    }
    private var lastPlacement: PlacementKey?

    /// Place the `AVPlayerLayer` to match the core's still geometry — Fit/Fill/Original,
    /// zoom, pan, and rotation, all in parity with a photo (task 79.9 phase 3). Pulls the
    /// core's computed placement (physical px, top-left) and converts it to the layer's
    /// point / bottom-left-origin space, applying rotation as a center transform. Before the
    /// renderer/fit exist (pre-first-frame) it falls back to a scale-mode-only placement.
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

    /// Apply the core's placement. `x/y/w/h` are physical px, top-left; `w/h` are the
    /// *rotated* footprint. We size the layer to the *unrotated* displayed size (so the
    /// video fills it at its true aspect with `.resize`), center it on the footprint, and
    /// rotate about that center. The core owns fit/zoom/pan, so the result matches the still.
    private func applyPlacement(_ p: VideoPlacementFfi, bounds: CGRect, scale: CGFloat) {
        let key = PlacementKey(
            x: p.x, y: p.y, w: p.w, h: p.h, rot: p.rotation,
            scale: scale, cw: bounds.width, ch: bounds.height)
        if lastPlacement == key { return } // unchanged — skip the CALayer writes
        lastPlacement = key

        let footW = CGFloat(p.w) / scale
        let footH = CGFloat(p.h) / scale
        // Footprint center: core top-left px → layer point, bottom-left origin (y-flip).
        let centerX = (CGFloat(p.x) + CGFloat(p.w) / 2) / scale
        let centerY = bounds.height - (CGFloat(p.y) + CGFloat(p.h) / 2) / scale
        // The footprint is the rotated size; un-swap for 90°/270° to get the layer's own
        // (unrotated) bounds, which then carry the video's native aspect.
        let swaps = p.rotation == 1 || p.rotation == 3
        let bw = swaps ? footH : footW
        let bh = swaps ? footW : footH
        // CW quadrants. The layer's geometry is y-up (bottom-left), where a positive
        // z-rotation is counter-clockwise, so negate to rotate clockwise like the still.
        let angle = -CGFloat(p.rotation) * .pi / 2

        CATransaction.begin()
        CATransaction.setDisableActions(true)
        defer { CATransaction.commit() }
        playerLayer.contentsScale = scale
        playerLayer.videoGravity = .resize // bounds already carry the displayed aspect
        playerLayer.anchorPoint = CGPoint(x: 0.5, y: 0.5)
        playerLayer.bounds = CGRect(x: 0, y: 0, width: bw, height: bh)
        playerLayer.position = CGPoint(x: centerX, y: centerY)
        playerLayer.transform = CATransform3DMakeRotation(angle, 0, 0, 1)
    }

    /// Pre-first-frame fallback (no renderer/fit yet): scale-mode-only placement, no zoom/
    /// pan/rotation. Resets any prior rotation transform so `frame` is well-defined.
    private func relayoutScaleMode(bounds: CGRect, scale: CGFloat) {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        defer { CATransaction.commit() }
        playerLayer.transform = CATransform3DIdentity
        playerLayer.anchorPoint = CGPoint(x: 0.5, y: 0.5)
        playerLayer.position = CGPoint(x: bounds.midX, y: bounds.midY)
        playerLayer.bounds = CGRect(x: 0, y: 0, width: bounds.width, height: bounds.height)
        playerLayer.contentsScale = scale
        switch scaleMode {
        case 1: playerLayer.videoGravity = .resizeAspectFill // Fill
        case 2: playerLayer.videoGravity = .resizeAspect // Original (pre-size — refined once placed)
        default: playerLayer.videoGravity = .resizeAspect // Fit
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
