import AppKit
import AVFoundation
import Observation
import PbMacFfi
import UniformTypeIdentifiers

/// Which NS2 dialog is presented as a SwiftUI sheet over the canvas. Confirm/Message are
/// NSAlert sheets (native buttons + Return/Esc for free); About is the standard NSApp
/// panel; Settings is its own window — none of those ride through here.
enum SheetKind: String, Identifiable {
    case password, loading, scanning
    var id: String { rawValue }
}

/// The Swift-side owner of the Rust engine — the NS1 host model.
///
/// Owns the opaque `AppCoreHandle` (the whole `AppCore` lives behind it), forwards input
/// events in, and pulls the effect queue dry on the main actor after every event/tick —
/// the FFI main-thread rule: a worker thread may only *schedule* a drain, never run one.
@MainActor
@Observable
final class CoreModel {
    /// The Rust engine. All calls happen on the main actor.
    private let core: AppCoreHandle

    // MARK: - Dialog state (NS2) — the SwiftUI-observable model the drain mutates

    /// The presented sheet, driven by ShowDialog/CloseDialog. Set programmatically here;
    /// a *user* dismissal goes through `userDismissedSheet()` (→ `dialog_dismissed`).
    private(set) var activeSheet: SheetKind?
    /// The dialog's headline/body text (confirm question, password prompt, progress title).
    private(set) var dialogMessage = ""
    /// Inline error under the password field after a wrong attempt ("" = none).
    private(set) var passwordError = ""
    /// The password field's live contents (view-bound; scrubbed on submit/close).
    var passwordEntry = ""
    /// The password sheet's "Checking…" state while a submitted entry is verified.
    private(set) var dialogChecking = false
    /// Loading sheet: decompressed fraction (0 until the archive header sets a total).
    private(set) var progressFraction: Double = 0
    /// Scanning sheet: supported images found so far / the folder being walked.
    private(set) var scanFound = 0
    private(set) var scanCurrentDir = ""
    /// An NSAlert sheet (confirm/message) is up — gates the key monitor like `panelOpen`.
    @ObservationIgnored private var alertUp = false
    /// Opens the SwiftUI Settings scene — injected by the root view (`openSettings` is an
    /// Environment action only a view can reach).
    @ObservationIgnored var openSettingsAction: (() -> Void)?

    @ObservationIgnored private var keyMonitor: Any?
    @ObservationIgnored private var focusObserver: NSObjectProtocol?
    @ObservationIgnored private var keyLossObserver: NSObjectProtocol?
    /// The display-synchronized frame pump (owned by the canvas view, which creates and
    /// invalidates it with the layer). Nil until the canvas attaches.
    @ObservationIgnored var framePump: FramePump?
    /// A precise off-link wake for a far-out `SetWake` deadline (slideshow dwell, the
    /// Live Photo revert) — cheaper than running the display link the whole wait.
    @ObservationIgnored private var wakeTimer: Timer?
    /// The core's latest wake request, as seconds-from-drain: nil = idle until an event.
    @ObservationIgnored private var requestedWakeDelay: Double?

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
            // Escape tracing for the owner-reported "second Esc quits, first is ignored"
            // — shows exactly which gate (panel/alert/sheet) or state ate the press.
            if name == "Escape" {
                self.log(
                    "Esc \(event.type == .keyDown ? "dn" : "up") repeat=\(event.isARepeat) "
                        + "gate: panel=\(self.panelOpen) alert=\(self.alertUp) "
                        + "sheet=\(self.activeSheet?.rawValue ?? "none")"
                )
            }
            // A native panel (NSOpenPanel), an alert, or a dialog sheet owns the keyboard
            // while it's up — don't swallow its typing/navigation (the password field!);
            // the viewer ignores input until it resolves.
            if self.panelOpen || self.alertUp || self.activeSheet != nil {
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
            self.kick() // a hold/nav may have started — run the pump
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
        kick()
        drainEffects()
    }

    /// Line-precise scroll (mouse wheel notches).
    func scrollLines(x: Float, y: Float) {
        core.scroll_lines(x, y)
        kick()
        drainEffects()
    }

    /// Pixel-precise scroll (trackpad two-finger swipe), already scaled to physical px.
    func scrollPixels(x: Float, y: Float) {
        core.scroll_pixels(x, y)
        kick()
        drainEffects()
    }

