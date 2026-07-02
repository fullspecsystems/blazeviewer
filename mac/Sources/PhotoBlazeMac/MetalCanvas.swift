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
    private var attached = false

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not from a nib") }

    override func makeBackingLayer() -> CALayer {
        CAMetalLayer()
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
