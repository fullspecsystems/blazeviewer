import AppKit
import QuartzCore

/// The frame pump (NS1 item 7): a `CADisplayLink` synchronized to the display the canvas
/// view is on (via `NSView.displayLink`, macOS 14+), driving `CoreModel.pump()` — one
/// tick + effect drain per refresh — while the engine has work, and **paused when idle**
/// (on-demand rendering; the core's `SetWake` deadlines schedule precise off-link wakes).
///
/// A small NSObject wrapper because CADisplayLink wants a target/selector; the link
/// retains its target, so `invalidate()` (called by the canvas view on detach) breaks
/// the cycle.
final class FramePump: NSObject {
    private var link: CADisplayLink?
    private weak var model: CoreModel?

    @MainActor
    init(view: NSView, model: CoreModel) {
        self.model = model
        super.init()
        let link = view.displayLink(target: self, selector: #selector(fire(_:)))
        link.add(to: .main, forMode: .common)
        self.link = link
    }

    @objc private func fire(_ link: CADisplayLink) {
        MainActor.assumeIsolated {
            guard let model else { return }
            guard pbTraceEnabled else {
                model.pump()
                return
            }
            let t0 = DispatchTime.now().uptimeNanoseconds
            model.pump()
            model.recordPumpTick(DispatchTime.now().uptimeNanoseconds &- t0)
        }
    }

    /// Pause = idle (no per-frame ticks); resumed by input or a SetWake deadline.
    var paused: Bool {
        get { link?.isPaused ?? true }
        set { link?.isPaused = newValue }
    }

    func invalidate() {
        link?.invalidate()
        link = nil
    }
}