    /// Trackpad pinch (incremental magnification).
    func pinch(delta: Float) {
        core.pinch(delta)
        kick()
        drainEffects()
    }

    /// Trackpad smart-magnify (two-finger double-tap): 100% ↔ fit.
    func doubleTap() {
        core.double_tap()
        kick()
        drainEffects()
    }

    /// The native menu bar. Installed from `onAppear` — SwiftUI writes its own main menu
    /// during launch, so ours must land after it to win.
    @ObservationIgnored private var menuBar: MenuBar?

    func installMenuBarIfNeeded() {
        guard menuBar == nil else { return }
        menuBar = MenuBar(model: self)
        menuBar?.sync(core.menu_state())
    }

    /// A native menu item fired (by stable Action id) — same dispatch as the keyboard.
    func menuAction(_ id: String) {
        core.menu_action(id)
        kick()
        drainEffects()
    }

    /// Right-click over the photo: ask the core for the context-menu description; the
    /// resulting ShowContextMenu effect pops it at the stashed event location.
    func contextMenu(at event: NSEvent, in view: NSView) {
        pendingContextMenuEvent = (event, view)
        core.context_menu()
        drainEffects()
        pendingContextMenuEvent = nil
    }

    @ObservationIgnored private var pendingContextMenuEvent: (NSEvent, NSView)?

    /// `CoreEffect::ShowContextMenu` — the curated per-photo popup (task #41), mirroring
    /// menu.rs `build_context_menu`. Items dispatch by the same Action ids as the menu bar.
    fileprivate func popContextMenu(
        hasImage: Bool, hasMotion: Bool, canReveal: Bool, fullscreen: Bool
    ) {
        guard hasImage, let (event, view) = pendingContextMenuEvent else { return }
        let menu = NSMenu()
        menu.autoenablesItems = false
        let add = { (id: String, title: String) in
            let item = NSMenuItem(
                title: title, action: #selector(self.contextItemFired(_:)), keyEquivalent: ""
            )
            item.target = self
            item.representedObject = id
            menu.addItem(item)
        }
        add("next", "Next")
        add("prev", "Previous")
        add("random", "Random")
        add("random_prev", "Previous Random")
        menu.addItem(.separator())
        add("rotate_ccw", "Rotate Left")
        add("rotate_cw", "Rotate Right")
        if hasMotion {
            add("play_pause", "Play/Pause")
        }
        menu.addItem(.separator())
        add("slideshow", "Start/Stop Slideshow")
        menu.addItem(.separator())
        add("copy", "Copy Image")
        add("copy_path", "Copy File Path")
        add("copy_image_details", "Copy Image Details")
        if canReveal {
            add("reveal", "Show in Finder")
        }
        menu.addItem(.separator())
        add("fullscreen", fullscreen ? "Exit Fullscreen" : "Enter Fullscreen")
        NSMenu.popUpContextMenu(menu, with: event, for: view)
    }

    @objc private func contextItemFired(_ sender: NSMenuItem) {
        guard let id = sender.representedObject as? String else { return }
        menuAction(id)
    }

    /// `CoreEffect::ShowDialog` — the NS2 dialog router. About = the standard NSApplication
    /// panel (ADR-021); Confirm/Message = NSAlert sheets; Password/Loading/Scanning =
    /// SwiftUI sheets bound to the dialog state; Settings = the Settings scene. The text
    /// payload rides in `dialog_message()` (pull-after-marker). Re-delivery of the same
    /// kind updates the sheet in place (a scanning re-point, a wrong-password retry).
    private func showDialog(_ kind: String) {
        switch kind {
        case "about":
            NSApp.activate()
            NSApp.orderFrontStandardAboutPanel(nil)
        case "settings":
            openSettingsAction?()
        case "confirm":
            presentConfirmAlert(core.dialog_message().toString())
        case "message":
            presentMessageAlert(core.dialog_message().toString())
        case "password":
            dialogMessage = core.dialog_message().toString()
            passwordError = core.dialog_password_error().toString()
            passwordEntry = "" // fresh prompt or wrong-attempt retry: field starts empty
            dialogChecking = false
            activeSheet = .password
        case "loading":
            dialogMessage = core.dialog_message().toString()
            progressFraction = 0
            dialogChecking = false
            activeSheet = .loading
        case "scanning":
            dialogMessage = core.dialog_message().toString()
            scanFound = 0
            scanCurrentDir = ""
            activeSheet = .scanning
        default:
            log("ShowDialog(\(kind)) — unknown kind")
        }
    }

