import AppKit
import AVFoundation
import QuartzCore
import SwiftUI

/// `PB_TRACE=1` → size/present diagnostics to stderr (dev-only; zero-ish cost when
/// off). stderr rather than NSLog: it's visible when launching the executable
/// directly (`.app/Contents/MacOS/PhotoBlaze 2> trace.log`), and cheap enough not
/// to perturb the resize-race timing it exists to observe — NSLog's latency masked
/// exactly the race we were chasing (2026-07-04).
let pbTraceEnabled = ProcessInfo.processInfo.environment["PB_TRACE"] != nil

@inline(__always)
func pbTrace(_ message: @autoclosure () -> String) {
    guard pbTraceEnabled else { return }
    let t = String(format: "%.3f", ProcessInfo.processInfo.systemUptime)
    FileHandle.standardError.write(Data("PB[\(t)] \(message())\n".utf8))
}

/// The wgpu canvas: a plain layer-hosting NSView whose backing layer is a `CAMetalLayer`
/// the Rust renderer draws into. Deliberately **not** `MTKView` — wgpu owns the drawable
/// loop (`nextDrawable`/present), and MTKView's own drawable management would fight it.
/// The view reports attach / pixel-size changes / teardown to the model, which forwards
/// them over the FFI (all on the main actor — the layer contract).
final class MetalCanvasNSView: NSView {
    var onAttach: ((CAMetalLayer, CGSize, CGFloat) -> Void)?
    var onResize: ((CGSize, CGFloat) -> Void)?
    var onDetach: (() -> Void)?
    /// The input adapter (NS1 item 4): pointer + gestures + drops forward to the model.
    weak var model: CoreModel?
    private var attached = false

