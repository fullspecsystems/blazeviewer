import AppKit
import QuartzCore
import SwiftUI

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

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        // Finder file drops onto the photo canvas (the winit shell's DroppedPaths).
        registerForDraggedTypes([.fileURL])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not from a nib") }

    override func makeBackingLayer() -> CALayer {
        CAMetalLayer()
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

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        guard window != nil, !attached, let metalLayer = layer as? CAMetalLayer else { return }
        let scale = backingScale
        metalLayer.contentsScale = scale
        attached = true
        onAttach?(metalLayer, pixelSize(at: scale), scale)
    }

    override func layout() {
        super.layout()
        guard attached else { return }
        let scale = backingScale
        layer?.contentsScale = scale
        onResize?(pixelSize(at: scale), scale)
    }

    /// Called by the representable's dismantle: drop the Rust renderer BEFORE this view
    /// (and its layer) dies — the FFI layer-lifetime contract.
    func detachNow() {
        guard attached else { return }
        attached = false
        onDetach?()
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

/// SwiftUI wrapper for the canvas view.
struct MetalCanvas: NSViewRepresentable {
    let model: CoreModel

    func makeNSView(context: Context) -> MetalCanvasNSView {
        let view = MetalCanvasNSView(frame: .zero)
        view.model = model
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
