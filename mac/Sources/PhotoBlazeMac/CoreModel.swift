import AppKit
import Observation
import PbMacFfi
import UniformTypeIdentifiers

/// One drained-effect line in the on-screen log (id is a monotonic counter so the
/// capped log never reuses SwiftUI row identities).
struct EffectLine: Identifiable {
    let id: Int
    let text: String
}

/// The Swift-side owner of the Rust engine — the NS1 slice-1 host model.
///
/// Owns the opaque `AppCoreHandle` (the whole `AppCore` lives behind it), forwards input
/// events in, and pulls the effect queue dry on the main actor after every event/tick —
/// the FFI main-thread rule: a worker thread may only *schedule* a drain, never run one.
@MainActor
@Observable
final class CoreModel {
    /// The Rust engine. All calls happen on the main actor.
    private let core: AppCoreHandle
    /// Drained effects, oldest first, capped — the visible proof of the FFI round trip.
    private(set) var effectLog: [EffectLine] = []
    private var nextLineId = 0

    @ObservationIgnored private var keyMonitor: Any?
    @ObservationIgnored private var focusObserver: NSObjectProtocol?
    @ObservationIgnored private var keyLossObserver: NSObjectProtocol?
    /// Slice-1 frame pump: a coarse fixed timer driving `tick()`. Replaced by the real
    /// MTKView-driven pump honoring SetWake in NS1 item 7.
    @ObservationIgnored private var tickTimer: Timer?

    init() {
        // Headless viewport at the main screen's pixel size (the live CAMetalLayer surface
        // is NS1 item 2; real construction over a photo source is item 3).
        let screen = NSScreen.main
        let scale = screen?.backingScaleFactor ?? 2.0
        let frame = screen?.frame ?? NSRect(x: 0, y: 0, width: 1920, height: 1080)
        core = AppCoreHandle(
            UInt32(frame.width * scale),
            UInt32(frame.height * scale),
            Float(scale)
        )
        log("AppCoreHandle created (\(Int(frame.width * scale))×\(Int(frame.height * scale)) @\(scale)x)")

        installInputForwarding()
        startTicking()
    }

    /// Deferred launch work, run from the view's `onAppear` (the window + canvas exist by
    /// then — the winit shell defers its launch into `resumed()` for the same reason):
    /// a path passed as `--pb-open /photos` (i.e. `open …/PhotoBlazeMac.app --args
    /// --pb-open /photos`) opens like the winit CLI arg — folder → recursive scan, image →
    /// its folder with the cursor on it, .zip/.7z → the archive contents.
    ///
    /// **Why a flag, not a bare path (the great windowless-app hunt):** AppKit treats a
    /// bare path in `argv[1]` as a document-open launch, and then *suppresses the initial
    /// WindowGroup window entirely* — the app runs windowless with a live menu bar. A
    /// `-`-prefixed argument is ignored by that machinery. Finder-drop +
    /// `application:openURLs:` land with the input adapter (item 4); the native open panel
    /// with the menus (item 8).
    func openLaunchPathIfAny() {
        guard !launchPathOpened else { return }
        launchPathOpened = true
        let args = ProcessInfo.processInfo.arguments
        if let flag = args.firstIndex(of: "--pb-open"), args.indices.contains(flag + 1) {
            let path = args[flag + 1]
            core.open_path(path)
            log("open_path(\(path))")
            drainEffects()
        }
    }

    @ObservationIgnored private var launchPathOpened = false

    // MARK: - Events in