    /// The delete Confirm (`ShowDialog("confirm")`): a native warning sheet, Delete as the
    /// destructive default, Cancel on Esc. The answer returns via `dialog_confirm_answered`
    /// and the core runs (or forgets) the armed permanent delete.
    private func presentConfirmAlert(_ message: String) {
        let alert = NSAlert()
        alert.messageText = message
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Delete").hasDestructiveAction = true
        alert.addButton(withTitle: "Cancel")
        presentAlert(alert) { [weak self] response in
            guard let self else { return }
            self.core.dialog_confirm_answered(response == .alertFirstButtonReturn)
            self.kick() // the delete-advance runs on the tick loop
            self.drainEffects()
        }
    }

    /// A one-button informational / error notice (`ReportError` + `ShowDialog("message")`):
    /// the egui Message dialog's native twin.
    private func presentMessageAlert(_ message: String) {
        let alert = NSAlert()
        alert.messageText = message
        alert.alertStyle = .warning
        alert.addButton(withTitle: "OK")
        presentAlert(alert) { [weak self] _ in
            guard let self else { return }
            self.core.dialog_closed()
            self.drainEffects()
        }
    }

    /// Run an alert as a window-attached sheet (app-modal fallback when no window yet),
    /// gating the key monitor while it's up so its Return/Esc aren't swallowed.
    private func presentAlert(
        _ alert: NSAlert, completion: @escaping (NSApplication.ModalResponse) -> Void
    ) {
        alertUp = true
        if let window = hostWindow {
            alert.beginSheetModal(for: window) { [weak self] response in
                MainActor.assumeIsolated {
                    self?.alertUp = false
                    completion(response)
                }
            }
        } else {
            let response = alert.runModal()
            alertUp = false
            completion(response)
        }
    }

    // MARK: - Sheet actions (NS2) — each maps one user gesture to its DialogResolved entry

    /// The user dismissed the presented sheet by a path other than its buttons (the sheet
    /// binding wrote nil). The core cancels any matching in-flight op and closes.
    func userDismissedSheet() {
        log("userDismissedSheet")
        activeSheet = nil
        core.dialog_dismissed()
        drainEffects()
    }

    /// Password Unlock / Return: submit the entry; the core shows "Checking…" and re-opens
    /// the pending archive with it. The field is scrubbed immediately (RAM-only etiquette).
    func passwordSubmit() {
        let entry = passwordEntry
        passwordEntry = ""
        guard !entry.isEmpty else { return }
        core.password_submitted(entry)
        kick() // the re-open runs on this crate's worker; the pump polls it
        drainEffects()
    }

    /// Password Cancel / Esc: abandon the pending archive.
    func passwordCancel() {
        passwordEntry = ""
        core.password_cancelled()
        drainEffects()
    }

    /// The archive "Opening…" sheet's Cancel.
    func loadingCancel() {
        core.loading_cancelled()
        drainEffects()
    }

    /// The folder "Scanning…" sheet's Cancel (stops the walk, keeps the current view).
    func scanningCancel() {
        core.scanning_cancelled()
        drainEffects()
    }

    // MARK: - Settings (NS2 item 5)

    /// The current settings as the flat form the Settings window binds to.
    func settingsForm() -> SettingsFormFfi {
        core.settings_form()
    }

    /// Settings Save: validate/clamp Rust-side and apply + persist through the core.
    func settingsSave(_ form: SettingsFormFfi) {
        core.submit_settings(form)
        kick()
        drainEffects()
    }

    /// Settings Cancel (buttons or the window's close button) — discard the draft.
    func settingsCancel() {
        core.settings_cancelled()
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
        kick() // the scan/open worker needs the pump polling
        drainEffects()
    }

    // MARK: - The frame pump (NS1 item 7)

    /// One frame of the engine loop, fired by the display link each refresh while running:
    /// tick (held-key pacing, slideshow, prefetch pump, animation, worker polls) → drain →
    /// re-decide the pacing. The winit `about_to_wait` equivalent.
    func pump() {
        core.tick()
        drainEffects()
        // Refresh the shown progress sheet from the Rust-side handles (a cheap read; the
        // pump is already running while a scan/open worker is in flight).
        if activeSheet == .loading || activeSheet == .scanning {
            let p = core.dialog_progress()
            progressFraction = Double(p.fraction)
            scanFound = Int(p.found)
            scanCurrentDir = p.current_dir.toString()
        }
        updatePacing()
    }