    /// The native video presentation (task 79.9): an **opaque black container** layer that
    /// always covers the canvas, holding the `AVPlayerLayer` (placed within it per the scale
    /// mode). Opaque so it fully hides the wgpu canvas while a video shows — no letterbox/
    /// Original-surround bleed-through of a stale poster ("ghost"), and a background video can
    /// never show through a foreground one. SwiftUI overlays still composite above it. There is
    /// exactly one; a new attach nukes any prior/orphaned video layers.
    private(set) var videoContainer: CALayer?
    /// The identifying `name` on the container, so a stray orphan can always be found + removed.
    private static let videoLayerName = "pb.video.container"

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        // Never stretch stale contents on a resize. For a layer-BACKED view AppKit owns
        // the backing layer's `contentsGravity`: it derives it from these two view
        // properties and re-asserts it on frame changes — so setting gravity on the
        // CAMetalLayer alone gets clobbered back to the stretching default
        // (`.scaleAxesIndependently`) at exactly the moment it matters. `.never`
        // because wgpu pushes frames itself (AppKit must not mark the layer for
        // redraw); `.center` so a not-yet-refreshed frame composites unscaled —
        // centered content holds its size for the one racy composite of a window
        // resize (wgpu presents out-of-band from the CA transaction that moves the
        // bounds) instead of ballooning/squeezing until the next render lands.
        layerContentsRedrawPolicy = .never
        layerContentsPlacement = .center
        // Own the window's keyboard focus (see `acceptsFirstResponder`). `.none` so that even
        // when this view IS first responder it paints no ring — the content is the focus, not a
        // control.
        focusRingType = .none
        // Finder file drops onto the photo canvas (the winit shell's DroppedPaths).
        registerForDraggedTypes([.fileURL])
        // The definitive size signal. SwiftUI sizes this representable on its own
        // deferred update cycle — after `layoutSubtreeIfNeeded` returns, after any
        // post-transition runloop hop — and a window-mode change doesn't reliably
        // re-fire `layout()` at all. The frame pump reconciles per tick, but it is
        // *paused when idle*, so a fullscreen toggle on a static screen could strand
        // the surface at the old size until the next input kicked the pump. This
        // notification posts synchronously whenever the view's frame is actually
        // set, whoever sets it and whenever that lands — the event the callbacks
        // above all only approximate.
        postsFrameChangedNotifications = true
        frameObserver = NotificationCenter.default.addObserver(
            forName: NSView.frameDidChangeNotification, object: self, queue: nil
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.reconcileSizeIfNeeded() }
        }
    }

    /// The frame-did-change observation token; removed on detach.
    private var frameObserver: NSObjectProtocol?

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not from a nib") }

    /// Accept first responder so the **content** owns the window's keyboard focus. Without
    /// this the canvas refuses focus, and when the window becomes key AppKit hands
    /// first-responder to the first toolbar control instead — which paints a blue focus ring
    /// that can't be Tabbed away (the SwiftUI-hosted toolbar has no working key-view loop).
    /// Every other app focuses its content on open for the same reason. Safe because keys are
    /// delivered by `CoreModel`'s local event monitor (gated on the target window), NOT the
    /// responder chain — so first responder here is purely about where the (suppressed) focus
    /// ring sits. `CoreModel` re-asserts this on `didBecomeKey`.
    override var acceptsFirstResponder: Bool { true }

    override func makeBackingLayer() -> CALayer {
        let layer = CAMetalLayer()
        // Never stretch stale contents. When the window jumps size in one step (the F
        // fullscreen toggle), there is inevitably one composite where the layer already
        // has its new bounds but still shows the previous drawable: wgpu presents
        // out-of-band from the CoreAnimation transaction that moves the bounds
        // (`presentsWithTransaction` is off, and wgpu 22 exposes no safe way to enable
        // it), so no shell-side sequencing can make {new bounds, new frame} atomic.
        // The default `.resize` gravity ballooned/squeezed that stale frame; `.center`
        // composites it unscaled instead — the centered Open panel stays pixel-identical
        // and a photo briefly reveals letterbox at the edges — until the next pump frame
        // (≤1 refresh) lands the correctly sized render. (Verified: wgpu's configure()
        // never touches contentsGravity, so setting it once here holds.)
        layer.contentsGravity = .center
        return layer
    }

    // MARK: - Pointer + gestures → the core (winit conventions: physical px, top-left origin)

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        trackingAreas.forEach(removeTrackingArea)
        addTrackingArea(NSTrackingArea(
            rect: .zero,
            options: [.mouseMoved, .cursorUpdate, .activeInKeyWindow, .inVisibleRect],
            owner: self
        ))
    }

    /// Claim cursor ownership over the canvas: without this, the last AppKit cursor the
    /// pointer crossed (a window-resize edge, a SwiftUI divider) leaks over the photo until
    /// the core happens to emit a SetCursor transition.
    override func cursorUpdate(with event: NSEvent) {
        (model?.desiredCursor ?? .arrow).set()
    }

    /// AppKit's view coords are bottom-left-origin points; the core speaks top-left-origin
    /// physical px (the winit `CursorMoved` convention).
    private func corePoint(for event: NSEvent) -> (Float, Float) {
        let p = convert(event.locationInWindow, from: nil)
        let scale = backingScale
        return (Float(p.x * scale), Float((bounds.height - p.y) * scale))
    }

    override func mouseMoved(with event: NSEvent) {
        let (x, y) = corePoint(for: event)
        model?.pointerMoved(x: x, y: y)
    }

    override func mouseDragged(with event: NSEvent) {
        let (x, y) = corePoint(for: event)
        model?.pointerMoved(x: x, y: y)
    }

    override func mouseDown(with event: NSEvent) {
        // Track the press position first so control hit-tests see the click point.
        let (x, y) = corePoint(for: event)
        model?.pointerMoved(x: x, y: y)
        model?.mouseLeft(pressed: true)
    }

    override func mouseUp(with event: NSEvent) {
        model?.mouseLeft(pressed: false)
    }

    override func rightMouseDown(with event: NSEvent) {
        // Track the position first (the core builds the menu from live state at the cursor),
        // then let the model pop the menu the ShowContextMenu effect describes.
        let (x, y) = corePoint(for: event)
        model?.pointerMoved(x: x, y: y)
        model?.contextMenu(at: event, in: self)
    }

    override func scrollWheel(with event: NSEvent) {
        if event.hasPreciseScrollingDeltas {
            // Trackpad two-finger swipe: points → physical px (the winit PixelDelta unit).
            let scale = backingScale
            model?.scrollPixels(
                x: Float(event.scrollingDeltaX * scale),
                y: Float(event.scrollingDeltaY * scale)
            )
        } else {
            model?.scrollLines(x: Float(event.scrollingDeltaX), y: Float(event.scrollingDeltaY))
        }
    }

    override func magnify(with event: NSEvent) {
        model?.pinch(delta: Float(event.magnification))
    }

    override func smartMagnify(with event: NSEvent) {
        model?.doubleTap()
    }

    // MARK: - File drop → open

    override func draggingEntered(_ sender: NSDraggingInfo) -> NSDragOperation {
        sender.draggingPasteboard.canReadObject(
            forClasses: [NSURL.self],
            options: [.urlReadingFileURLsOnly: true]
        ) ? .copy : []
    }

    override func performDragOperation(_ sender: NSDraggingInfo) -> Bool {
        guard let urls = sender.draggingPasteboard.readObjects(
            forClasses: [NSURL.self],
            options: [.urlReadingFileURLsOnly: true]
        ) as? [URL], !urls.isEmpty else { return false }
        model?.openPaths(urls.map(\.path))
        return true
    }

    private var pump: FramePump?

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        guard window != nil, !attached, let metalLayer = layer as? CAMetalLayer else { return }
        let scale = backingScale
        metalLayer.contentsScale = scale
        attached = true
        model?.hostWindow = window
        // Take the window's keyboard focus immediately so no toolbar control becomes the
        // initial first responder and paints a stuck focus ring (see `acceptsFirstResponder`).
        window?.initialFirstResponder = self
        window?.makeFirstResponder(self)
        // Report the effective appearance BEFORE the renderer attaches, so the first
        // frame's letterbox + HUD already resolve `Appearance: System` correctly (#46).
        model?.osThemeChanged(dark: effectiveAppearance.isDarkAppearance)
        lastReported = (pixelSize(at: scale), scale) // attach reports this same geometry
        pbTrace("attach px=\(Int(lastReported.size.width))x\(Int(lastReported.size.height)) scale=\(scale)")
        onAttach?(metalLayer, pixelSize(at: scale), scale)
        // Stand the display-synchronized frame pump up on this view (it tracks the view's
        // display for the refresh rate); the model paces it, we own its lifetime.
        if let model {
            let pump = FramePump(view: self, model: model)
            self.pump = pump
            model.framePump = pump
            // A launch-opened folder/archive (`--pb-open`, a cold Finder open) resolved
            // BEFORE this pump existed — tick once now so its scan worker gets polled
            // immediately (and `updatePacing` takes over), rather than waiting for the
            // display link's first fire.
            model.pump()
        }
    }

    /// The last geometry actually reported to the model — the baseline
    /// `reconcileSizeIfNeeded` compares against.
    private var lastReported: (size: CGSize, scale: CGFloat) = (.zero, 0)

    /// The single funnel every size/scale report goes through, so `lastReported`
    /// always matches what the core was told.
    private func report() {
        let scale = backingScale
        layer?.contentsScale = scale
        let size = pixelSize(at: scale)
        lastReported = (size, scale)
        pbTrace("report px=\(Int(size.width))x\(Int(size.height)) scale=\(scale) bounds=\(Int(bounds.width))x\(Int(bounds.height)) onResize=\(onResize != nil)")
        onResize?(size, scale)
        syncVideoLayerFrame()
    }

    override func layout() {
        super.layout()
        // Reconcile (not an unconditional report): the frame-did-change notification
        // usually fired for this same change already, and re-reporting an unchanged
        // size would re-render a full frame per live-resize step for nothing.
        reconcileSizeIfNeeded()
        syncVideoLayerFrame()
    }

    /// The backing scale changed with no bounds change — a bare scale flip does NOT
    /// re-run `layout()`, so this is the only hook that catches it. Two real cases:
    /// the launch race where attach reads the window's scale before it settles on its
    /// final screen (the owner-reported blurry / unclickable Open buttons, stuck until
    /// a manual resize forced layout), and dragging the window between 1x and 2x
    /// displays. Re-reporting through the standard resize path re-sizes the surface,
    /// re-rasterizes the overlays, and re-aligns the hit-test coordinates.
    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        // Reconcile: the scale compare catches a bare scale flip; a same-geometry
        // re-fire (AppKit posts this liberally) stays a no-op.
        reconcileSizeIfNeeded()
    }

    /// Re-report the current pixel size through the standard resize path, without waiting
    /// for AppKit to re-fire `layout()`. The fullscreen↔windowed transition
    /// (`setWindowMode`) changes the window frame but doesn't reliably drive a fresh
    /// `layout()` → `onResize` with the settled size; the transition path calls this
    /// once AppKit has applied the new frame.
    func reportSizeNow() {
        guard attached else { return }
        report()
    }

    /// The pull half of size tracking: compare the live geometry against what was last
    /// reported and re-report on drift. Called at the top of every pump tick (and once
    /// after a window-mode transition), so a missed or mistimed AppKit layout callback
    /// can strand the surface at a stale size/scale for at most one frame — the invariant
    /// that ends the "stuck at the wrong size" class of bug (fullscreen toggles, 1x↔2x
    /// display moves) rather than patching its instances callback by callback.
    func reconcileSizeIfNeeded() {
        guard attached else { return }
        let scale = backingScale
        if pixelSize(at: scale) != lastReported.size || scale != lastReported.scale {
            report()
        }
    }

    /// The effective light/dark appearance changed — an OS theme switch, or our own
    /// `NSApp.appearance` override landing (the Theme preference, #46). Report it so
    /// `Appearance: System` re-resolves the HUD + letterbox live, without a restart.
    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        guard attached else { return }
        model?.osThemeChanged(dark: effectiveAppearance.isDarkAppearance)
    }

    /// Called by the representable's dismantle: drop the Rust renderer BEFORE this view
    /// (and its layer) dies — the FFI layer-lifetime contract. The pump goes first (it
    /// must not fire into a detached core).
    func detachNow() {
        guard attached else { return }
        attached = false
        if let frameObserver {
            NotificationCenter.default.removeObserver(frameObserver)
            self.frameObserver = nil
        }
        pump?.invalidate()
        pump = nil
        model?.framePump = nil
        detachVideoLayer()
        onDetach?()
    }

    // MARK: - Native video layer (task 79.9 Phase-0A spike)

    /// Attach an `AVPlayerLayer` inside a fresh opaque black container over the Metal layer,
    /// hidden until the caller reveals it on the first displayable frame (so the wgpu poster
    /// shows until then — no black/stale flash). Nukes any prior/orphaned video layers first,
    /// so there is never a second (background) video showing through.
    func attachVideoLayer(_ playerLayer: AVPlayerLayer) {
        playerLayer.videoGravity = .resizeAspect
        attachVideoSublayer(playerLayer)
    }

    /// Generalized attach for the Phase 3 sample-buffer presenter: hosts ANY
    /// `CALayer`-backed video presentation (an `AVPlayerLayer` or an
    /// `AVSampleBufferDisplayLayer`) inside the same opaque letterbox container,
    /// hidden until the caller reveals it on the first displayable frame. The
    /// caller owns its own `videoGravity` (both layer types have it); everything
    /// else — container, letterbox color, clipping, hidden-until-reveal — is shared.
    func attachVideoSublayer(_ videoLayer: CALayer) {
        detachVideoLayer()
        let container = CALayer()
        container.name = Self.videoLayerName
        // The user's theme-aware letterbox color (what photos letterbox with) — falls back
        // to black only if the model isn't wired. Opaque so it hides the wgpu canvas.
        container.backgroundColor = model?.videoLetterboxCGColor ?? NSColor.black.cgColor
        container.isOpaque = true
        // Clip the player layer to the canvas: a zoomed / Fill / rotated video overflows its
        // footprint, and the overflow must be cropped to the window like a still (not spill
        // over the chrome). The container fills the canvas, so this bounds the video to it.
        //
        // ⚠ A/B (PB_NO_MASK): a clipping ancestor disqualifies the AVSampleBufferDisplayLayer
        // from a hardware **overlay plane** (direct scanout), forcing GPU compositing — the
        // suspected cause of the ~3 dropped frames/sec on a high-refresh display. Forcing this
        // off tests that (visually correct ONLY for a Fit, unrotated, unzoomed clip — nothing
        // overflows to clip). If it lands the fix, the shipping version makes `masksToBounds`
        // conditional on whether the video actually overflows.
        container.masksToBounds = ProcessInfo.processInfo.environment["PB_NO_MASK"] == nil
        // Hide the *whole container* (not just the inner player) until the first frame is
        // displayable, so the opaque fill doesn't cover the wgpu poster before there's a real
        // video frame to show — that gap is the "blackout" flash. `revealVideoLayer()` unhides
        // it (called from the player's isReadyForDisplay callback), swapping poster→video in
        // one step with nothing solid in between.
        container.isHidden = true
        videoLayer.isHidden = true
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        container.frame = layer?.bounds ?? bounds
        container.contentsScale = backingScale
        videoLayer.frame = container.bounds
        videoLayer.contentsScale = backingScale
        container.addSublayer(videoLayer)
        layer?.addSublayer(container)
        CATransaction.commit()
        videoContainer = container
    }

    /// Reveal the video container once the player has its first displayable frame (called from
    /// `NativeVideoPlayer`'s `isReadyForDisplay` observer). Until this fires the wgpu poster
    /// shows through the hidden container — this is the poster→video swap with no black flash.
    func revealVideoLayer() {
        videoContainer?.isHidden = false
    }

    /// Update the video container's background to the current letterbox color (theme switch /
    /// Settings edit). No-op when no video is showing.
    func setVideoLetterbox(_ color: CGColor) {
        videoContainer?.backgroundColor = color
    }

    /// Remove the video container (and its player layer). Idempotent, and sweeps any orphan
    /// container left by a mistimed teardown so two videos can never coexist.
    func detachVideoLayer() {
        videoContainer?.removeFromSuperlayer()
        videoContainer = nil
        for sub in layer?.sublayers ?? [] where sub.name == Self.videoLayerName {
            sub.removeFromSuperlayer()
        }
    }

    /// Keep the video presentation placed correctly across live resize, fullscreen
    /// transitions, and 1x↔2x display moves. The container fills the canvas; the player owns
    /// its own frame within it (scale-mode + presentation-size dependent).
    private func syncVideoLayerFrame() {
        guard let container = videoContainer else { return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        container.frame = layer?.bounds ?? bounds
        container.contentsScale = backingScale
        CATransaction.commit()
        model?.relayoutNativeVideo()
    }

    private var backingScale: CGFloat {
        window?.backingScaleFactor ?? 2.0
    }

    private func pixelSize(at scale: CGFloat) -> CGSize {
        CGSize(
            width: max(1, bounds.width * scale).rounded(),
            height: max(1, bounds.height * scale).rounded()
        )
    }
}

extension NSAppearance {
    /// Whether this appearance resolves dark (the core's `os_dark` currency, #46).
    var isDarkAppearance: Bool {
        bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
    }
}

/// SwiftUI wrapper for the canvas view.
struct MetalCanvas: NSViewRepresentable {
    let model: CoreModel

    func makeNSView(context: Context) -> MetalCanvasNSView {
        let view = MetalCanvasNSView(frame: .zero)
        view.model = model
        // The model re-reports the canvas size through this view after a fullscreen→windowed
        // restore (see `setWindowMode`), where AppKit doesn't reliably re-fire `layout()`.
        model.canvasView = view
        view.onAttach = { layer, size, scale in
            model.attachCanvas(layer: layer, pixelSize: size, scale: scale)
        }
        view.onResize = { size, scale in
            model.canvasResized(pixelSize: size, scale: scale)
        }
        view.onDetach = {
            model.detachCanvas()
        }
        return view
    }

    func updateNSView(_ view: MetalCanvasNSView, context: Context) {}

    static func dismantleNSView(_ view: MetalCanvasNSView, coordinator: ()) {
        view.detachNow()
    }
}