    private func installInputForwarding() {
        // A local monitor sees key events before the responder chain; returning nil swallows
        // them (no system beep). The full physical-key map lives in `KeyMap` (NS1 item 4).
        // ⌘-chords are forwarded to the core (a custom keymap may bind one; unbound ⌘ never
        // falls through to the bare key — the contract's logo rule) but ALSO passed on to
        // AppKit, so the standard menu shortcuts (⌘Q, ⌘W, ⌘M, …) keep working.
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .keyUp]) { [weak self] event in
            guard let self, let name = KeyMap.pbKeyName(for: event.keyCode) else { return event }
            // A native panel (NSOpenPanel) owns the keyboard while it's up — don't swallow
            // its typing/navigation; the viewer ignores input until it resolves.
            if self.panelOpen {
                return event
            }
            if event.type == .keyDown {
                let f = event.modifierFlags
                self.core.key_down(
                    name,
                    f.contains(.control),
                    f.contains(.shift),
                    f.contains(.option),
                    f.contains(.command),
                    event.isARepeat
                )
            } else {
                self.core.key_up(name)
            }
            self.drainEffects()
            return event.modifierFlags.contains(.command) ? event : nil
        }

        // The focus-loss release net: held keys are cleared so nothing keeps flying —
        // on app deactivation AND on the window losing key status (a dialog opening).
        focusObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.didResignActiveNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.forwardFocusLost() }
        }
        keyLossObserver = NotificationCenter.default.addObserver(
            forName: NSWindow.didResignKeyNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.forwardFocusLost() }
        }

        // Finder / Dock / "Open with" URLs (application:open:) — route through the same
        // classify-and-open path as a drop. Buffered by AppDelegate if they arrive before
        // this handler is installed (a cold double-click launch).
        AppDelegate.installOpenHandler { [weak self] urls in
            self?.openPaths(urls.map(\.path))
        }
    }

    private func forwardFocusLost() {
        core.focus_lost()
        drainEffects()
    }

    // MARK: - Pointer + gestures (forwarded by MetalCanvasNSView)

    /// Pointer moved over the canvas, in physical px, top-left origin (the winit convention).
    func pointerMoved(x: Float, y: Float) {
        core.pointer_moved(x, y)
        drainEffects()
    }

    /// Left mouse press/release: on-image controls or drag-to-pan (the core decides).
    func mouseLeft(pressed: Bool) {
        core.mouse_left(pressed)
        drainEffects()
    }

    /// Line-precise scroll (mouse wheel notches).
    func scrollLines(x: Float, y: Float) {
        core.scroll_lines(x, y)
        drainEffects()
    }

    /// Pixel-precise scroll (trackpad two-finger swipe), already scaled to physical px.
    func scrollPixels(x: Float, y: Float) {
        core.scroll_pixels(x, y)
        drainEffects()
    }

    /// Trackpad pinch (incremental magnification).
    func pinch(delta: Float) {
        core.pinch(delta)
        drainEffects()
    }

    /// Trackpad smart-magnify (two-finger double-tap): 100% ↔ fit.
    func doubleTap() {
        core.double_tap()
        drainEffects()
    }

    /// Open dropped / Finder-opened paths (multi-select aware — the launch policy classifies).
    func openPaths(_ paths: [String]) {
        guard !paths.isEmpty else { return }
        let vec = RustVec<RustString>()
        for p in paths {
            vec.push(value: RustString(p))
        }
        core.open_paths(vec)
        log("open_paths(\(paths.count): \(paths.first ?? ""))")
        drainEffects()
    }

    private func startTicking() {
        let timer = Timer(timeInterval: 1.0 / 30.0, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.core.tick()
                self.drainEffects()
            }
        }
        RunLoop.main.add(timer, forMode: .common)
        tickTimer = timer
    }

    // MARK: - The wgpu canvas (NS1 item 2)

    /// Stand the Rust renderer up on the view's `CAMetalLayer`. The layer is retained by
    /// the view; `detachCanvas` drops the renderer before the view dies (the FFI layer
    /// contract), so passing the unretained pointer bits is sound.
    func attachCanvas(layer: CAMetalLayer, pixelSize: CGSize, scale: CGFloat) {
        let ptr = UInt(bitPattern: Unmanaged.passUnretained(layer).toOpaque())
        core.attach_layer(ptr, UInt32(pixelSize.width), UInt32(pixelSize.height), Float(scale))
        configureEDR(on: layer)
        core.render()
        log("canvas attached (\(Int(pixelSize.width))×\(Int(pixelSize.height)) @\(scale)x)")
        drainEffects()
    }

    func canvasResized(pixelSize: CGSize, scale: CGFloat) {
        core.resized(UInt32(pixelSize.width), UInt32(pixelSize.height), Float(scale))
        if let layer = canvasLayer {
            // A surface reconfigure can reset the layer's colorspace — re-assert, exactly
            // like the winit shell does after its resize handling.
            configureEDR(on: layer)
        }
        core.render()
        drainEffects()
    }

    func detachCanvas() {
        canvasLayer = nil
        core.detach_layer()
    }

    /// The layer poke `pb-app/src/hdr_surface.rs` does on the winit target — here the host
    /// owns the layer, so it's plain Swift: an fp16 scRGB surface needs the layer tagged
    /// extended-linear-sRGB (+ EDR on), and the roll-off needs the panel's real headroom
    /// (macOS hard-clips above it; Windows' DWM tone-maps for you).
    private func configureEDR(on layer: CAMetalLayer) {
        canvasLayer = layer
        guard core.wants_edr() else { return }
        layer.colorspace = CGColorSpace(name: CGColorSpace.extendedLinearSRGB)
        layer.wantsExtendedDynamicRangeContent = true
        // NSScreen.main for now; tracking the WINDOW's actual screen (the winit shell's
        // Moved handler — the multi-display bug the port already hit once) comes with the
        // window plumbing in a later slice.
        let headroom = Float(NSScreen.main?.maximumExtendedDynamicRangeColorComponentValue ?? 1.0)
        core.set_edr_headroom(max(1.0, headroom))
    }

    /// The attached canvas layer, kept weakly-by-convention (the view owns it; cleared in
    /// `detachCanvas`) so resize can re-assert the EDR colorspace.
    @ObservationIgnored private weak var canvasLayer: CAMetalLayer?

    // MARK: - Effects out

    /// Pull the effect queue dry and execute each effect — always on the main actor.
    private func drainEffects() {
        while let effect = core.next_effect() {
            apply(effect)
        }
    }

    private func apply(_ effect: CoreEffectFfi) {
        switch effect {
        case .RequestRender:
            core.render()
            log("RequestRender")
        case .SetTitle(let title):
            log("SetTitle(\"\(title.toString())\")")
        case .SetWakeSoon, .ClearWake:
            break // the fixed timer stands in for the wake seam until item 7
        case .Quit:
            log("Quit → NSApp.terminate")
            NSApp.terminate(nil)
        case .ShellFlowAction(let id):
            // A host-side flow command by stable Action id. Esc arrives HERE (the keymap
            // resolves Escape → Action::Quit → a host-side flow action), not as .Quit.
            let action = id.toString()
            log("ShellFlowAction(\"\(action)\")")
            if action == "quit" {
                NSApp.terminate(nil)
            }
        case .ReportError(let msg):
            // An NSAlert once the NS2 dialogs land.
            log("ERROR: \(msg.toString())")
        case .OpenFilePanel(let dir):
            presentOpenPanel(startDir: dir.toString(), choosingFolders: false)
        case .OpenFolderPanel(let dir):
            presentOpenPanel(startDir: dir.toString(), choosingFolders: true)
        case .SetCursor(let kind):
            applyCursor(kind.toString())
        case .Other:
            log("Other (not yet bridged)")
        }
    }

    private func log(_ text: String) {
        effectLog.append(EffectLine(id: nextLineId, text: text))
        nextLineId += 1
        if effectLog.count > 200 {
            effectLog.removeFirst(effectLog.count - 200)
        }
    }

    // MARK: - Native handlers (the genuinely-platform effects)

    /// Whether an NSOpenPanel is up — the key monitor passes events through untouched then,
    /// so typing in the panel (its search field, ⌘-shortcuts) isn't swallowed by the viewer.
    @ObservationIgnored private(set) var panelOpen = false

    /// The native open panel (`CoreEffect::OpenFilePanel`/`OpenFolderPanel`). Mirrors the
    /// winit shell's rfd usage: files default to the images+archives filter (a picked `.zip`
    /// opens as an archive), multi-select allowed; results feed `open_paths` — the same
    /// classify-and-open path as a Finder drop.
    private func presentOpenPanel(startDir: String, choosingFolders: Bool) {
        let panel = NSOpenPanel()
        panel.canChooseFiles = !choosingFolders
        panel.canChooseDirectories = choosingFolders
        panel.allowsMultipleSelection = !choosingFolders
        panel.directoryURL = URL(fileURLWithPath: startDir, isDirectory: true)
        if !choosingFolders {
            // Images + archives — mirror IMAGE_FILTER_EXTS (+zip/7z) in pb-app/src/main.rs.
            // (No "All files" escape hatch here — NSOpenPanel has no filter popup like
            // Windows'; anything exotic can come in via the folder panel or a drop.)
            let exts = [
                "jpg", "jpeg", "jpe", "jfif", "png", "gif", "bmp", "tif", "tiff", "webp",
                "tga", "qoi", "jxl", "svg", "svgz", "heic", "heif", "avif", "hdr", "exr",
                "arw", "nef", "cr2", "cr3", "dng", "raf", "rw2", "orf", "srw", "pef", "raw",
                "zip", "7z",
            ]
            panel.allowedContentTypes = exts.compactMap { UTType(filenameExtension: $0) }
        }
        panelOpen = true
        panel.begin { [weak self] response in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.panelOpen = false
                guard response == .OK, !panel.urls.isEmpty else { return }
                self.openPaths(panel.urls.map(\.path))
            }
        }
    }

    /// `CoreEffect::SetCursor` → NSCursor. "hidden" uses hide-until-mouse-moves, matching
    /// the viewer's cursor etiquette (any movement brings it back). The canvas view also
    /// re-asserts `desiredCursor` on AppKit's `cursorUpdate` pass, so a window-edge resize
    /// cursor (or any SwiftUI cursor rect) can't leak over the canvas.
    private func applyCursor(_ kind: String) {
        switch kind {
        case "hidden":
            desiredCursor = .arrow
            NSCursor.setHiddenUntilMouseMoves(true)
        case "grab":
            desiredCursor = .openHand
        case "grabbing":
            desiredCursor = .closedHand
        case "pointer":
            desiredCursor = .pointingHand
        default:
            desiredCursor = .arrow
        }
        desiredCursor.set()
    }

    /// The cursor the core last asked for — the canvas re-asserts it whenever AppKit
    /// runs a cursor update over the view.
    @ObservationIgnored private(set) var desiredCursor: NSCursor = .arrow
}