    /// Continuous while the engine has work or an imminent wake; a precise timer for a
    /// far-out wake; fully idle (link paused, no timers) otherwise. Mirrors the winit
    /// shell's ControlFlow::WaitUntil/Wait decision.
    private func updatePacing() {
        guard let pump = framePump else { return }
        wakeTimer?.invalidate()
        wakeTimer = nil
        if core.work_pending() {
            pump.paused = false
            return
        }
        guard let delay = requestedWakeDelay else {
            pump.paused = true // idle: the next input/panel/wake kicks us awake
            return
        }
        if delay <= 0.02 {
            pump.paused = false // within ~a frame — stay on the link
        } else {
            pump.paused = true
            let timer = Timer(timeInterval: delay, repeats: false) { [weak self] _ in
                MainActor.assumeIsolated {
                    guard let self else { return }
                    self.framePump?.paused = false
                    self.pump()
                }
            }
            RunLoop.main.add(timer, forMode: .common)
            wakeTimer = timer
        }
    }

    /// Any input may start work (a hold, a decode, an open) — make sure the loop is alive;
    /// it re-pauses itself once the engine goes quiet.
    private func kick() {
        framePump?.paused = false
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
        // SwiftUI may have clobbered the F-mode chrome during any UI pass since the last
        // drain — re-assert it (compare-before-set, so a no-op in the steady state).
        assertWindowChrome()
    }

    private func apply(_ effect: CoreEffectFfi) {
        switch effect {
        case .RequestRender:
            core.render()
            log("RequestRender")
        case .SetTitle(let title):
            let t = title.toString()
            hostWindow?.title = t
            // SetTitle fires exactly when the displayed item changes — the right cadence
            // for the title-bar proxy icon too (hover the title bar to reveal/drag it;
            // hover-to-reveal is standard since macOS 11).
            refreshProxyIcon()
            log("SetTitle(\"\(t)\")")
        case .SetWake(let seconds):
            requestedWakeDelay = seconds
        case .ClearWake:
            requestedWakeDelay = nil
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
            // A user-facing error (bad open, refused archive, …) — a native alert.
            let text = msg.toString()
            log("ERROR: \(text)")
            presentMessageAlert(text)
        case .CloseDialog:
            // Programmatic close (the answer was processed / the op finished). Setting
            // activeSheet directly never routes through userDismissedSheet().
            activeSheet = nil
            dialogChecking = false
            passwordEntry = ""
        case .SetDialogChecking:
            dialogChecking = true
        case .OpenFilePanel(let dir):
            presentOpenPanel(startDir: dir.toString(), choosingFolders: false)
        case .OpenFolderPanel(let dir):
            presentOpenPanel(startDir: dir.toString(), choosingFolders: true)
        case .SetCursor(let kind):
            applyCursor(kind.toString())
        case .WriteClipboard:
            writeClipboard()
        case .RevealPath(let path):
            NSWorkspace.shared.activateFileViewerSelecting([
                URL(fileURLWithPath: path.toString())
            ])
            log("RevealPath")
        case .StartLiveAudio(let path, let atSecs):
            startLiveAudio(path: path.toString(), atSecs: atSecs)
        case .StopLiveAudio:
            liveAudio?.stop()
            liveAudio = nil
        case .PauseLiveAudio:
            liveAudio?.pause()
        case .ResumeLiveAudio:
            liveAudio?.play()
        case .SetWindowMode(let fullscreen):
            setWindowMode(fullscreen: fullscreen)
        case .HideWindow:
            hostWindow?.orderOut(nil)
        case .MenuStateChanged:
            menuBar?.sync(core.menu_state())
        case .ShowContextMenu(let hasImage, let hasMotion, let canReveal, let fullscreen):
            popContextMenu(
                hasImage: hasImage, hasMotion: hasMotion,
                canReveal: canReveal, fullscreen: fullscreen
            )
        case .ShowDialog(let kind):
            showDialog(kind.toString())
        case .Other:
            log("Other (not yet bridged)")
        }
    }

    /// Dev diagnostics. The NS1 on-screen effect log retired with the NS2 dialogs; a
    /// terminal launch (`swift run`, the dev build-run loop) still sees the trace.
    private func log(_ text: String) {
        #if DEBUG
            print("PB: \(text)")
        #endif
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

    /// The hosting NSWindow (handed over by the canvas view) — the SetTitle target.
    @ObservationIgnored weak var hostWindow: NSWindow? {
        didSet {
            guard oldValue !== hostWindow else { return }
            installScreenChangeClamp()
        }
    }

    @ObservationIgnored private var screenChangeObserver: NSObjectProtocol?
    @ObservationIgnored private var dragSettleTimer: Timer?

    /// Clamp-on-screen-change: dragging the window from a wide monitor to a narrower one
    /// leaves it wider than the destination screen — stock AppKit only protects titlebar
    /// reachability, never width. When the window lands on a new screen and doesn't
    /// *fit*, shrink it to the screen's visible area (never grow, and a window that fits
    /// but sits partly offscreen is left alone — parking a window half-off is a
    /// legitimate user choice). Deferred until the drag settles: the notification fires
    /// mid-drag with the button still down, and resizing a window the user is holding is
    /// the one version of this that would feel platform-hostile.
    private func installScreenChangeClamp() {
        if let old = screenChangeObserver {
            NotificationCenter.default.removeObserver(old)
            screenChangeObserver = nil
        }
        guard let window = hostWindow else { return }
        screenChangeObserver = NotificationCenter.default.addObserver(
            forName: NSWindow.didChangeScreenNotification, object: window, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.clampToScreenWhenSettled() }
        }
    }

    private func clampToScreenWhenSettled() {
        guard !speedModeFullscreen else { return } // F mode owns its frame
        dragSettleTimer?.invalidate()
        dragSettleTimer = nil
        // A window drag belongs to the window server — its mouse events never reach the
        // app — so poll the global button state until the user lets go.
        guard NSEvent.pressedMouseButtons != 0 else {
            clampToScreenNow()
            return
        }
        let timer = Timer(timeInterval: 0.1, repeats: true) { [weak self] timer in
            MainActor.assumeIsolated {
                guard let self, NSEvent.pressedMouseButtons == 0 else { return }
                timer.invalidate()
                self.dragSettleTimer = nil
                self.clampToScreenNow()
            }
        }
        RunLoop.main.add(timer, forMode: .common)
        dragSettleTimer = timer
    }

    private func clampToScreenNow() {
        guard !speedModeFullscreen, let window = hostWindow, let screen = window.screen
        else { return }
        let visible = screen.visibleFrame
        var frame = window.frame
        guard frame.width > visible.width || frame.height > visible.height else { return }
        frame.size.width = min(frame.width, visible.width)
        frame.size.height = min(frame.height, visible.height)
        frame.origin.x = max(visible.minX, min(frame.origin.x, visible.maxX - frame.width))
        frame.origin.y = max(visible.minY, min(frame.origin.y, visible.maxY - frame.height))
        window.setFrame(frame, display: true, animate: true)
        log("clamped oversized window to \(Int(frame.width))×\(Int(frame.height)) after screen change")
    }

    /// The path the proxy icon currently shows (cached so unchanged photos are a no-op).
    @ObservationIgnored private var proxyIconPath = ""
    /// The window is in the borderless fullscreen speed mode (no title bar → no proxy).
    /// Observable: ContentView drives the SwiftUI-owned titlebar surface off it
    /// (`.toolbarBackground` — the AppKit props alone get re-clobbered by SwiftUI).
    private(set) var speedModeFullscreen = false

    /// The title-bar **proxy icon** (`NSWindow.representedURL` — the winit shell's
    /// `refresh_proxy_icon` mirrored, macOS port task #12): the draggable doc icon +
    /// ⌘-click folder popup. Windowed mode only, and only for a real on-disk file — an
    /// archive entry or the empty deck clears it. RAM-only, never
    /// `noteNewRecentDocumentURL:` (no Recents → privacy #2 holds).
    private func refreshProxyIcon() {
        let want = speedModeFullscreen ? "" : core.current_photo_path().toString()
        guard want != proxyIconPath else { return }
        proxyIconPath = want
        hostWindow?.representedURL = want.isEmpty ? nil : URL(fileURLWithPath: want)
    }

    /// The Live Photo's audio player (its companion .mov's audio track). The core decides
    /// when/where; this is just the retained platform handle (the winit shell's ObjC twin).
    @ObservationIgnored private var liveAudio: AVAudioPlayer?

    private func startLiveAudio(path: String, atSecs: Double) {
        liveAudio?.stop()
        liveAudio = try? AVAudioPlayer(contentsOf: URL(fileURLWithPath: path))
        liveAudio?.currentTime = max(0, atSecs)
        liveAudio?.play()
    }

    /// `CoreEffect::WriteClipboard` (via the marker + accessors): text goes on as a string;
    /// an image goes on as one pasteboard item carrying BOTH the rendered TIFF and — when
    /// the photo is a real on-disk file — its file URL, mirroring the Windows
    /// CF_DIBV5 + CF_HDROP pairing. The host toasts afterwards (winit-shell parity).
    private func writeClipboard() {
        let pb = NSPasteboard.general
        let text = core.take_clipboard_text().toString()
        if !text.isEmpty {
            pb.clearContents()
            pb.setString(text, forType: .string)
            let toastMsg = text.contains("\n")
                ? "Copied to clipboard"
                : "Copied \((text as NSString).lastPathComponent)"
            core.toast(toastMsg)
            return
        }
        let w = Int(core.clipboard_image_width())
        let h = Int(core.clipboard_image_height())
        let file = core.clipboard_image_file().toString()
        let rgba = core.take_clipboard_image()
        guard w > 0, h > 0, rgba.len() == w * h * 4 else { return }
        let data = Data(bytes: UnsafeRawPointer(rgba.as_ptr()), count: rgba.len())
        guard let provider = CGDataProvider(data: data as CFData),
              let cg = CGImage(
                  width: w,
                  height: h,
                  bitsPerComponent: 8,
                  bitsPerPixel: 32,
                  bytesPerRow: w * 4,
                  space: CGColorSpace(name: CGColorSpace.sRGB)!,
                  bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.last.rawValue),
                  provider: provider,
                  decode: nil,
                  shouldInterpolate: false,
                  intent: .defaultIntent
              )
        else {
            core.toast("Copy failed")
            return
        }
        let image = NSImage(cgImage: cg, size: NSSize(width: w, height: h))
        pb.clearContents()
        let item = NSPasteboardItem()
        if let tiff = image.tiffRepresentation {
            item.setData(tiff, forType: .tiff)
        }
        if !file.isEmpty {
            item.setString(URL(fileURLWithPath: file).absoluteString, forType: .fileURL)
        }
        pb.writeObjects([item])
        core.toast("Copied")
    }

    /// `CoreEffect::SetWindowMode` — the borderless fullscreen **speed mode** (F), NOT
    /// macOS native Spaces fullscreen. Chromeless-but-key: keep `.titled` (a truly
    /// borderless NSWindow can't become key) and hide the titlebar + traffic lights
    /// instead; menu bar + Dock auto-hide while frontmost. NS3 replaces this with the
    /// bespoke window treatment.
    private func setWindowMode(fullscreen: Bool) {
        guard let window = hostWindow else { return }
        speedModeFullscreen = fullscreen
        refreshProxyIcon() // no title bar in the speed mode → clear; restore on exit
        if fullscreen {
            savedFrame = window.frame
            savedStyleMask = window.styleMask
            // TRUE borderless: dropping `.titled` is what removes Tahoe's superelliptical
            // corner radius + glass rim, so the photo owns every pixel. A bare borderless
            // NSWindow refuses key status (and SwiftUI's window class doesn't override
            // that — the probe below failed on the first attempt), so first make the
            // window keyable-when-borderless, then probe; if it still refuses, fall back
            // to the chromeless-but-titled look rather than a dead keyboard.
            makeKeyableWhenBorderless(window)
            window.styleMask = [.borderless, .fullSizeContentView]
            borderlessOK = window.canBecomeKey
            if !borderlessOK {
                window.styleMask = savedStyleMask ?? window.styleMask
                log("borderless probe failed — keeping the titled fallback")
            }
            NSApp.presentationOptions = [.autoHideMenuBar, .autoHideDock]
            if let screen = window.screen ?? NSScreen.main {
                window.setFrame(screen.frame, display: true)
            }
            window.makeKeyAndOrderFront(nil)
        } else {
            NSApp.presentationOptions = []
            if let mask = savedStyleMask {
                window.styleMask = mask
            }
            borderlessOK = false
            if let frame = savedFrame {
                window.setFrame(frame, display: true)
            }
        }
        assertWindowChrome()
    }

    @ObservationIgnored private var savedFrame: NSRect?
    /// The windowed-mode style mask, captured entering F mode and restored on exit.
    @ObservationIgnored private var savedStyleMask: NSWindow.StyleMask?
    /// F mode achieved true borderless (vs the titled fallback) — assertWindowChrome
    /// keeps the mask that way if SwiftUI re-adds `.titled` mid-mode.
    @ObservationIgnored private var borderlessOK = false

    /// Window classes already patched keyable (once per class, app lifetime).
    private static var keyablePatched: Set<String> = []

    /// Let `window` stay key without `.titled`: NSWindow refuses key status while
    /// borderless (and SwiftUI's window class keeps that default), which would mean a
    /// dead keyboard in the F speed mode — the effect winit gets from a static
    /// `canBecomeKeyWindow → YES` override on its own NSWindow subclass.
    ///
    /// ⚠ Patch the window's declared CLASS, never the instance's isa: the first version
    /// used `object_setClass` (a dynamic subclass swap), which corrupts KVO's
    /// bookkeeping on an observed object — SwiftUI KVO-observes its window, and the
    /// very next `setStyleMask` crashed in `NSHostingView`'s observer removal
    /// (the owner-reported F-mode segfault). A `class_replaceMethod` override leaves
    /// the KVO shim chain intact. Titled windows already answer yes, so the override
    /// only changes behavior for the borderless speed mode; the Settings window (same
    /// class, always titled) is unaffected.
    private func makeKeyableWhenBorderless(_ window: NSWindow) {
        // Walk past any KVO shim (NSKVONotifying_*) to the real declared class.
        var cls: AnyClass = object_getClass(window) ?? NSWindow.self
        while NSStringFromClass(cls).hasPrefix("NSKVONotifying_"),
              let sup = class_getSuperclass(cls) {
            cls = sup
        }
        let name = NSStringFromClass(cls)
        guard !Self.keyablePatched.contains(name) else { return }
        Self.keyablePatched.insert(name)
        let yes: @convention(block) (AnyObject?) -> Bool = { _ in true }
        for sel in ["canBecomeKeyWindow", "canBecomeMainWindow"] {
            class_replaceMethod(
                cls, NSSelectorFromString(sel),
                imp_implementationWithBlock(yes), "B@:"
            )
        }
    }

    /// Keep the window chrome matching the mode. A one-shot mutation is NOT enough:
    /// SwiftUI owns the WindowGroup window's titlebar and re-asserts transparency /
    /// button visibility on its own update passes (clobbering the F-mode styling — the
    /// owner-reported "titlebar stays, stoplights visible" bug). So the desired chrome is
    /// cheaply re-asserted after every drain, compare-before-set — the same
    /// defeat-the-framework pattern the winit shell uses for per-tick menu state.
    private func assertWindowChrome() {
        guard let window = hostWindow else { return }
        let fs = speedModeFullscreen
        if fs, borderlessOK {
            // True borderless mode: keep it that way if SwiftUI re-adds `.titled`.
            if window.styleMask.contains(.titled) {
                window.styleMask = [.borderless, .fullSizeContentView]
            }
        } else if window.styleMask.contains(.fullSizeContentView) != fs {
            if fs {
                window.styleMask.insert(.fullSizeContentView)
            } else {
                window.styleMask.remove(.fullSizeContentView)
            }
        }
        // No shadow in F mode — its rim highlight reads as a border at the screen edge.
        if window.hasShadow != !fs {
            window.hasShadow = !fs
        }
        if window.titlebarAppearsTransparent != fs {
            window.titlebarAppearsTransparent = fs
        }
        let vis: NSWindow.TitleVisibility = fs ? .hidden : .visible
        if window.titleVisibility != vis {
            window.titleVisibility = vis
        }
        let sep: NSTitlebarSeparatorStyle = fs ? .none : .automatic
        if window.titlebarSeparatorStyle != sep {
            window.titlebarSeparatorStyle = sep
        }
        for kind in [NSWindow.ButtonType.closeButton, .miniaturizeButton, .zoomButton] {
            if let button = window.standardWindowButton(kind), button.isHidden != fs {
                button.isHidden = fs
            }
        }
    }
}
