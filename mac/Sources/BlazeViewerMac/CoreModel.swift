import AppKit
import AVFoundation
import Observation
import PbMacFfi
import QuartzCore
import SwiftUI
import UniformTypeIdentifiers

/// Which NS2 dialog is presented as a SwiftUI sheet over the canvas. Confirm/Message are
/// NSAlert sheets (native buttons + Return/Esc for free); About is the standard NSApp
/// panel; Settings is its own window — none of those ride through here.
enum SheetKind: String, Identifiable {
    case password, ask, loading, scanning
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
    /// The "Ask about image" question field's live contents (task #44; view-bound).
    var askEntry = ""
    /// The password sheet's "Checking…" state while a submitted entry is verified.
    private(set) var dialogChecking = false
    /// Loading sheet: decompressed fraction (0 until the archive header sets a total).
    private(set) var progressFraction: Double = 0
    /// Scanning sheet: supported images found so far / the folder being walked.
    private(set) var scanFound = 0
    private(set) var scanCurrentDir = ""

    /// The ambient scan pill (task #54, ④): a non-blocking, top-center progress element that
    /// replaces the modal Scanning dialog and the old in-canvas chip — you keep browsing the
    /// photos already streaming in while the rest of a big folder scans. Refreshed each pump
    /// (a scan keeps the pump running) from the Rust worker handle.
    private(set) var scanPillVisible = false
    private(set) var scanPillName = ""
    private(set) var scanPillFound = 0
    private(set) var scanPillCurrent = ""

    /// The unified native toast (every `show_toast` fires one now that the core suppresses the
    /// HUD raster) — a bottom-center SwiftUI pill. `toastSeq` increments per toast so the view
    /// re-animates even for a repeated message; the core expires it (`toastVisible` → false).
    private(set) var toastVisible = false
    private(set) var toastMessage = ""
    private(set) var toastIcon = 0
    private(set) var toastSeq: UInt64 = 0

    /// The one-line info readout (`i`) — the last HUD element to go native. `rel · W×H ·
    /// CODEC[· Live]`, in a small bottom-corner pill; `align` is 0 left / 1 center / 2 right.
    private(set) var infoLineVisible = false
    private(set) var infoLineText = ""
    private(set) var infoLineCodec = ""
    private(set) var infoLineIsLive = false
    private(set) var infoLineIsAnimated = false
    private(set) var infoLineIsVideo = false
    private(set) var infoLineAlign = 2

    /// The info-line **playback row** (task 79.9 phase 5): a play/pause button + a
    /// click/drag scrubber + elapsed/total, shown while a video is active and the
    /// info line is visible. Two clock sources behind the same vars: a native
    /// video's AVPlayer periodic time observer, or — for a session-backed (FFmpeg,
    /// task #84 §8) video — the pump polling the core's session clock.
    private(set) var videoControlsVisible = false
    private(set) var videoElapsed = "0:00"
    private(set) var videoTotal = ""
    private(set) var videoFraction = 0.0
    private(set) var videoPlaying = false
    /// Whether the displayed item plays through the core's `VideoSession` (the FFmpeg
    /// route) — edge-tracked in `pump()` to reset the scrubber on a fresh session.
    @ObservationIgnored private var sessionVideoActive = false
    /// True while the user is actively dragging the info-line scrubber. A drag captures the
    /// pointer, so canvas hover moves stop and the reveal flash would decay mid-drag — the
    /// pump keeps the controls up while this is set (not `@Published`; only `pump()` reads it).
    var videoScrubbing = false
    /// True while the track picker's popover is open (task #99) — the same "pin the controls
    /// up" need as `videoScrubbing`, and separate from it for the same reason the two can't
    /// share one flag: whichever ended last would unpin the other. The pointer is inside the
    /// popover, not over the canvas, so the hover flash stops refreshing and the bar the
    /// popover is anchored to would fade out from under it.
    var videoPickerOpen = false
    /// While a scrubber seek is settling, the fraction the knob (and time label) should hold
    /// — the *target* — so a ~20 Hz progress report at the pre-seek position can't snap the
    /// knob back before the seek visually lands (plan §H3: the scrubber-flash symptom — click
    /// ahead, jump to target, snap back, land on target). Set on each fractional seek (the
    /// latest wins, so a superseding scrub replaces it); cleared when a report reaches the
    /// target in the seek's direction (the landing), or on a new/ended/failed video. `nil` =
    /// not settling, so the knob tracks the live playhead. A stalled seek (H1) simply stays
    /// pinned at the target, which is the correct scrubber behaviour regardless of H1's fix.
    @ObservationIgnored private var pendingSeekTarget: Double?
    /// Whether subtitles are switched on (the `C` state) — the picker button fills its icon
    /// on this, so it must be observable rather than read through on each draw. Refreshed in
    /// `pump()` while the controls are up.
    private(set) var subtitlesOn = false

    /// The subtitle overlay (task #90): the core rasterizes the cue — shaping, outline,
    /// shadow, background, placement — and hands us a bitmap and a rect. The whole Swift
    /// side is "draw this image there", so macOS and winit can never disagree about what a
    /// subtitle looks like.
    private(set) var subtitleImage: NSImage?
    private(set) var subtitleRect: CGRect = .zero
    /// The generation of `subtitleImage`. A cue lives for seconds, so this is unchanged on
    /// nearly every frame — and an unchanged generation transfers no pixels.
    @ObservationIgnored private var subtitleGen: UInt64 = 0

    /// The native play hint (▶ / Live Photo on a motion item) — the last on-image HUD overlay
    /// to go native. `kind`: 0 none / 1 Live Photo / 2 animation. It flashes for ~3s on a fresh
    /// motion item, hover holds it open, and a click (or P) plays.
    private(set) var playHintVisible = false
    private(set) var playHintKind = 0
    @ObservationIgnored private var playHintSeq: UInt64 = 0
    @ObservationIgnored private var playHintHovered = false
    @ObservationIgnored private var playHintFadeTask: Task<Void, Never>?

    // MARK: - Native rich panels (task #54, mac-first) — the first is Help

    /// Whether the native SwiftUI Help panel should show, and its sections — refreshed
    /// from the core on a `PanelsChanged` marker (`refreshHelp`). The core suppresses
    /// Help's HUD rasterization while this host presents it, so there's no double-draw.
    private(set) var helpVisible = false
    private(set) var helpSections: [HelpSection] = []
    /// Whether the native empty-state Open panel (the welcome surface) should show —
    /// true when no photos are loaded. Refreshed on the same `PanelsChanged` marker.
    private(set) var openPanelVisible = false
    /// The native Inspector panel (Details/Text/Describe tabs) — visibility, selected
    /// tab (0/1/2), and the active tab's rows, all refreshed on `PanelsChanged`
    /// (`refreshInspector`). The core re-signals on async OCR / describe results too.
    private(set) var inspectorVisible = false
    private(set) var inspectorTab = 0
    private(set) var inspectorRows: [InspectorRow] = []
    /// The native folder tree (⇧F) — visibility and the current photo's folder hierarchy
    /// as a flat, depth-indented row list, refreshed on `PanelsChanged` (`refreshTree`).
    /// A native list scrolls, so every derived row shows (no HUD paging).
    private(set) var treeVisible = false
    private(set) var treeRows: [FolderTreeRow] = []
    /// The current photo's folder path — the tree view scrolls the current row into view
    /// when this changes (so advancing to an off-screen folder pulls it back into view),
    /// keyed off the folder itself so an unrelated expand/collapse doesn't yank the scroll.
    private(set) var currentTreePath = ""
    /// Whether the Finder tree (chevron expand/collapse, name-to-open) is active — else
    /// the flat v1 archive tree (click-to-activate). Drives the row rendering + actions.
    private(set) var treeUsesFs = false
    /// The native Thumbnails strip (⇧T, task #83) — the left pane's second tab.
    /// Visibility + the virtual row count + the current highlight + the store's
    /// change counter, refreshed on `PanelsChanged` (`refreshThumbs`).
    private(set) var thumbsVisible = false
    private(set) var leftTab = 0
    private(set) var thumbCount = 0
    private(set) var thumbCurrent = -1
    private(set) var thumbDirty: UInt64 = 0
    /// The pending follow-scroll command (item + generation), pulled from the
    /// core; `thumbScrollSeq` bumps so the panel's `.onChange` fires even when
    /// the same item repeats.
    private(set) var thumbScrollItem = -1
    private(set) var thumbScrollGen: UInt64 = 0
    private(set) var thumbScrollSeq: UInt64 = 0
    /// Per-cell NSImage cache keyed by playlist index; entries carry the store
    /// generation they were built from (pull-once, plan §8). Bounded: pruned
    /// around the most recent pull; emptied when the tab closes.
    private var thumbImages: [Int: (gen: UInt64, image: NSImage)] = [:]
    /// User-resizable panel widths (drag the inner edge). The defaults are the minimums;
    /// session-persistent (survive close/reopen) — disk persistence is a later slice.
    /// ONE width for the whole left pane, whichever tab shows (the Inspector
    /// idiom — owner call 2026-07-12): switching tabs never resizes the panel.
    var treeWidth: CGFloat = 280
    var inspectorWidth: CGFloat = 360
    /// The shared native-panel background opacity (0.5–1.0), from the Settings "Panel opacity"
    /// slider — fed to `panelBackground`. Refreshed on load + on every settings edit so a
    /// live slider drag updates the tree / inspector / scan pill / toast immediately.
    private(set) var panelOpacity: Double = 0.92
    /// An NSAlert sheet (confirm/message) is up — gates the key monitor like `panelOpen`.
    @ObservationIgnored private var alertUp = false
    /// Opens the SwiftUI Settings scene — injected by the root view (`openSettings` is an
    /// Environment action only a view can reach).
    @ObservationIgnored var openSettingsAction: (() -> Void)?

    @ObservationIgnored private var keyMonitor: Any?
    @ObservationIgnored private var focusObserver: NSObjectProtocol?
    /// Prints the `--metrics` report on quit (task #78) — see `init`.
    @ObservationIgnored private var metricsObserver: NSObjectProtocol?
    @ObservationIgnored private var keyLossObserver: NSObjectProtocol?
    @ObservationIgnored private var keyGainObserver: NSObjectProtocol?
    @ObservationIgnored private var menuTrackObservers: [NSObjectProtocol] = []
    /// Open menus being tracked (menu bar or context menu). While non-zero,
    /// `assertWindowChrome` must not touch the window — a `styleMask` write during menu
    /// tracking makes AppKit dismiss the open menu, and the pump runs in `.common` mode
    /// so a drain can land mid-tracking. (Defensive: the 2026-07-02 "menus snap shut"
    /// report turned out to be macOS High Performance Screen Sharing doing this to every
    /// app — but the styleMask hazard is real, so the guard stays.)
    @ObservationIgnored private var menuTrackingDepth = 0
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

        // Apply the CLI session overrides (task #78) IMMEDIATELY — before anything reads
        // `startup_fullscreen` / `effective_appearance` / the menu state, so a `--theme` /
        // `--fullscreen` / `--mute` launch wears its override from the first frame. The
        // preflight (Launch.preflight, in the App init) already gated help/version/usage
        // errors, so this parse cannot fail; the positional paths it stashes are opened by
        // `openLaunchPathIfAny` once the canvas exists.
        core.apply_launch_args(Launch.argvVec(), RustString(Launch.versionString))
        // Arm the Apple-Event echo filter: a bare-path launch re-delivers the argv
        // path as a document-open (see Launch.filterLaunchEcho).
        Launch.recordArgvPaths(
            (0..<core.launch_path_count()).map { core.launch_path_at($0).toString() }
        )

        // `--metrics` (task #78): print the core's per-stage p50/p95/p99 summary on quit
        // — the winit shell's post-`run_app` report. Always observed; the report is ""
        // unless the launch enabled metrics, so it's a no-op read otherwise (and stdout
        // on a GUI launch goes to the void, harmlessly).
        metricsObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.willTerminateNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let report = self?.core.metrics_report().toString(), !report.isEmpty
                else { return }
                try? FileHandle.standardOutput.write(contentsOf: Data("\n\(report)".utf8))
            }
        }

        installInputForwarding()
    }

    /// Deferred launch work, run from the view's `onAppear` (the window + canvas exist by
    /// then — the winit shell defers its launch into `resumed()` for the same reason):
    /// the CLI's positional paths — stashed Rust-side by `apply_launch_args` in `init` —
    /// open like the winit CLI args: folder → recursive scan (honoring `--recursive` /
    /// `--no-recursive`, else the saved preference), image → its folder with the cursor
    /// on it, .zip/.7z → the archive contents. Consumed exactly once (`open_launch_paths`
    /// is a no-op on a second call).
    ///
    /// **The windowless-app gotcha lives on:** AppKit treats a bare path in `argv[1]` as
    /// a document-open launch and *suppresses the initial WindowGroup window entirely* —
    /// the app runs windowless with a live menu bar. A `-`-prefixed first argument is
    /// ignored by that machinery, so flag-first invocations (and the hidden `--pb-open
    /// <path>` alias, now parsed by the shared pb-cli surface) work today; forcing the
    /// window for bare-path launches is task #78.10. Finder-drop +
    /// `application:openURLs:` land via the open handler as before.
    func openLaunchPathIfAny() {
        guard !launchPathOpened else { return }
        launchPathOpened = true
        if core.open_launch_paths() {
            log("open_launch_paths()")
            kick() // the scan/open worker needs the pump polling (as openPaths does)
            drainEffects()
        }
        // A bare launch opens the empty state — nothing is auto-opened (owner call,
        // 2026-07-03: reversed from the brief reopen-last-folder behavior). The
        // remembered last_folder only seeds the Open dialog's start (core-side).
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
                        + "sheet=\(self.activeSheet?.rawValue ?? "none") "
                        + "key=\(self.hostWindow?.isKeyWindow == true) "
                        + "target=\(event.window === self.hostWindow)"
                )
            }
            // Keys drive the viewer only while a native panel (NSOpenPanel), an alert, or
            // a dialog sheet doesn't own the keyboard (don't swallow the password field's
            // typing!) …
            if self.panelOpen || self.alertUp || self.activeSheet != nil {
                return event
            }
            // … and only when the event is aimed at the VIEWER window. Gate on the event's
            // target window, NOT on `isKeyWindow`: key-status restoration after a panel /
            // menu / About-panel close is asynchronous, so for a moment the host window
            // isn't key while key events still target it — an `isKeyWindow` gate ate that
            // press (the owner-reported "first Esc ignored, second quits"). The target-
            // window gate still keeps other key windows to themselves: typing Space in the
            // Settings window must not advance the photo behind it (the monitor is
            // app-wide, and events there target the Settings window, not the host).
            guard let host = self.hostWindow, event.window === host else {
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
            // Keep the toolbar in step with keyboard-driven state the effect markers don't
            // cover — notably the slideshow interval (`[` / `]`), which only toasts. Discrete
            // presses only (skip OS auto-repeat) to stay off any held-key fast path.
            if event.type == .keyDown, !event.isARepeat { self.syncToolbar() }
            return event.modifierFlags.contains(.command) ? event : nil
        }

        // The focus-loss release net: held keys are cleared so nothing keeps blazing —
        // on app deactivation AND on the window losing key status (a dialog opening).
        focusObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.didResignActiveNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.forwardFocusLost("app didResignActive") }
        }
        keyLossObserver = NotificationCenter.default.addObserver(
            forName: NSWindow.didResignKeyNotification,
            object: nil,
            queue: .main
        ) { [weak self] note in
            MainActor.assumeIsolated {
                self?.forwardFocusLost(
                    "window didResignKey (\((note.object as? NSWindow)?.title ?? "?"))")
            }
        }
        // Keep the cosmetic keyboard focus on the content whenever the viewer window becomes
        // key. Otherwise AppKit/SwiftUI hands first-responder to the first toolbar control on
        // the first photo open — a blue focus ring that can't be Tabbed off (the SwiftUI-hosted
        // toolbar has no working key-view loop). Keys go through the monitor above, so pinning
        // first responder to the canvas is purely visual. Only touch the host window, and only
        // when a control other than the canvas holds focus (avoid needless churn).
        keyGainObserver = NotificationCenter.default.addObserver(
            forName: NSWindow.didBecomeKeyNotification,
            object: nil,
            queue: .main
        ) { [weak self] note in
            MainActor.assumeIsolated {
                guard let self, let window = note.object as? NSWindow, window === self.hostWindow,
                      let canvas = self.canvasView, window.firstResponder !== canvas
                else { return }
                window.makeFirstResponder(canvas)
            }
        }

        // Menu tracking guard: window-chrome writes are deferred while any menu is open
        // (see menuTrackingDepth), then re-asserted once it closes.
        menuTrackObservers.append(
            NotificationCenter.default.addObserver(
                forName: NSMenu.didBeginTrackingNotification,
                object: nil,
                queue: .main
            ) { [weak self] note in
                MainActor.assumeIsolated {
                    guard let self else { return }
                    self.menuTrackingDepth += 1
                    let title = (note.object as? NSMenu)?.title ?? "?"
                    self.log("menu didBeginTracking \"\(title)\" depth=\(self.menuTrackingDepth)")
                }
            })
        menuTrackObservers.append(
            NotificationCenter.default.addObserver(
                forName: NSMenu.didEndTrackingNotification,
                object: nil,
                queue: .main
            ) { [weak self] note in
                MainActor.assumeIsolated {
                    guard let self else { return }
                    self.menuTrackingDepth = max(0, self.menuTrackingDepth - 1)
                    let title = (note.object as? NSMenu)?.title ?? "?"
                    self.log("menu didEndTracking \"\(title)\" depth=\(self.menuTrackingDepth)")
                    // Apply any chrome / main-menu clobber that arrived while the
                    // menu was open.
                    if self.menuTrackingDepth == 0 {
                        self.assertWindowChrome()
                        self.reassertMenuBar()
                    }
                }
            })

        // Finder / Dock / "Open with" URLs (application:open:) — route through the same
        // classify-and-open path as a drop. Buffered by AppDelegate if they arrive before
        // this handler is installed (a cold double-click launch).
        AppDelegate.installOpenHandler { [weak self] urls in
            // Drop the document-open echo of a bare-path CLI launch (the same path
            // arrives via parsed argv too); real Finder opens pass through untouched.
            let fresh = Launch.filterLaunchEcho(urls)
            guard !fresh.isEmpty else { return }
            self?.openPaths(fresh.map(\.path))
        }
    }

    private func forwardFocusLost(_ why: String) {
        log("focusLost ← \(why)")
        core.focus_lost()
        drainEffects()
    }

    // MARK: - Pointer + gestures (forwarded by MetalCanvasNSView)

    /// Pointer moved over the canvas, in physical px, top-left origin (the winit convention).
    func pointerMoved(x: Float, y: Float) {
        core.pointer_moved(x, y)
        // Wake the pump: a move over the bottom controls zone arms the video-line flash
        // (`video_hover_reveal`), which only surfaces as `videoControlsVisible` when `pump()`
        // reconciles. During native playback the pump is otherwise paused (AVPlayer composites
        // itself), so without this a hover would never reveal the controls. The pump re-pauses
        // itself once the flash decays.
        kick()
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
        // SwiftUI keeps rewriting the menu bar on scene updates — not just at launch
        // (the F-mode toggle was the repro: its observable flip re-evaluates the window
        // scene). Crucially it guts the installed NSMenu IN PLACE (--pb-f-smoke showed
        // NSApp.mainMenu keeping its identity while our items vanished), so the
        // reliable signal is our menu losing items — watch for that and win the bar
        // back. The async hop defers past SwiftUI's in-flight mutation; reassert's
        // intact check makes the re-install converge (our own build only ADDS items).
        menuClobberObserver = NotificationCenter.default.addObserver(
            forName: NSMenu.didRemoveItemNotification,
            object: nil,
            queue: .main
        ) { [weak self] note in
            MainActor.assumeIsolated {
                guard let self, let menu = note.object as? NSMenu,
                    menu === self.menuBar?.currentMenu
                else { return }
                DispatchQueue.main.async {
                    MainActor.assumeIsolated { self.reassertMenuBar() }
                }
            }
        }
        // Belt-and-braces for the other clobber mechanism (a wholesale replacement of
        // NSApp.mainMenu, should some SwiftUI version switch to it).
        mainMenuObservation = NSApp.observe(\.mainMenu) { [weak self] _, _ in
            DispatchQueue.main.async {
                MainActor.assumeIsolated { self?.reassertMenuBar() }
            }
        }
    }

    @ObservationIgnored private var mainMenuObservation: NSKeyValueObservation?
    @ObservationIgnored private var menuClobberObserver: NSObjectProtocol?

    /// Re-assert the native menu bar unless a menu is open — replacing the main menu
    /// mid-tracking snaps the open menu shut (the `assertWindowChrome` hazard, same
    /// guard); a clobber that lands while a menu is up is re-applied by the
    /// didEndTracking observer.
    private func reassertMenuBar() {
        guard menuTrackingDepth == 0 else { return }
        if menuBar?.reassert() == true {
            // The rebuild starts from defaults — re-apply the live checks/enables.
            menuBar?.sync(core.menu_state())
            log("main menu was clobbered — rebuilt + reinstalled ours")
        }
    }

    /// `--pb-f-smoke` (dev diagnostics): drive the F-mode round trip from inside the
    /// app — toggle borderless fullscreen on, off, then dump the REAL state of
    /// `NSApp.mainMenu` (identity + top-level titles) at each step and quit. Exercises
    /// the exact scene-update path of the owner-reported menu-bar reset without
    /// needing synthetic key events or Accessibility permission.
    func runFSmokeIfRequested() {
        guard ProcessInfo.processInfo.arguments.contains("--pb-f-smoke"),
            !fSmokeStarted // onAppear refires on scene updates — arm exactly once
        else { return }
        fSmokeStarted = true
        func dump(_ tag: String) {
            // A top-level holder's own title is often empty — display comes from the
            // submenu's title, so print that.
            let titles = (NSApp.mainMenu?.items ?? [])
                .map { $0.submenu?.title ?? $0.title }
                .joined(separator: " | ")
            log(
                "F-SMOKE \(tag): ours=\(menuBar?.isInstalled ?? false) items=[\(titles)]")
        }
        func after(_ secs: Double, _ body: @escaping @MainActor () -> Void) {
            DispatchQueue.main.asyncAfter(deadline: .now() + secs) {
                MainActor.assumeIsolated(body)
            }
        }
        dump("start")
        after(1.5) { [self] in
            log("F-SMOKE toggling F on")
            menuAction("fullscreen")
            dump("after-on-sync")
            after(1.5) { [self] in
                dump("in-fullscreen")
                log("F-SMOKE toggling F off")
                menuAction("fullscreen")
                dump("after-off-sync")
                after(1.5) {
                    dump("final")
                    NSApp.terminate(nil)
                }
            }
        }
    }

    @ObservationIgnored private var fSmokeStarted = false

    /// A native menu item fired (by stable Action id) — same dispatch as the keyboard.
    /// Menu bar AND toolbar clicks both land here (mouse-driven), so it's where we mark a
    /// Fullscreen toggle as mouse-initiated (for the exit hint) and re-sync the toolbar.
    func menuAction(_ id: String) {
        if id == "fullscreen" { fullscreenHintFromMouse = true }
        core.menu_action(id)
        kick()
        drainEffects()
        syncToolbar()
    }

    /// A toolbar nav/random button was pressed and **held** — begin hold-to-blaze (task #55).
    /// The initial advance runs now; `kick()` keeps the pump ticking so it blazes each frame
    /// (the tick's `held_nav()` sees the pointer hold) until `endPointerNav`. Same machinery a
    /// held Space key uses.
    func beginPointerNav(_ actionId: String) {
        core.begin_pointer_nav(actionId)
        kick()
        drainEffects()
    }

    /// The held toolbar button was released — stop blazing (the pump re-pauses once idle).
    func endPointerNav() {
        core.end_pointer_nav()
        kick()
        drainEffects()
    }

    // MARK: - The window toolbar (task #55)

    /// The mouse-driven toolbar (nav / view / panel affordances). AppKit, like the menu bar —
    /// its buttons fire the same Action ids through `menuAction`, and `syncToolbar` mirrors
    /// the live `MenuState` onto it. Installed once the window exists (`attachCanvas`).
    @ObservationIgnored private var toolbarController: ToolbarController?
    /// The deferred `window.toolbar` assignment has actually run — until it has, the
    /// per-drain clobber re-assert must stay its hand (it would set `window.toolbar`
    /// synchronously, re-triggering the very crash the defer avoids).
    @ObservationIgnored private var toolbarInstalled = false

    func installToolbarIfNeeded() {
        guard toolbarController == nil, let window = hostWindow else { return }
        let tc = ToolbarController(model: self)
        toolbarController = tc
        // Defer the `window.toolbar =` assignment off the window-realization callstack.
        // `installToolbarIfNeeded` runs inside `attachCanvas` ← `viewDidMoveToWindow` ←
        // SwiftUI's `addSubview:` while it's still building the WindowGroup window (and its
        // own `.toolbarBackground` toolbar surface). Setting the toolbar *there* mutates it
        // re-entrantly mid-realization and crashes in `-[NSToolbar _loadAllPlaceholderItems]`
        // (SIGTRAP). One runloop hop lands the install after the window is fully realized.
        DispatchQueue.main.async { [weak self, weak window] in
            guard let self, let window else { return }
            tc.install(on: window)
            self.toolbarInstalled = true
            self.syncToolbar()
        }
    }

    /// Push the live `MenuState` (+ the folder-tree visibility, which lives outside it) to the
    /// toolbar — the toolbar twin of `menuBar?.sync`. Called on `MenuStateChanged` and
    /// `PanelsChanged`, the same markers that re-sync the menu bar / panels.
    private func syncToolbar() {
        let menu = core.menu_state()
        toolbarController?.sync(
            menu,
            treeVisible: treeVisible,
            slideshowInterval: core.slideshow_interval_display().toString(),
            hasMotion: core.current_has_motion(),
            playing: core.motion_playing()
        )
        // Mirror the scale mode (8/9/0 / the View menu) onto a playing video so it fits/
        // crops like a still. Cheap; re-lays-out only on a real change.
        nativeVideo?.setScaleMode(menu.scale)
    }

    /// The last motion state pushed to the toolbar's Play-Animation button — so the pump
    /// re-syncs it only on a real change (a new item under hold-to-blaze, or playback ending
    /// on its own), not every frame.
    @ObservationIgnored private var lastHasMotion = false
    @ObservationIgnored private var lastPlaying = false

    /// Set when a Fullscreen toggle arrives via the toolbar/menu (mouse) rather than the F
    /// key — drives the one-shot "Press F to exit" hint on entering the borderless speed mode
    /// (a keyboard user who pressed F doesn't need telling). Read + cleared in `SetWindowMode`.
    @ObservationIgnored private var fullscreenHintFromMouse = false

    /// The titlebar filename, driven through SwiftUI's `.navigationTitle` (see `ContentView`).
    var windowTitleText = "Blaze Viewer"
    /// The titlebar subtitle ("N of M"), driven through SwiftUI's `.navigationSubtitle`.
    var windowSubtitleText = ""

    /// Split the core's `name (idx/n)` window title into a clean filename title + an
    /// "N of M" subtitle (the Preview-on-Tahoe unified-toolbar look). A title that doesn't
    /// carry the trailing counter (the "PhotoBlaze" empty state) just sets the title and
    /// clears the subtitle. The counter is matched only at the very end, so a filename that
    /// itself contains " (…)" isn't mis-split.
    ///
    /// ⚠ These flow through **SwiftUI** `.navigationTitle`/`.navigationSubtitle` (observed
    /// properties), NOT a direct `window.title`/`window.subtitle` write. SwiftUI owns the
    /// `WindowGroup` titlebar and repaints over AppKit-side title writes on its update passes —
    /// an AppKit `window.subtitle` set gets silently cleared (and the title races back to the
    /// WindowGroup's "PhotoBlaze"). Letting SwiftUI own it makes both stick.
    private func applyWindowTitle(_ full: String) {
        if let m = Self.titleCounter.firstMatch(
            in: full, range: NSRange(full.startIndex..., in: full)),
            let nameRange = Range(m.range(at: 1), in: full),
            let idxRange = Range(m.range(at: 2), in: full),
            let nRange = Range(m.range(at: 3), in: full)
        {
            windowTitleText = String(full[nameRange])
            let idx = Self.grouped(String(full[idxRange]))
            let n = Self.grouped(String(full[nRange]))
            let count = "\(idx) of \(n)"
            // Prepend the immediate parent folder when the photo is a real on-disk file
            // ("Pictures · 2 of 147"); archive entries have no path, so just the count.
            let folder = folderLabel(for: core.current_photo_path().toString())
            windowSubtitleText = folder.isEmpty ? count : "\(folder)  ·  \(count)"
        } else {
            windowTitleText = full
            windowSubtitleText = ""
        }
    }

    /// The immediate parent folder's display name for a photo's full path — the subtitle's
    /// folder chip. Empty for an archive entry or the empty deck (`current_photo_path()` is
    /// empty), which drops the chip and shows just the counter. RAM-only, like the filename
    /// title and proxy icon (no persisted viewing trace — privacy #2 holds).
    private func folderLabel(for path: String) -> String {
        guard !path.isEmpty else { return "" }
        return URL(fileURLWithPath: path).deletingLastPathComponent().lastPathComponent
    }

    /// `^(.*) \((\d+)/(\d+)\)$` — matches the `engine::title_for` format, capturing the name,
    /// the 1-based index, and the total.
    private static let titleCounter = try! NSRegularExpression(pattern: #"^(.*) \((\d+)/(\d+)\)$"#)

    /// A number string with the locale's thousands grouping ("1204" → "1,204").
    private static func grouped(_ digits: String) -> String {
        guard let value = Int(digits) else { return digits }
        return Self.groupingFormatter.string(from: NSNumber(value: value)) ?? digits
    }

    private static let groupingFormatter: NumberFormatter = {
        let f = NumberFormatter()
        f.numberStyle = .decimal
        return f
    }()

    /// Re-pull the native Help panel model after a `PanelsChanged` marker: its
    /// visibility and (when visible) its rows, flattened from the core's live keymap.
    private func refreshHelp() {
        core.help_refresh()
        let vis = core.help_visible()
        if vis != helpVisible {
            withAnimation(Layout.chromeFade) { helpVisible = vis }
        }
        guard vis else {
            // Keep the last sections so the sheet holds its size while it fades out (same
            // no-collapse rule as the corner panels); rebuilt on the next open.
            return
        }
        // The core hands rows as a flat (isHeader, text, shortcut) list; regroup into
        // sections (a header starts a new one) for the two-column section layout.
        let n = Int(core.help_row_count())
        var sections: [HelpSection] = []
        var title = ""
        var items: [HelpItem] = []
        var open = false
        var itemId = 0
        func flush() {
            if open {
                sections.append(HelpSection(id: sections.count, title: title, items: items))
            }
        }
        for i in 0..<n {
            let idx = UInt(i)
            if core.help_row_is_header(idx) {
                flush()
                title = core.help_row_text(idx).toString()
                items = []
                open = true
            } else {
                items.append(
                    HelpItem(
                        id: itemId,
                        label: core.help_row_text(idx).toString(),
                        shortcut: core.help_row_shortcut(idx).toString()
                    ))
                itemId += 1
            }
        }
        flush()
        helpSections = sections
    }

    /// Close the Help panel from its ✕ button — the same toggle the `?` key drives, so
    /// state stays in the core (and the menu checkmark follows).
    func closeHelp() {
        menuAction("help")
    }

    /// Re-pull the native Inspector after a `PanelsChanged` marker: visibility, the
    /// selected tab, and (when visible) the active tab's rows flattened by the core.
    private func refreshInspector() {
        let vis = core.inspector_visible()
        if vis != inspectorVisible {
            withAnimation(Layout.chromeFade) { inspectorVisible = vis }
        }
        guard vis else {
            // Deliberately keep the last rows: the card holds its full height while it
            // fades out. Emptying them here collapses the content to one line mid-fade —
            // the "info hides before the header" jank on Tab. They're repopulated below on
            // the next open (synchronously, before it paints), so there's no stale flash.
            return
        }
        inspectorTab = Int(core.inspector_tab())
        core.inspector_refresh()
        let n = Int(core.inspector_row_count())
        var rows: [InspectorRow] = []
        rows.reserveCapacity(n)
        for i in 0..<n {
            let idx = UInt(i)
            rows.append(
                InspectorRow(
                    id: i,
                    kind: Int(core.inspector_row_kind(idx)),
                    a: core.inspector_row_a(idx).toString(),
                    b: core.inspector_row_b(idx).toString()
                ))
        }
        inspectorRows = rows
    }

    /// Switch the Inspector to a tab from its tab bar (0 Details / 1 Text / 2 Describe).
    /// Opens it there (never toggles closed); `kick()` runs a tick so the change signals.
    func showInspectorTab(_ tab: Int) {
        core.inspector_show_tab(UInt8(tab))
        kick()
    }

    /// Close the Inspector from its ✕ button.
    func closeInspector() {
        core.inspector_close()
        kick()
    }

    /// Copy the whole active Inspector tab to the clipboard (the header ⧉ button) — routes to
    /// the core's existing per-tab copy command (details / recognized text / description),
    /// which writes the clipboard and shows the confirming toast.
    func copyInspectorTab() {
        switch inspectorTab {
        case 1: menuAction("copy_text")  // Action::CopyImageText's id is "copy_text"
        case 2: menuAction("copy_description")
        default: menuAction("copy_image_details")
        }
    }

    /// The active tab's copy tooltip / accessibility label.
    var inspectorCopyLabel: String {
        switch inspectorTab {
        case 1: return "Copy all text"
        case 2: return "Copy description"
        default: return "Copy details"
        }
    }

    /// Re-pull the native folder tree after a `PanelsChanged` marker: visibility and (when
    /// visible) the current folder's hierarchy rows, as derived by the core.
    private func refreshTree() {
        let vis = core.tree_visible()
        if vis != treeVisible {
            withAnimation(Layout.chromeFade) { treeVisible = vis }
        }
        guard vis else {
            // Keep the last rows/path so the panel holds its height through the fade-out
            // (emptying collapses it to just the "Folders" header mid-fade). Repopulated
            // on the next reveal before it paints — no stale flash.
            return
        }
        treeUsesFs = core.tree_uses_fs()
        currentTreePath = core.tree_current_path().toString()
        core.tree_refresh()
        let n = Int(core.tree_row_count())
        var rows: [FolderTreeRow] = []
        rows.reserveCapacity(n)
        for i in 0..<n {
            let idx = UInt(i)
            rows.append(
                FolderTreeRow(
                    id: i,
                    depth: Int(core.tree_row_depth(idx)),
                    name: core.tree_row_name(idx).toString(),
                    isCurrent: core.tree_row_is_current(idx),
                    isUp: core.tree_row_is_up(idx),
                    hasChildren: core.tree_row_has_children(idx),
                    expanded: core.tree_row_expanded(idx),
                    loading: core.tree_row_loading(idx),
                    count: Int(core.tree_row_count_badge(idx)),
                    hasTarget: core.tree_row_has_target(idx)
                ))
        }
        treeRows = rows
    }

    /// Open a folder-tree row (a name click): load its photos / re-scope the archive.
    func activateTreeRow(_ i: Int) {
        core.tree_activate(UInt(i))
        kick()
    }

    /// Toggle a Finder-tree row's expansion (the chevron) — browsing only, no photo load.
    func toggleTreeRow(_ i: Int) {
        core.tree_toggle(UInt(i))
        kick()
    }

    /// Close the folder tree from its ✕ — the same toggle ⇧F drives (menu checkmark follows).
    func closeTree() {
        menuAction("folder_tree")
    }

    // MARK: - Thumbnails strip (task #83)

    /// Re-pull the strip's model after a `PanelsChanged` marker: visibility, the
    /// active left-pane tab, counts, the highlight, and any pending follow-scroll.
    private func refreshThumbs() {
        let vis = core.thumbs_visible()
        leftTab = Int(core.left_tab())
        if vis != thumbsVisible {
            withAnimation(Layout.chromeFade) { thumbsVisible = vis }
            if !vis {
                thumbImages.removeAll() // free the CGImage cache with the tab
            }
        }
        guard vis else { return }
        thumbCount = Int(core.thumb_count())
        thumbCurrent = Int(core.thumb_current())
        thumbDirty = core.thumb_dirty()
        let item = Int(core.thumb_scroll_item())
        if item >= 0 {
            thumbScrollItem = item
            thumbScrollGen = core.thumb_scroll_gen()
            core.take_thumb_scroll()
            thumbScrollSeq &+= 1
        }
    }

    /// The cell's thumb as an NSImage, built from the store's RGBA8 bytes and
    /// cached per store generation — an unchanged cell transfers nothing.
    func thumbImage(_ i: Int) -> NSImage? {
        let gen = core.thumb_gen(UInt(i))
        if gen == 0 { return nil }
        if let cached = thumbImages[i], cached.gen == gen { return cached.image }
        let w = Int(core.thumb_width(UInt(i)))
        let h = Int(core.thumb_height(UInt(i)))
        let rgba = core.thumb_rgba(UInt(i))
        guard w > 0, h > 0, rgba.len() == w * h * 4 else { return nil }
        let data = Data(bytes: UnsafeRawPointer(rgba.as_ptr()), count: rgba.len())
        guard let provider = CGDataProvider(data: data as CFData),
              let cg = CGImage(
                  width: w, height: h,
                  bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: w * 4,
                  space: CGColorSpace(name: CGColorSpace.sRGB)!,
                  bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.last.rawValue),
                  provider: provider, decode: nil, shouldInterpolate: true,
                  intent: .defaultIntent)
        else { return nil }
        let image = NSImage(cgImage: cg, size: NSSize(width: w, height: h))
        thumbImages[i] = (gen, image)
        if thumbImages.count > 96 {
            // Keep the 64 nearest the pull point — the visible+overscan working set.
            let keep = Set(thumbImages.keys.sorted { abs($0 - i) < abs($1 - i) }.prefix(64))
            thumbImages = thumbImages.filter { keep.contains($0.key) }
        }
        return image
    }

    /// Pull the subtitle overlay if it changed (task #90). Runs every tick, so the fast
    /// path — a cue that's still on screen — must be a single `u64` read and two compares.
    ///
    /// The rect is refreshed even when the generation didn't change, because the core
    /// bumps the generation on a move as well as a repaint; the pixels are only pulled on
    /// a real change.
    private func syncSubtitle() {
        let gen = core.subtitle_gen()
        if gen == subtitleGen { return }
        subtitleGen = gen

        let r = core.subtitle_rect()
        let w = Int(core.subtitle_width())
        let h = Int(core.subtitle_height())
        guard gen != 0, r.valid, w > 0, h > 0 else {
            if subtitleImage != nil { subtitleImage = nil }
            subtitleRect = .zero
            return
        }
        subtitleRect = CGRect(x: CGFloat(r.x), y: CGFloat(r.y), width: CGFloat(r.w), height: CGFloat(r.h))

        let rgba = core.subtitle_rgba()
        guard rgba.len() == w * h * 4 else { return }
        let data = Data(bytes: UnsafeRawPointer(rgba.as_ptr()), count: rgba.len())
        guard let provider = CGDataProvider(data: data as CFData),
              let cg = CGImage(
                  width: w, height: h,
                  bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: w * 4,
                  space: CGColorSpace(name: CGColorSpace.sRGB)!,
                  // Premultiplied — unlike `thumbImage`'s straight alpha. The rasterizer
                  // composites outline/shadow/background itself and emits premultiplied
                  // pixels; reading them as straight alpha halos every glyph edge.
                  bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue),
                  provider: provider, decode: nil, shouldInterpolate: false,
                  intent: .defaultIntent)
        else { return }
        // Size in *points* = the core's rect, so the extra pixels land on the Retina grid
        // instead of being thrown away — this is what makes the text sharp.
        subtitleImage = NSImage(cgImage: cg, size: subtitleRect.size)
    }

    func thumbName(_ i: Int) -> String { core.thumb_name(UInt(i)).toString() }
    func thumbBadge(_ i: Int) -> Int { Int(core.thumb_badge(UInt(i))) }
    func thumbRotation(_ i: Int) -> Int { Int(core.thumb_rotation(UInt(i))) }
    func thumbFailed(_ i: Int) -> Bool { core.thumb_failed(UInt(i)) }

    /// A cell click: absolute jump + the instant thumb-preview present.
    func thumbClick(_ i: Int) {
        core.thumb_click(UInt(i))
        kick()
    }

    /// The strip's realized-cell span → the core's demand window (fills + pinning).
    func thumbsSetViewport(_ visLo: Int, _ visHi: Int, _ overLo: Int, _ overHi: Int) {
        core.thumbs_set_viewport(
            UInt(max(0, visLo)), UInt(max(0, visHi)),
            UInt(max(0, overLo)), UInt(max(0, overHi)))
        kick()
    }

    /// The user grabbed the list — detach auto-follow until the next nav/click.
    func thumbsUserScrolled() {
        core.thumbs_user_scrolled()
    }

    /// Our follow-scroll animation for `gen` landed.
    func thumbsScrollDone(_ gen: UInt64) {
        core.thumbs_scroll_done(gen)
    }

    /// Switch the left pane's tab (the tab-bar click): 0 = Folders, 1 = Thumbnails.
    /// Rides the same actions as the keyboard (⇧F / ⇧T), so the semantics match.
    func showLeftTab(_ tab: Int) {
        if tab == 0 && leftTab != 0 {
            menuAction("folder_tree")
        } else if tab == 1 && leftTab != 1 {
            menuAction("thumbnails")
        }
    }

    func closeThumbs() {
        menuAction("thumbnails")
    }

    // MARK: - Empty-state panel actions (task #54)

    /// Open File / Open Folder from the welcome surface — the same commands as the menu
    /// and the O / ⇧O keys (open state stays in the core).
    func openFile() { menuAction("open_file") }
    func openFolder() { menuAction("open_folder") }

    /// The user-facing key label for an action by id ("next", "open_file", …) — for the
    /// welcome surface's shortcut tips. A generic lookup, so new tips need no new FFI.
    func shortcut(_ id: String) -> String {
        core.action_shortcut(id).toString()
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
        hasImage: Bool, hasMotion: Bool, canReveal: Bool, fullscreen: Bool,
        comparePinned: Bool, comparePinnedHere: Bool
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
        menu.addItem(.separator())
        // Flicker compare (task #43): the pin item flips to its unpin reading on the
        // pinned photo; the flip appears only once a pin exists (menu-bar parity).
        add("compare_pin", comparePinnedHere ? "Unpin from Compare" : "Pin for Compare")
        if comparePinned {
            add("compare_toggle", "Compare with Pinned")
        }
        if hasMotion {
            add("play_pause", "Play/Pause")
        }
        menu.addItem(.separator())
        add("slideshow", "Start/Stop Slideshow")
        menu.addItem(.separator())
        add("copy", "Copy Image")
        add("copy_path", "Copy File Path")
        add("copy_image_details", "Copy Image Details")
        add("copy_text", "Copy Text from Image")
        menu.addItem(.separator())
        add("describe", "Describe Image")
        add("ask_image", "Ask About Image…")
        add("copy_description", "Copy AI Description")
        if canReveal {
            add("reveal", "Show in Finder")
        }
        menu.addItem(.separator())
        add("fullscreen", fullscreen ? "Exit Quick Full Screen" : "Enter Quick Full Screen")
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
        log("ShowDialog(\(kind))")
        switch kind {
        case "about":
            NSApp.activate()
            NSApp.orderFrontStandardAboutPanel(options: Self.aboutPanelOptions())
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
        case "ask_image":
            askEntry = "" // each Ask starts blank
            activeSheet = .ask
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

    /// Extras for the standard About panel, matching the egui About card's content:
    /// the git build stamp (PBBuildID from Info.plist, stamped by build-swift-host.sh)
    /// shown as "Version X.Y.Z (hash)", and a credits block with the tagline and a
    /// clickable GitHub link. Name, icon, version, and copyright come from the bundle.
    private static func aboutPanelOptions() -> [NSApplication.AboutPanelOptionKey: Any] {
        var options: [NSApplication.AboutPanelOptionKey: Any] = [:]
        if let build = Bundle.main.object(forInfoDictionaryKey: "PBBuildID") as? String,
            !build.isEmpty
        {
            options[.version] = build
        }
        let center = NSMutableParagraphStyle()
        center.alignment = .center
        let credits = NSMutableAttributedString(
            string: "An ultra-fast image viewer\n\n",
            attributes: [
                .font: NSFont.systemFont(ofSize: NSFont.smallSystemFontSize),
                .foregroundColor: NSColor.labelColor,
                .paragraphStyle: center,
            ])
        credits.append(
            NSAttributedString(
                string: "blazeviewer.app",
                attributes: [
                    .font: NSFont.systemFont(ofSize: NSFont.smallSystemFontSize),
                    .link: URL(string: "https://blazeviewer.app")!,
                    .paragraphStyle: center,
                ]))
        options[.credits] = credits
        return options
    }

    /// The delete Confirm (`ShowDialog("confirm")`), styled for Finder parity (owner
    /// request): `.critical` = the caution triangle badged with the app icon, the
    /// message's first line as the bold headline and the rest as the informative text,
    /// destructive Delete + Cancel on Esc. The answer returns via
    /// `dialog_confirm_answered` and the core runs (or forgets) the armed delete.
    private func presentConfirmAlert(_ message: String) {
        let alert = NSAlert()
        let lines = message.split(separator: "\n", maxSplits: 1)
        alert.messageText = lines.first.map(String.init) ?? message
        if lines.count > 1 {
            alert.informativeText = String(lines[1])
        }
        alert.alertStyle = .critical
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

    /// Ask / ⌘Return: submit the question; the core runs it through the describe backend and
    /// shows the answer in the description panel (the CloseDialog effect dismisses the sheet).
    func askSubmit() {
        let q = askEntry.trimmingCharacters(in: .whitespacesAndNewlines)
        askEntry = ""
        guard !q.isEmpty else { return }
        core.ask_submitted(q)
        drainEffects()
    }

    /// Ask Cancel / Esc: close the prompt without asking.
    func askCancel() {
        askEntry = ""
        core.dialog_dismissed()
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

    /// The ambient scan pill's (④) Cancel — stop the walk but keep everything that already
    /// streamed in (a "Scan stopped" toast confirms). Refresh so the pill hides at once.
    func scanPillCancel() {
        core.scan_pill_cancel()
        scanPillVisible = false
        drainEffects()
    }

    /// Show a native toast (the shell renders it). Wakes the pump so it appears even from idle.
    func toast(_ msg: String) {
        core.toast(msg)
        kick()
    }

    // ── The native play hint's shell-owned fade / hover / click ──

    private func showPlayHint() {
        playHintVisible = true
        schedulePlayHintFade()
    }

    /// Auto-hide after 3s — but only if the pointer isn't holding it open.
    private func schedulePlayHintFade() {
        playHintFadeTask?.cancel()
        playHintFadeTask = Task { @MainActor [weak self] in
            try? await Task.sleep(for: .seconds(3))
            guard let self, !Task.isCancelled, !self.playHintHovered else { return }
            self.playHintVisible = false
        }
    }

    private func hidePlayHint() {
        playHintFadeTask?.cancel()
        playHintFadeTask = nil
        playHintVisible = false
    }

    /// Hover holds the hint open (cancels the fade); leaving restarts the 3s countdown.
    func playHintHover(_ hovering: Bool) {
        playHintHovered = hovering
        if hovering {
            playHintFadeTask?.cancel()
        } else if playHintVisible {
            schedulePlayHintFade()
        }
    }

    /// Click the hint → play (same as the P key); dismiss it since it's done its job.
    func triggerPlay() {
        hidePlayHint()
        menuAction("play_pause")
    }

    // ── The "Press F to exit fullscreen" hint (task #55) ──

    /// Shown for ~6s when the borderless speed mode is entered **by mouse** (toolbar/menu) —
    /// a keyboard user who pressed the key doesn't need it. Rendered natively with the keycap
    /// pill (not a toast), so the key reads as a `⌗`-style cap; the shown key is the live
    /// primary binding (`F` by default). Driven by `SetWindowMode`.
    private(set) var fullscreenHintVisible = false
    @ObservationIgnored private var fullscreenHintTask: Task<Void, Never>?

    private func showFullscreenHint() {
        fullscreenHintVisible = true
        fullscreenHintTask?.cancel()
        fullscreenHintTask = Task { @MainActor [weak self] in
            try? await Task.sleep(for: .seconds(6))
            guard let self, !Task.isCancelled else { return }
            self.fullscreenHintVisible = false
        }
    }

    private func hideFullscreenHint() {
        fullscreenHintTask?.cancel()
        fullscreenHintTask = nil
        fullscreenHintVisible = false
    }

    /// Map the core's semantic toast icon (0 = none, see `ToastIcon`) to an SF Symbol, or nil
    /// for a text-only toast.
    func toastSymbol(_ icon: Int) -> String? {
        switch icon {
        case 1: return "speaker.slash.fill"  // Mute
        case 2: return "speaker.wave.1.fill"  // Unmute
        case 3: return "photo.badge.checkmark"  // Save (rotation)
        case 4: return "arrow.uturn.backward"  // Undo
        case 5: return "trash.fill"  // Delete (permanent)
        case 6: return "trash"  // Recycle (recoverable)
        case 7: return "pin.fill"  // Pin
        case 8: return "pin.slash.fill"  // Unpin
        case 9: return "rotate.left"  // Rotate CCW
        case 10: return "rotate.right"  // Rotate CW
        case 11: return "doc.on.doc"  // Copy
        case 12: return "captions.bubble.fill"  // Captions on
        case 13: return "captions.bubble"  // Captions off
        case 14: return "speaker.wave.2.fill"  // Audio track switched
        case 15: return "speaker.slash.fill"  // Audio track switch refused
        default: return nil  // None
        }
    }

    // MARK: - Settings (NS2 item 5)

    /// The current settings as the flat form the Settings window binds to.
    func settingsForm() -> SettingsFormFfi {
        core.settings_form()
    }

    /// The current image's containing folder ("" = nothing open / an archive entry) —
    /// the Settings window shows it, grayed, as the unpinned "Open files in" default.
    func currentImageFolder() -> String {
        let p = core.current_photo_path().toString()
        guard !p.isEmpty else { return "" }
        return (p as NSString).deletingLastPathComponent
    }

    /// A live edit from the auto-saving Settings window: fold + clamp Rust-side; the
    /// core applies + persists only when something actually changed (an unchanged form
    /// is a no-op, so the window's load echo and close-time flush cost nothing).
    func settingsEdited(_ form: SettingsFormFfi) {
        core.settings_edited(form)
        // Keep the app-wide chrome (menus, Settings window, sheets) on the chosen
        // theme too (#46) — the canvas re-reports the resulting effective appearance.
        applyAppearancePreference()
        refreshPanelOpacity()  // a live "Panel opacity" drag updates the panels at once
        refreshGlassToolbar()  // a "Transparent toolbar" flip re-chromes the window (#59)
        kick()
        drainEffects()  // → assertWindowChrome applies the glass chrome + inset
    }

    /// Pull the shared panel opacity from the core (0.5–1.0). Called on load + on settings edits.
    func refreshPanelOpacity() {
        panelOpacity = Double(core.panel_opacity()) / 100.0
    }

    /// Apply the Appearance preference (#46) to the whole app: forced Light/Dark set
    /// an `NSApp.appearance` override (so native chrome matches the HUD); System
    /// clears it, letting the OS theme through. The canvas's
    /// `viewDidChangeEffectiveAppearance` then reports the resulting effective theme
    /// back to the core, which keeps `Appearance: System` resolving live.
    func applyAppearancePreference() {
        // Effective, not the raw saved form: a `--theme` launch override (task #78) must
        // wear from the first frame. An explicit Settings change clears the override
        // core-side, so the dialog keeps working; `settings_form()` stays raw for editing.
        switch core.effective_appearance() {
        case 1: NSApp.appearance = NSAppearance(named: .aqua)
        case 2: NSApp.appearance = NSAppearance(named: .darkAqua)
        default: NSApp.appearance = nil
        }
    }

    /// The effective light/dark appearance changed (OS switch, or our own override
    /// landing) — the core re-resolves the preference and re-themes on a real flip.
    func osThemeChanged(dark: Bool) {
        core.os_theme_changed(dark)
        drainEffects()
    }

    /// The Settings window closed (⌘W / traffic light / Esc). Edits were already
    /// applied live; this clears the core's dialog-open state and drops the
    /// Shortcuts draft.
    func settingsClosed() {
        core.settings_closed()
        drainEffects()
    }

    // MARK: - The Shortcuts editor (NS2.6) — thin wrappers over the Rust draft

    struct ShortcutCommand: Identifiable {
        let id: String
        let label: String
        /// The menu bar's own ⌘-accelerator ("" = none) — shown as a read-only hint;
        /// it lives in the menu, not the keymap, so the editor can't rebind it.
        let menuChord: String
    }

    struct ShortcutGroup: Identifiable {
        let title: String
        let commands: [ShortcutCommand]
        var id: String { title }
    }

    /// Begin editing (draft = the live keymap). Called when the Settings window opens.
    // ---- The Subtitles settings tab (task #90.4) ----------------------------------
    //
    // Its own pull/push pair, deliberately not folded into the 37-field settings form —
    // the live preview needs the DRAFT style every slider tick, and it debounces on its
    // own schedule. See `SubtitlesPane`.

    func subtitleStyleForm() -> SubtitleStyleFfi {
        core.subtitle_style_form()
    }

    func subtitleStyleEdited(_ form: SubtitleStyleFfi) {
        core.subtitle_style_edited(form)
    }

    /// "Always show forced subtitles" (task #99) — behaviour, so it rides its own pair
    /// rather than `SubtitleStyleFfi` (which drives the preview swatch). No debounce: one
    /// click, one write, and Rust hard no-ops when the value is unchanged.
    func forcedSubtitles() -> Bool {
        core.forced_subtitles()
    }

    func setForcedSubtitles(_ on: Bool) {
        core.set_forced_subtitles(on)
    }

    /// The preview swatch as an `NSImage`, drawn by Rust with the **same** rasterizer and
    /// placement math the real overlay uses — so it cannot drift from what a film shows.
    ///
    /// `nil` while the font system is still building (261 ms, on a worker): the pane shows
    /// a spinner rather than an empty frame that reads as "the preview is broken".
    ///
    /// `w`/`h` are **physical pixels**. Rasterizing at a logical size and letting the layer
    /// scale it up is what makes text blurry, and this project's known sharp edge is
    /// exactly that (a 1× ultrawide beside 2× Studios).
    func subtitlePreviewImage(_ form: SubtitleStyleFfi, _ w: Int, _ h: Int) -> NSImage? {
        guard w > 0, h > 0 else { return nil }
        let rgba = core.subtitle_preview_rgba(form, UInt32(w), UInt32(h))
        guard rgba.len() == w * h * 4 else { return nil }
        let data = Data(bytes: UnsafeRawPointer(rgba.as_ptr()), count: rgba.len())
        guard let provider = CGDataProvider(data: data as CFData),
              let cg = CGImage(
                  width: w, height: h,
                  bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: w * 4,
                  space: CGColorSpace(name: CGColorSpace.sRGB)!,
                  bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.last.rawValue),
                  provider: provider, decode: nil, shouldInterpolate: false,
                  intent: .defaultIntent)
        else { return nil }
        return NSImage(cgImage: cg, size: NSSize(width: w, height: h))
    }

    /// The curated font list the picker offers. Indexed accessors rather than a
    /// `Vec<String>`, which does not cross back to Swift.
    func subtitleFontChoices() -> [String] {
        (0..<Int(subtitle_font_count())).map { subtitle_font_name(UInt($0)).toString() }
    }

    /// Has the font system landed? Also *starts* it, so opening the Subtitles tab spends
    /// the 261 ms while the user reads the pane rather than on a film's first cue.
    func subtitlePreviewReady() -> Bool {
        core.subtitle_preview_ready()
    }

    func keymapBeginEdit() {
        core.keymap_begin_edit()
    }

    /// The editor's sections/rows — the shared `EDITOR_GROUPS` shape, same as egui's.
    func keymapGroups() -> [ShortcutGroup] {
        (0..<Int(core.keymap_group_count())).map { g in
            ShortcutGroup(
                title: core.keymap_group_title(UInt(g)).toString(),
                commands: (0..<Int(core.keymap_group_len(UInt(g)))).map { i in
                    let id = core.keymap_action_id(UInt(g), UInt(i)).toString()
                    return ShortcutCommand(
                        id: id,
                        label: core.keymap_action_label(UInt(g), UInt(i)).toString(),
                        menuChord: core.keymap_menu_chord(id).toString()
                    )
                }
            )
        }
    }

    /// The chord glyphs in a slot ("" = unbound), from the draft.
    func keymapSlotDisplay(id: String, slot: Int) -> String {
        core.keymap_slot_display(id, UInt(slot)).toString()
    }

    /// A captured chord. False = the keymap can't express that key (stay armed).
    func keymapCapture(
        id: String, slot: Int, key: String, ctrl: Bool, shift: Bool, alt: Bool, logo: Bool
    ) -> Bool {
        core.keymap_capture(id, UInt(slot), key, ctrl, shift, alt, logo)
    }

    /// The transient "Moved ⌘C from Copy Image" note from the last capture ("" = none).
    func keymapNote() -> String {
        core.keymap_last_note().toString()
    }

    func keymapClear(id: String, slot: Int) {
        core.keymap_clear_slot(id, UInt(slot))
    }

    func keymapResetDefaults() {
        core.keymap_reset_defaults()
    }

    /// Commit the draft live (auto-save): apply + persist if a binding actually changed,
    /// and re-label the menu bar's shortcut badges to match. Called after each editor
    /// gesture (capture / clear / reset).
    func keymapCommit() {
        core.keymap_commit()
        drainEffects()
        menuBar?.refreshShortcutBadges()
    }

    /// Open dropped / Finder-opened paths (multi-select aware — the launch policy classifies).
    func openPaths(_ paths: [String]) {
        guard !paths.isEmpty else { return }
        // Take focus. A drag-drop or Finder "Open With" targets PhotoBlaze, but macOS does
        // NOT auto-activate a background app that only receives a drag/open — so without
        // this the user lands on an unfocused window and has to click before interacting.
        // Bring ourselves forward + make the window key; a no-op when already active (the
        // in-app picker path). `hostWindow` is nil only on a cold pre-window launch, where
        // applicationDidFinishLaunching already activates.
        NSApp.activate()
        hostWindow?.makeKeyAndOrderFront(nil)
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
        // Pull-based size guard: heal any missed/mistimed AppKit size or scale callback
        // BEFORE ticking, so a frame is never rendered — or left composited — at a stale
        // surface size (a no-op compare on the overwhelmingly common path).
        canvasView?.reconcileSizeIfNeeded()
        core.tick()
        drainEffects()
        // Reconcile the native video against the core's authority: if we hold a player the
        // core no longer has (a torn-down/replaced session whose StopVideo we somehow
        // missed), tear it down now — so a stale video can never keep playing behind a new
        // item. Cheap: one u64 read per tick.
        if let nv = nativeVideo, nv.sessionId != core.native_video_session_id() {
            nv.stop()
            nativeVideo = nil
        }
        // Same self-heal for the sample-buffer presenter (also a Native-proxy backend,
        // so it shares the core's native-video session authority).
        if let sbv = sampleBufferVideo, sbv.sessionId != core.native_video_session_id() {
            sbv.stop()
            sampleBufferVideo = nil
        }
        // Track the view transform: zoom/pan/rotation/scale-mode changes reach the video
        // layer here (they only touch the core's `view`, never a menu/effect). Cheap —
        // `relayout()` re-applies only when the placement actually changed.
        nativeVideo?.relayout()
        sampleBufferVideo?.relayout()
        // Keep the video container's background on the current letterbox color (theme
        // switch / Settings edit) — repaint only on a real change.
        let lb = core.effective_letterbox_rgb()
        if (nativeVideo != nil || sampleBufferVideo != nil), lb != lastVideoLetterbox {
            lastVideoLetterbox = lb
            canvasView?.setVideoLetterbox(videoLetterboxCGColor)
        }
        // Refresh the shown progress sheet from the Rust-side handles (a cheap read; the
        // pump is already running while a scan/open worker is in flight).
        if activeSheet == .loading || activeSheet == .scanning {
            let p = core.dialog_progress()
            progressFraction = Double(p.fraction)
            scanFound = Int(p.found)
            scanCurrentDir = p.current_dir.toString()
        }
        // The ambient scan pill (④): mirror the in-flight walk's live progress. Cheap reads
        // off the worker handle; `scanPillVisible` gates them (false when no scan is running).
        let pillVisible = core.scan_pill_visible()
        if pillVisible {
            scanPillName = core.scan_pill_name().toString()
            scanPillFound = Int(core.scan_pill_found())
            scanPillCurrent = core.scan_pill_current().toString()
        }
        if pillVisible != scanPillVisible {
            scanPillVisible = pillVisible
        }
        // The unified native toast — mirror the core's transient toast (cheap reads; the
        // core keeps the pump ticking while one is live, then expires it).
        let toastVis = core.toast_visible()
        if toastVis {
            toastMessage = core.toast_message().toString()
            toastIcon = Int(core.toast_icon())
            toastSeq = core.toast_seq()
        }
        if toastVis != toastVisible {
            toastVisible = toastVis
        }
        // The native one-line info readout.
        let infoVis = core.info_line_visible()
        if infoVis {
            infoLineText = core.info_line_text().toString()
            infoLineCodec = core.info_line_codec().toString()
            infoLineIsLive = core.info_line_is_live()
            infoLineIsAnimated = core.info_line_is_animated()
            infoLineIsVideo = core.info_line_is_video()
            infoLineAlign = Int(core.info_line_align())
        }
        if infoVis != infoLineVisible {
            // Its own explicit fade (was only fading as a side effect of the toast's
            // `toastBottomInset` animation) — so it matches the panels and stays smooth
            // even when nothing else on screen moves.
            withAnimation(Layout.chromeFade) { infoLineVisible = infoVis }
        }
        // Session-backed video (task #84 §8): the FFmpeg fallback renders through the
        // wgpu canvas and has no AVPlayer observer, so the pump reads its scrubber
        // state each tick (cheap FFI; the link is alive whenever a session is active).
        let sessionVideo = core.video_session_active()
        if sessionVideo != sessionVideoActive {
            sessionVideoActive = sessionVideo
            if sessionVideo { resetVideoControls() } // fresh clip — scrubber starts at 0
        }
        if sessionVideo { updateSessionVideoProgress() }
        // Session-video audio clock (task #84 §7): ~4 Hz played-position samples to
        // the core — the session's master clock while audio plays — plus a
        // scheduling top-up safety net (completion callbacks are the primary driver).
        if let sa = sessionAudio, Date().timeIntervalSince(sessionAudioSampledAt) >= 0.25 {
            sessionAudioSampledAt = Date()
            sa.topUp()
            let (state, position) = sa.sample()
            core.video_audio_clock(sa.sessionId, state, position)
        }
        // The playback row shows while a video is active — native OR session-backed —
        // and the info line is on: either the persistent `i` line or the transient
        // hover-reveal flash (armed by a pointer move over the bottom controls zone;
        // `info_line_visible()` folds both in). An in-flight scrubber drag also pins it
        // up: the drag captures the pointer, so the hover flash would decay out from
        // under the user's own knob (see `videoScrubbing`).
        let controls =
            (infoVis || videoScrubbing || videoPickerOpen)
            && (nativeVideo != nil || sampleBufferVideo != nil || sessionVideo)
        if controls != videoControlsVisible {
            withAnimation(Layout.chromeFade) { videoControlsVisible = controls }
        }
        // The picker button fills its icon when subtitles are on, so it has to be observable
        // state (a computed property calling into the core would never re-render) — the same
        // shape as `videoPlaying` above.
        if controls {
            let on = core.subtitles_on()
            if on != subtitlesOn { subtitlesOn = on }
        }
        syncSubtitle()
        // The native play hint: kind 0 = playing / a still (hide), 1/2 = a motion item. A seq
        // bump is the "fresh motion item — flash it" trigger.
        let phKind = Int(core.play_hint_kind())
        let phSeq = core.play_hint_seq()
        if phKind == 0 {
            // Hide — but DON'T zero playHintKind: the icon must stay put while the pill fades
            // out (clicking a Live Photo flips kind 1→0, and livephoto/play.fill differ in
            // width, which shifted the layout mid-fade).
            if playHintVisible { hidePlayHint() }
        } else {
            if phKind != playHintKind { playHintKind = phKind }
            if phSeq != playHintSeq {
                playHintSeq = phSeq
                showPlayHint()
            }
        }
        // Keep the toolbar's Play-Animation button in step with motion state the discrete
        // input paths miss: a new item reached under hold-to-blaze, or playback finishing on
        // its own. Cheap reads (the current item's motion is cached); full re-sync only on a
        // change. (Both accessors are cache hits here — the tick above primed them.)
        let hasMotion = core.current_has_motion()
        let playing = core.animation_playing()
        if hasMotion != lastHasMotion || playing != lastPlaying {
            lastHasMotion = hasMotion
            lastPlaying = playing
            syncToolbar()
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

    // PB_TRACE pump-load diagnostics: the main-thread pump's tick rate + cost. Before the
    // native-video `work_pending` fix (2026-07-15) this spun at the display refresh (120 Hz)
    // during OS-presented playback; this confirms it now idles, and flags any pump-duration
    // spike that could hitch presentation. Windowed ~2 s.
    @ObservationIgnored private var pumpWinStart = DispatchTime.now()
    @ObservationIgnored private var pumpWinTicks = 0
    @ObservationIgnored private var pumpWinNanos: UInt64 = 0
    @ObservationIgnored private var pumpWinMaxNanos: UInt64 = 0

    /// Fold one `pump()` wall-time into the current window; emit + reset ~every 2 s. Called
    /// from `FramePump.fire` only when `pbTraceEnabled`.
    func recordPumpTick(_ nanos: UInt64) {
        pumpWinTicks += 1
        pumpWinNanos &+= nanos
        if nanos > pumpWinMaxNanos { pumpWinMaxNanos = nanos }
        let elapsed =
            Double(DispatchTime.now().uptimeNanoseconds &- pumpWinStart.uptimeNanoseconds) / 1e9
        guard elapsed >= 2.0 else { return }
        let busyPct = Double(pumpWinNanos) / 1e9 / elapsed * 100
        pbTrace(
            String(
                format: "pump diag: %.1fs — %d ticks (%.0f/s), %.1f%% main busy, avg %.2fms max %.2fms",
                elapsed, pumpWinTicks, Double(pumpWinTicks) / elapsed, busyPct,
                Double(pumpWinNanos) / Double(pumpWinTicks) / 1e6, Double(pumpWinMaxNanos) / 1e6))
        pumpWinStart = DispatchTime.now()
        pumpWinTicks = 0
        pumpWinNanos = 0
        pumpWinMaxNanos = 0
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
        applyStartupWindowState()
        // The window exists now — stand the toolbar up on it (idempotent; a scene rebuild
        // that re-attaches the canvas won't create a second controller).
        installToolbarIfNeeded()
    }

    // MARK: - Startup window state + geometry persistence (finalize item 2)

    /// Honor the Startup setting (Fullscreen / Windowed / Remember) and the remembered
    /// windowed geometry — the settings the egui build honors that were dead here.
    ///
    /// settings.toml is the ONLY frame restorer: SwiftUI's own frame persistence is
    /// disabled below, because the two restorers fought — SwiftUI re-applied its
    /// remembered size (off from ours by exactly the title-bar height, 32 pt) ~10 ms
    /// after our restore, and the old 0.6 s re-assert then snapped it back in plain
    /// sight (the owner-reported "opens, then shortens" glitch; trace 2026-07-03).
    /// A short-lived resize guard now corrects any remaining clobber the moment it
    /// lands — before the window is meaningfully visible — then retires.
    private func applyStartupWindowState() {
        guard let window = hostWindow else {
            startupSettled = true
            return
        }
        window.isRestorable = false // settings.toml is the restorer here, not Cocoa
        // Kill SwiftUI's frame persistence: stop future saves AND delete the already-
        // stored value, so no stale remembered size re-applies on this or any later
        // launch. (Empty name = SwiftUI used scene storage instead; the guard below
        // still catches that clobber.)
        let autosave = window.frameAutosaveName
        if !autosave.isEmpty {
            _ = window.setFrameAutosaveName("")
            NSWindow.removeFrame(usingName: autosave)
        }
        let saved = savedWindowFrame()
        log(
            "startup restore: saved=\(saved.map { "\($0)" } ?? "nil") "
                + "fullscreen=\(core.startup_fullscreen()) window=\(window.frame)")
        if core.startup_fullscreen() {
            // Land F mode on the monitor the user was on: place the window at the
            // remembered windowed frame FIRST, so setWindowMode fullscreens that
            // screen — and captures the right frame to restore on an F exit — instead
            // of whatever screen SwiftUI created the window on (task #42).
            if let frame = saved {
                window.setFrame(frame, display: false)
            }
            setWindowMode(fullscreen: true)
            beginLaunchFrameGuard(target: window.frame, window: window, expectFullscreen: true)
            return
        }
        guard let frame = saved else {
            startupSettled = true
            return
        }
        window.setFrame(frame, display: true)
        beginLaunchFrameGuard(target: frame, window: window)
    }

    /// For the first ~2 s after the restore, snap any programmatic resize OR move away
    /// from the restored frame straight back (SwiftUI's launch layout re-applying its
    /// remembered size — and, with its frame autosave severed, re-CENTERING the window
    /// on the main screen, which is a pure didMove the old resize-only guard never saw:
    /// the #42 "always restarts in the middle of the primary monitor" mechanism). User
    /// interaction or a mode change ends the guard — the user's intent wins — and a
    /// correction cap keeps any unforeseen frame war finite. A launch into the F speed
    /// mode guards its fullscreen frame the same way (`expectFullscreen`).
    private func beginLaunchFrameGuard(
        target: NSRect, window: NSWindow, expectFullscreen: Bool = false
    ) {
        launchFrameCorrections = 0
        launchFrameExpectsFullscreen = expectFullscreen
        launchFrameGuards = [
            NSWindow.didResizeNotification, NSWindow.didMoveNotification,
        ].map { name in
            NotificationCenter.default.addObserver(
                forName: name, object: window, queue: .main
            ) { [weak self] _ in
                MainActor.assumeIsolated { self?.enforceLaunchFrame(target) }
            }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 2.0) { [weak self] in
            self?.endLaunchFrameGuard()
        }
    }

    private func enforceLaunchFrame(_ target: NSRect) {
        guard let window = hostWindow else {
            endLaunchFrameGuard()
            return
        }
        // The user (a mode toggle, a native-fullscreen transition, a drag) owns the
        // frame now — stand down.
        if speedModeFullscreen != launchFrameExpectsFullscreen
            || window.styleMask.contains(.fullScreen)
            || window.inLiveResize || NSEvent.pressedMouseButtons != 0
        {
            endLaunchFrameGuard()
            return
        }
        guard window.frame != target else { return }
        launchFrameCorrections += 1
        guard launchFrameCorrections <= 4 else {
            log("launch frame guard: giving up after \(launchFrameCorrections) corrections")
            endLaunchFrameGuard()
            return
        }
        log("launch frame guard: \(window.frame) → snapping back to \(target)")
        window.setFrame(target, display: true)
    }

    private func endLaunchFrameGuard() {
        for g in launchFrameGuards {
            NotificationCenter.default.removeObserver(g)
        }
        launchFrameGuards.removeAll()
        startupSettled = true // the frame is user-intent from here — geometry notes may record
    }

    @ObservationIgnored private var launchFrameGuards: [NSObjectProtocol] = []
    /// Launch settling is over — `noteWindowGeometry` may record. Until then the
    /// move/resize observers see SwiftUI's own launch placement (its scene-storage frame
    /// lands BEFORE our restore in some launches — a race, trace 2026-07-03), and
    /// recording it would clobber the loaded `settings.window` in memory — making the
    /// restore a no-op — and then the debounced save would overwrite the REAL remembered
    /// geometry on disk. That self-clobber was #42's "position sticks but size doesn't."
    @ObservationIgnored private var startupSettled = false
    @ObservationIgnored private var launchFrameCorrections = 0
    @ObservationIgnored private var launchFrameExpectsFullscreen = false

    /// The saved geometry as an AppKit frame, or nil when absent / not meaningfully on
    /// any connected screen. Stored values use winit's convention (physical px,
    /// top-left virtual-desktop origin, scaled by the monitor the window was on —
    /// shared with the egui build); AppKit wants points with a bottom-left origin
    /// relative to the primary screen.
    ///
    /// The source monitor's scale isn't recorded, so each connected screen's scale is
    /// hypothesized in turn and a candidate accepted when it lands meaningfully on the
    /// screen whose scale produced it. Dividing by the *launch* window's backing scale
    /// (the old behavior) restored a frame saved on a different-DPI monitor to the
    /// wrong place entirely on a mixed-DPI setup (1x ultrawide + 2x Studio — the #42
    /// "restarts on the wrong monitor" mechanism: the mis-scaled frame failed the
    /// visibility check and silently fell back to SwiftUI's default screen).
    private func savedWindowFrame() -> NSRect? {
        let g = core.saved_geometry()
        guard g.present else { return nil }
        let primaryTop = NSScreen.screens.first?.frame.maxY ?? 0
        for screen in NSScreen.screens {
            let scale = screen.backingScaleFactor
            let size = NSSize(width: CGFloat(g.w) / scale, height: CGFloat(g.h) / scale)
            let origin = NSPoint(
                x: CGFloat(g.x) / scale,
                y: primaryTop - CGFloat(g.y) / scale - size.height
            )
            let frame = NSRect(origin: origin, size: size)
            // Meaningfully visible on the hypothesized screen (the geometry_on_screen
            // intent, evaluated in AppKit space).
            let overlap = screen.visibleFrame.intersection(frame)
            if overlap.width >= 100 && overlap.height >= 40 {
                return frame
            }
        }
        return nil
    }

    /// Window moved/resized: refresh the remembered geometry (winit's
    /// `track_windowed_geometry` — the core dedupes and owns the debounced save).
    /// Suppressed until the startup restore settles — see `startupSettled`.
    private func noteWindowGeometry() {
        guard startupSettled, !speedModeFullscreen, let window = hostWindow else { return }
        let scale = window.backingScaleFactor
        let primaryTop = NSScreen.screens.first?.frame.maxY ?? 0
        let f = window.frame
        core.note_window_geometry(
            Int32((f.minX * scale).rounded()),
            Int32(((primaryTop - f.maxY) * scale).rounded()),
            UInt32((f.width * scale).rounded()),
            UInt32((f.height * scale).rounded())
        )
        kick() // the core tick flushes the debounced save once the user stops
    }

    func canvasResized(pixelSize: CGSize, scale: CGFloat) {
        pbTrace("canvasResized px=\(Int(pixelSize.width))x\(Int(pixelSize.height)) scale=\(scale)")
        core.resized(UInt32(pixelSize.width), UInt32(pixelSize.height), Float(scale))
        if let layer = canvasLayer {
            // A surface reconfigure can reset the layer's colorspace — re-assert, exactly
            // like the winit shell does after its resize handling.
            configureEDR(on: layer)
        }
        core.render()
        drainEffects()
        // The render above can be DROPPED by the surface (Lost/Outdated/Timeout —
        // routine mid resize/fullscreen churn); the core flags it (`redraw_pending` →
        // `work_pending`) and the tick loop retries. Wake the pump so that retry runs
        // even from idle — it re-pauses itself once the engine goes quiet.
        kick()
    }

    func detachCanvas() {
        canvasLayer = nil
        core.detach_layer()
    }

    /// The layer poke `pb-app/src/hdr_surface.rs` does on the winit target — here the host
    /// owns the layer, so it's plain Swift: an fp16 scRGB surface needs the layer tagged
    /// extended-linear-sRGB (+ EDR on), and the roll-off needs the panel's real headroom
    /// (macOS hard-clips above it; Windows' DWM tone-maps for you).
    ///
    /// Headroom comes from **the window's actual screen** — the winit port's known
    /// multi-display bug: reading `NSScreen.main` made HDR "look totally broken" whenever
    /// the main display was an SDR panel. Re-poked on every screen change and on display
    /// parameter changes (HDR toggled on the same display — the winit build's leftover).
    private func configureEDR(on layer: CAMetalLayer) {
        canvasLayer = layer
        guard core.wants_edr() else { return }
        layer.colorspace = CGColorSpace(name: CGColorSpace.extendedLinearSRGB)
        layer.wantsExtendedDynamicRangeContent = true
        let screen = hostWindow?.screen ?? NSScreen.main
        // POTENTIAL, not current: maximumExtendedDynamicRangeColorComponentValue sits at
        // ~1.0 until EDR content is already on screen (it ramps up after), so reading it
        // at attach rolled every highlight off toward SDR — the owner-reported "old
        // build showed brighter highlights" regression vs winit's hdr_surface.rs, which
        // reads the panel's potential capability. Match it.
        let headroom = Float(
            screen?.maximumPotentialExtendedDynamicRangeColorComponentValue ?? 1.0)
        core.set_edr_headroom(max(1.0, headroom))
    }

    /// Re-assert the EDR colorspace + headroom for the window's current screen, then
    /// repaint (the highlight roll-off changes with the headroom).
    private func refreshEDR() {
        guard let layer = canvasLayer else { return }
        configureEDR(on: layer)
        core.render()
    }

    /// The attached canvas layer, kept weakly-by-convention (the view owns it; cleared in
    /// `detachCanvas`) so resize can re-assert the EDR colorspace.
    @ObservationIgnored private weak var canvasLayer: CAMetalLayer?
    /// The canvas view, so a window-mode transition can re-report its settled pixel size
    /// (`reportSizeNow`) when AppKit doesn't re-fire the view's `layout()` on its own.
    @ObservationIgnored weak var canvasView: MetalCanvasNSView?

    /// The active native video player (task 79.9): `AVPlayer` + `AVPlayerLayer` over the
    /// canvas, commanded by the core's `PlayVideo`/`StopVideo` effects. macOS-only; the
    /// single media authority (the Rust core keeps only a passive proxy).
    @ObservationIgnored private var nativeVideo: NativeVideoPlayer?

    /// The active sample-buffer video presenter (video-overhaul Phase 3): FFmpeg
    /// (Rust) demux → `AVSampleBufferDisplayLayer`, for containers `AVPlayer` can't
    /// demux (MKV/WebM) — commanded by the core's `PlaySampleBuffer`/`StopVideo`/…
    /// effects. Mutually exclusive with `nativeVideo` (one Apple presenter at a
    /// time); it reports state through the same `nativeVideo*` callbacks, so the
    /// core drives both through one `Native` proxy.
    @ObservationIgnored private var sampleBufferVideo: SampleBufferPresenter?

    /// The session-video audio sink (task #84 §7): AVAudioEngine over the Rust FFmpeg
    /// audio decoder, for session-backed (FFmpeg) videos — commanded by the core's
    /// `StartVideoAudio`/`StopVideoAudio`/… effects; its clock samples flow back ~4×/s
    /// from `pump()`.
    @ObservationIgnored private var sessionAudio: SessionAudioPlayer?
    @ObservationIgnored private var sessionAudioSampledAt = Date.distantPast

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
            // Split `name (idx/n)` into a filename title + an "N of M" subtitle for the
            // unified toolbar (the empty-state "PhotoBlaze" just sets the title).
            applyWindowTitle(t)
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
            terminateNow()
        case .ShellFlowAction(let id):
            // A host-side flow command by stable Action id. Esc arrives HERE (the keymap
            // resolves Escape → Action::Quit → a host-side flow action), not as .Quit.
            let action = id.toString()
            log("ShellFlowAction(\"\(action)\")")
            if action == "quit" {
                terminateNow()
            }
        case .ReportError(let msg):
            // A user-facing error (bad open, refused archive, …) — a native alert.
            let text = msg.toString()
            log("ERROR: \(text)")
            presentMessageAlert(text)
        case .CloseDialog:
            // Programmatic close (the answer was processed / the op finished). Setting
            // activeSheet directly never routes through userDismissedSheet().
            log("CloseDialog (sheet was \(activeSheet?.rawValue ?? "none"))")
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
        // Session-video audio (task #84 §7, plan §7/1E): the FFmpeg-backed sink for
        // session videos, now decoded OFF the main actor (R5). Open is async:
        // until it lands the clock reports Opening; a no-audio / open / graph
        // failure reports Failed and the session degrades to silent — so init is
        // non-failable and there is no nil check here.
        case .StartVideoAudio(let sessionId, let muted):
            sessionAudio?.stop()
            sessionAudio = SessionAudioPlayer(sessionId: sessionId, muted: muted)
            sessionAudioSampledAt = Date.distantPast // sample immediately (readiness)
        case .StopVideoAudio:
            sessionAudio?.stop()
            sessionAudio = nil
        case .PauseVideoAudio:
            sessionAudio?.pause()
        case .ResumeVideoAudio:
            sessionAudio?.resume()
        case .SeekVideoAudio(let seconds):
            sessionAudio?.seek(toSeconds: seconds)
        case .SetVideoAudioMuted(let muted):
            sessionAudio?.setMuted(muted)
        case .PlayVideo(let path, let sessionId, let muted, let startSecs):
            playNativeVideo(
                path: path.toString(), sessionId: sessionId, muted: muted, startSecs: startSecs)
        case .PlayVideoBytes(let name, let sessionId, let muted, let startSecs):
            playNativeVideoBytes(
                name: name.toString(), sessionId: sessionId, muted: muted, startSecs: startSecs)
        case .PlaySampleBuffer(let sessionId, let muted, let startSecs):
            playSampleBufferVideo(sessionId: sessionId, muted: muted, startSecs: startSecs)
        case .RequestVideoPoster(let requestId, let item, let name, let maxEdge):
            generateArchivePoster(
                requestId: requestId, item: item, name: name.toString(), maxEdge: maxEdge)
        case .StopVideo(let sessionId):
            // Session-gated: a StopVideo for a superseded session must not tear down
            // the current one (a newer Play may already have replaced it). Routed to
            // whichever Apple presenter is active (mutually exclusive).
            if nativeVideo?.sessionId == sessionId {
                nativeVideo?.stop()
                nativeVideo = nil
            }
            if sampleBufferVideo?.sessionId == sessionId {
                sampleBufferVideo?.stop()
                sampleBufferVideo = nil
            }
        case .PauseVideo(let sessionId):
            if nativeVideo?.sessionId == sessionId { nativeVideo?.pause() }
            if sampleBufferVideo?.sessionId == sessionId { sampleBufferVideo?.pause() }
        case .ResumeVideo(let sessionId):
            if nativeVideo?.sessionId == sessionId { nativeVideo?.resume() }
            if sampleBufferVideo?.sessionId == sessionId { sampleBufferVideo?.resume() }
        case .SelectAudioTrack(let row):
            // `A` / `Shift+A`. The rows are the core's, but the switch is ours — and the
            // core stepped from a row index, so refresh the snapshot before acting or the
            // locator lookup would read a list that no longer matches.
            core.audio_picker_refresh()
            selectAudioTrack(Int(row))
        case .SeekVideoBy(let sessionId, let generation, let deltaMs):
            // Arrow-key seek (±2s / Shift ±10s). The player resolves + clamps the delta and
            // reports back so the proxy's in-flight/generation bookkeeping stays honest.
            if nativeVideo?.sessionId == sessionId {
                nativeVideo?.seek(byMilliseconds: deltaMs, generation: generation)
            }
            if sampleBufferVideo?.sessionId == sessionId {
                sampleBufferVideo?.seek(byMilliseconds: deltaMs, generation: generation)
            }
        case .StepVideo(let sessionId, let forward):
            if nativeVideo?.sessionId == sessionId { nativeVideo?.step(forward: forward) }
            if sampleBufferVideo?.sessionId == sessionId { sampleBufferVideo?.step(forward: forward) }
        case .SetVideoMuted(let sessionId, let muted):
            if nativeVideo?.sessionId == sessionId { nativeVideo?.setMuted(muted) }
            if sampleBufferVideo?.sessionId == sessionId { sampleBufferVideo?.setMuted(muted) }
        case .SetWindowMode(let fullscreen):
            log("SetWindowMode(fullscreen: \(fullscreen))")
            setWindowMode(fullscreen: fullscreen)
            // Entering the borderless speed mode by mouse hides all chrome (the toolbar and
            // menu bar auto-hide) — a mouse user has no visible way back out, so hint the key.
            // Only when the toggle came by mouse (a keyboard user just pressed it themselves).
            if fullscreen {
                if fullscreenHintFromMouse { showFullscreenHint() }
            } else {
                hideFullscreenHint() // exiting — a "press F to exit" notice is now stale
            }
            fullscreenHintFromMouse = false
        case .HideWindow:
            hostWindow?.orderOut(nil)
        case .MenuStateChanged:
            menuBar?.sync(core.menu_state())
            reassertMenuBar() // belt-and-braces beside the KVO watch
            syncToolbar()
        case .PanelsChanged:
            // A natively-presented panel changed — re-pull the Help model + visibility,
            // the empty-state Open panel's visibility, and the Inspector (visibility +
            // tab + rows; async OCR / describe results re-signal here too).
            refreshHelp()
            openPanelVisible = core.open_panel_visible()
            refreshInspector()
            refreshTree()
            refreshThumbs()
            syncToolbar() // the folder-tree / inspector toggle state lives here, not MenuState
        case .ShowContextMenu(
            let hasImage, let hasMotion, let canReveal, let fullscreen,
            let comparePinned, let comparePinnedHere
        ):
            popContextMenu(
                hasImage: hasImage, hasMotion: hasMotion,
                canReveal: canReveal, fullscreen: fullscreen,
                comparePinned: comparePinned, comparePinnedHere: comparePinnedHere
            )
        case .ShowDialog(let kind):
            showDialog(kind.toString())
        case .Other:
            log("Other (not yet bridged)")
        }
    }

    /// Dev diagnostics. The NS1 on-screen effect log retired with the NS2 dialogs; a
    /// terminal launch (`swift run`, the dev build-run loop) still sees the trace.
    /// Quit with a zombie watchdog. `NSApp.terminate` is a *request*: SwiftUI's
    /// termination machinery (or a modal session in flight) can silently defer or
    /// absorb it — and because the quit path hides the window first (`HideWindow`
    /// drains before the terminate), a swallowed terminate leaves an invisible live
    /// process. LaunchServices then "reopens" that zombie on every `open`, which is
    /// how a freshly built app can appear stale. The Esc teardown writes nothing to
    /// disk by design (privacy #2 — there is no flush step to lose), so if we're
    /// still alive shortly after asking nicely, exiting hard forfeits nothing.
    private func terminateNow() {
        NSApp.terminate(nil)
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) {
            NSLog("\(appName): NSApp.terminate was deferred — hard exit (zombie watchdog)")
            exit(0)
        }
    }

    private func log(_ text: String) {
        #if DEBUG
            // Uptime-stamped (ms precision) so event ordering around a bug is readable.
            print("PB[\(String(format: "%10.3f", ProcessInfo.processInfo.systemUptime))]: \(text)")
        #endif
    }

    // MARK: - Native handlers (the genuinely-platform effects)

    /// Whether an NSOpenPanel is up — the key monitor passes events through untouched then,
    /// so typing in the panel (its search field, ⌘-shortcuts) isn't swallowed by the viewer.
    /// Tracked (not `@ObservationIgnored`) so the welcome screen's Open buttons can disable
    /// themselves while it's up — the empty-state view is the one place a view reads this.
    private(set) var panelOpen = false

    /// The native open panel (`CoreEffect::OpenFilePanel`/`OpenFolderPanel`). Mirrors the
    /// winit shell's rfd usage: files default to the images+archives filter (a picked `.zip`
    /// opens as an archive), multi-select allowed; results feed `open_paths` — the same
    /// classify-and-open path as a Finder drop. Guarded against re-entry: `open_file`/
    /// `open_folder` have no menu/keyboard gating of their own, so without this, invoking
    /// either again (⌘O, the menu, or — before they disabled — the welcome buttons) while
    /// a panel is already up spawned a second `NSOpenPanel` stacked on the first.
    private func presentOpenPanel(startDir: String, choosingFolders: Bool) {
        guard !panelOpen else {
            log("presentOpenPanel: already up — ignoring duplicate request")
            return
        }
        let panel = NSOpenPanel()
        panel.canChooseFiles = !choosingFolders
        panel.canChooseDirectories = choosingFolders
        panel.allowsMultipleSelection = !choosingFolders
        panel.directoryURL = URL(fileURLWithPath: startDir, isDirectory: true)
        if !choosingFolders {
            // Images + video + archives — mirror IMAGE_FILTER_EXTS + VIDEO_FILTER_EXTS
            // (+zip/7z) in pb-app/src/main.rs. A container AVFoundation can't play still
            // shows here but fails gracefully on open (nativeVideoFailed → Message dialog),
            // same as an unreadable archive. (No "All files" escape hatch — NSOpenPanel has
            // no filter popup like Windows'; anything exotic comes in via a folder or a drop.)
            let exts = [
                "jpg", "jpeg", "jpe", "jfif", "png", "gif", "bmp", "tif", "tiff", "webp",
                "tga", "qoi", "jxl", "svg", "svgz", "heic", "heif", "avif", "hdr", "exr",
                "arw", "nef", "cr2", "cr3", "dng", "raf", "rw2", "orf", "srw", "pef", "raw",
                "mp4", "m4v", "mov", "qt", "mkv", "webm", "avi", "wmv", "asf", "mpg", "mpeg",
                "mts", "m2ts", "3gp", "3g2",
                // Archives (#30 zip/7z, #102 tar family). The panel matches on the
                // FINAL extension, so `.tar.gz` needs the bare `gz` entry; a picked
                // bare `photo.jpg.gz` is refused cleanly by the classifier. UTTypes
                // that don't resolve on this OS just drop out of the filter.
                "zip", "7z",
                "tar", "tgz", "tbz2", "tbz", "tzst", "txz", "gz", "bz2", "zst", "xz",
                "rar", "cbr", "cbz",
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
        // While a panel's resize strip owns the cursor, ignore the core's cursor
        // intent. The core reacts to pointer-moves over the photo *beneath* the panel
        // (it doesn't know the panel is there) and emits an arrow/grab `SetCursor` on
        // essentially every move — which would stomp the resize cursor back a pixel
        // after the strip's hover set it, so the ↔ only flashed for ~1pt. The strip's
        // hover owns the cursor until the pointer leaves it (see `setPanelResizeCursor`).
        if panelResizeCursorActive { return }
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

    /// A panel's resize strip claims the cursor while hovered. It must route through
    /// `desiredCursor` (not a bare `NSCursor.set()`): the canvas re-asserts `desiredCursor`
    /// on every `cursorUpdate` pass — including under the panel, since its tracking area
    /// spans the whole view — so a directly-`set()` resize cursor is stomped back to the
    /// arrow on the same event cycle and never shows. Setting the source of truth makes the
    /// re-assertion agree instead of fight. Restores the arrow on exit.
    func setPanelResizeCursor(_ active: Bool) {
        panelResizeCursorActive = active
        desiredCursor = active ? .resizeLeftRight : .arrow
        desiredCursor.set()
    }

    /// True while the pointer is over a panel's resize strip — makes `applyCursor`
    /// ignore the core's competing cursor updates so the ↔ resize cursor holds across
    /// the whole strip instead of being stomped back to the arrow on the next move.
    @ObservationIgnored private var panelResizeCursorActive = false

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
    @ObservationIgnored private var screenParamsObserver: NSObjectProtocol?
    @ObservationIgnored private var geometryObservers: [NSObjectProtocol] = []
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
        if let old = resizeTraceObserver {
            NotificationCenter.default.removeObserver(old)
            resizeTraceObserver = nil
        }
        if let old = screenParamsObserver {
            NotificationCenter.default.removeObserver(old)
            screenParamsObserver = nil
        }
        for old in geometryObservers {
            NotificationCenter.default.removeObserver(old)
        }
        geometryObservers.removeAll()
        guard let window = hostWindow else { return }
        screenChangeObserver = NotificationCenter.default.addObserver(
            forName: NSWindow.didChangeScreenNotification, object: window, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                // EDR first (immediate — an XDR↔SDR hop changes the roll-off headroom),
                // then the size clamp (deferred until the drag settles).
                self?.refreshEDR()
                self?.clampToScreenWhenSettled()
            }
        }
        // HDR toggled / display re-configured without the window moving — the case the
        // winit build couldn't catch (it re-checks only on window move).
        screenParamsObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.didChangeScreenParametersNotification,
            object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.refreshEDR() }
        }
        // Remember the windowed frame across moves/resizes (#1 — the debounced save
        // lives in the core; a drag is not a write storm).
        for name in [NSWindow.didMoveNotification, NSWindow.didResizeNotification] {
            geometryObservers.append(NotificationCenter.default.addObserver(
                forName: name, object: window, queue: .main
            ) { [weak self] _ in
                MainActor.assumeIsolated { self?.noteWindowGeometry() }
            })
        }
        // DEBUG forensics for the owner-reported cross-monitor size weirdness: log every
        // PROGRAMMATIC resize (a hand drag has inLiveResize == true — skip those). Our
        // own clamp logs separately, so an unexplained entry here is the OS (drag-tiling
        // fill / pre-tile size restore) or SwiftUI resizing the window.
        #if DEBUG
            resizeTraceObserver = NotificationCenter.default.addObserver(
                forName: NSWindow.didResizeNotification, object: window, queue: .main
            ) { [weak self] note in
                MainActor.assumeIsolated {
                    guard let self, let w = note.object as? NSWindow, !w.inLiveResize
                    else { return }
                    self.log(
                        "window resized programmatically to "
                            + "\(Int(w.frame.width))×\(Int(w.frame.height))"
                    )
                }
            }
        #endif
    }

    @ObservationIgnored private var resizeTraceObserver: NSObjectProtocol?

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
        let frame = window.frame
        guard frame.width > visible.width || frame.height > visible.height else { return }
        let fitted = Self.shrunkToFit(frame, in: visible)
        window.setFrame(fitted, display: true, animate: true)
        log("clamped oversized window to \(Int(fitted.width))×\(Int(fitted.height)) after screen change")
    }

    /// Shrink-only fit: each axis is clamped independently to `visible` (never grown),
    /// then the frame is pulled fully inside it. A frame that already fits is unchanged.
    private static func shrunkToFit(_ frame: NSRect, in visible: NSRect) -> NSRect {
        var f = frame
        f.size.width = min(f.width, visible.width)
        f.size.height = min(f.height, visible.height)
        f.origin.x = max(visible.minX, min(f.origin.x, visible.maxX - f.width))
        f.origin.y = max(visible.minY, min(f.origin.y, visible.maxY - f.height))
        return f
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

    /// `CoreEffect::PlayVideo` (task 79.9): open the clip in a native `AVPlayer` and
    /// present it over the canvas. Replaces any prior player. The `AVPlayerLayer` shows
    /// only once its first frame is ready — the wgpu poster holds until then.
    private func playNativeVideo(path: String, sessionId: UInt64, muted: Bool, startSecs: Double) {
        nativeVideo?.stop()
        nativeVideo = nil
        guard let canvas = canvasView else {
            log("PlayVideo: no canvas view to present into")
            return
        }
        resetVideoControls() // start the new clip's scrubber at 0, not the last clip's spot
        nativeVideo = NativeVideoPlayer(
            url: URL(fileURLWithPath: path), muted: muted, sessionId: sessionId,
            scaleMode: core.menu_state().scale, canvas: canvas, model: self, startSecs: startSecs)
    }

    /// `CoreEffect::PlaySampleBuffer` (video-overhaul Phase 3): open the clip in the
    /// sample-buffer presenter — FFmpeg (Rust) demuxes the container `AVPlayer` can't
    /// open, and the compressed packets feed an `AVSampleBufferDisplayLayer` (system
    /// decode + DoVi/HDR). The container input was stashed Rust-side by the effect; the
    /// presenter's `DemuxReader` opens it off the main actor via `open_stashed_demux`.
    private func playSampleBufferVideo(sessionId: UInt64, muted: Bool, startSecs: Double) {
        nativeVideo?.stop()
        nativeVideo = nil
        sampleBufferVideo?.stop()
        sampleBufferVideo = nil
        guard let canvas = canvasView else {
            log("PlaySampleBuffer: no canvas view to present into")
            return
        }
        resetVideoControls()
        sampleBufferVideo = SampleBufferPresenter(
            sessionId: sessionId, scaleMode: core.menu_state().scale, muted: muted,
            startSecs: startSecs, canvas: canvas, model: self)
    }

    /// `CoreEffect::PlayVideoBytes` (macOS archive video, task #30): the core stashed the
    /// entry's in-RAM container bytes — pull them once and open an `AVPlayer` backed by a
    /// resource loader that serves them on demand (never written to disk; privacy #2).
    private func playNativeVideoBytes(
        name: String, sessionId: UInt64, muted: Bool, startSecs: Double
    ) {
        nativeVideo?.stop()
        nativeVideo = nil
        guard let canvas = canvasView else {
            log("PlayVideoBytes: no canvas view to present into")
            return
        }
        let bytes = core.take_pending_video_bytes()
        guard bytes.len() > 0 else {
            log("PlayVideoBytes: no bytes stashed for \(name)")
            return
        }
        let data = Data(bytes: UnsafeRawPointer(bytes.as_ptr()), count: bytes.len())
        resetVideoControls()
        nativeVideo = NativeVideoPlayer(
            data: data, name: name, muted: muted, sessionId: sessionId,
            scaleMode: core.menu_state().scale, canvas: canvas, model: self, startSecs: startSecs)
    }

    /// `CoreEffect::RequestVideoPoster` (macOS archive-video posters, task #30): pull the
    /// entry's in-RAM bytes, grab a frame with `AVAssetImageGenerator` off the same
    /// resource-loader asset playback uses, convert to RGBA8, and hand it to the core, which
    /// uploads it into the resident ring (replacing the placeholder). Off the event loop:
    /// the frame decode is async; only the quick byte copy + asset build run here.
    private func generateArchivePoster(
        requestId: UInt64, item: UInt64, name: String, maxEdge: UInt32
    ) {
        let bytes = core.take_pending_poster_bytes(requestId)
        guard bytes.len() > 0 else { return }
        let data = Data(bytes: UnsafeRawPointer(bytes.as_ptr()), count: bytes.len())
        let loader = ArchiveVideoLoader(data: data, name: name)
        let asset = AVURLAsset(url: loader.url)
        asset.resourceLoader.setDelegate(loader, queue: loader.queue)
        // Poster: brightness-walk a few candidate times off the main thread; deliver the
        // first bright-enough frame (falling back to the last one so a dark clip still gets
        // *a* poster — parity with the Rust loose-file walk, POSTER_LUMA_MIN = 0.10).
        Task.detached { [weak self] in
            let gen = AVAssetImageGenerator(asset: asset)
            gen.appliesPreferredTrackTransform = true  // upright (rotation baked in)
            gen.requestedTimeToleranceBefore = CMTime(seconds: 1.0, preferredTimescale: 600)
            gen.requestedTimeToleranceAfter = CMTime(seconds: 1.0, preferredTimescale: 600)
            var fallback: (w: Int, h: Int, data: Data)?
            for secs in [0.5, 2.0, 5.0] {  // times past the clip's end just fail → skipped
                let time = CMTime(seconds: secs, preferredTimescale: 600)
                guard let cg = try? await gen.image(at: time).image,
                    let poster = CoreModel.cgImageToRGBA(cg, maxEdge: Int(maxEdge))
                else { continue }
                if CoreModel.meanLuma(poster.data) > 0.10 {
                    await self?.deliverArchivePoster(requestId: requestId, item: item, poster: poster)
                    _ = loader
                    return
                }
                fallback = poster
            }
            if let fb = fallback {
                await self?.deliverArchivePoster(requestId: requestId, item: item, poster: fb)
            }
            _ = loader  // retain the resource loader until generation completes
        }

        // Metadata: probe the stream facts (codec/fps/duration/audio) for the inspector,
        // off-actor via the async load APIs, then hand them to the core.
        Task.detached { [weak self] in
            guard let track = try? await asset.loadTracks(withMediaType: .video).first else {
                _ = loader
                return
            }
            let fps = (try? await track.load(.nominalFrameRate)) ?? 0
            let formats = (try? await track.load(.formatDescriptions)) ?? []
            let codec =
                formats.first.map { CoreModel.fourccName(CMFormatDescriptionGetMediaSubType($0)) }
                ?? "Video"
            let durSecs = (try? await asset.load(.duration)).map { CMTimeGetSeconds($0) } ?? 0
            let hasAudio = !(((try? await asset.loadTracks(withMediaType: .audio)) ?? []).isEmpty)
            let durMs = Int64((durSecs.isFinite && durSecs > 0) ? durSecs * 1000 : -1)
            let fpsMilli = UInt32(max(0, (Double(fps) * 1000).rounded()))
            await self?.deliverArchiveVideoMeta(
                item: item, codec: codec, fpsMilli: fpsMilli, durationMs: durMs, hasAudio: hasAudio)
            _ = loader
        }
    }

    /// Hand a generated archive-video poster to the core (main actor): copy the RGBA8 into
    /// the resident ring and kick the pump so the next tick uploads it.
    @MainActor private func deliverArchivePoster(
        requestId: UInt64, item: UInt64, poster: (w: Int, h: Int, data: Data)
    ) {
        poster.data.withUnsafeBytes { raw in
            core.video_poster_ready(
                requestId, item, UInt32(poster.w), UInt32(poster.h),
                UInt(bitPattern: raw.baseAddress), UInt(raw.count))
        }
        kick()
    }

    /// Hand an archive-video's probed stream facts to the core (main actor) for the inspector.
    @MainActor private func deliverArchiveVideoMeta(
        item: UInt64, codec: String, fpsMilli: UInt32, durationMs: Int64, hasAudio: Bool
    ) {
        core.archive_video_meta_ready(item, codec, fpsMilli, durationMs, hasAudio)
        kick()
    }

    /// Mean luma (0…1) of an RGBA8 buffer, sampled every 8th pixel — the black-lead-in
    /// gate for the poster walk (matches `pb_decode`'s `mean_luma_rgba8` stride + range).
    nonisolated private static func meanLuma(_ data: Data) -> Double {
        data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> Double in
            let p = raw.bindMemory(to: UInt8.self)
            var sum = 0.0
            var n = 0
            var i = 0
            let step = 8 * 4
            while i + 2 < p.count {
                sum += (Double(p[i]) + Double(p[i + 1]) + Double(p[i + 2])) / 3.0 / 255.0
                n += 1
                i += step
            }
            return n > 0 ? sum / Double(n) : 0
        }
    }

    /// A CoreMedia FourCC video subtype → display codec name (matches the Rust
    /// `codec_name_from_fourcc` labels). Unknown → "Video".
    nonisolated private static func fourccName(_ cc: FourCharCode) -> String {
        let b = [
            UInt8((cc >> 24) & 0xff), UInt8((cc >> 16) & 0xff),
            UInt8((cc >> 8) & 0xff), UInt8(cc & 0xff),
        ]
        switch String(bytes: b, encoding: .ascii) {
        case "avc1", "avc3": return "H.264"
        case "hvc1", "hev1": return "HEVC"
        case "av01": return "AV1"
        case "vp09": return "VP9"
        case "vp08": return "VP8"
        case "mp4v": return "MPEG-4"
        case "mjpg", "jpeg": return "Motion JPEG"
        case "apch", "apcn", "apcs", "apco", "ap4h", "ap4x": return "ProRes"
        default: return "Video"
        }
    }

    /// A `CGImage` → straight RGBA8 (`w*h*4`), fitted so the long edge ≤ `maxEdge` (the
    /// decode-fit target), matching what the Rust poster path hands the ring.
    nonisolated private static func cgImageToRGBA(_ cg: CGImage, maxEdge: Int) -> (w: Int, h: Int, data: Data)?
    {
        let (srcW, srcH) = (cg.width, cg.height)
        guard srcW > 0, srcH > 0 else { return nil }
        var (w, h) = (srcW, srcH)
        let longEdge = max(srcW, srcH)
        if maxEdge > 0, longEdge > maxEdge {
            let s = Double(maxEdge) / Double(longEdge)
            w = max(1, Int((Double(srcW) * s).rounded()))
            h = max(1, Int((Double(srcH) * s).rounded()))
        }
        let bytesPerRow = w * 4
        var buf = Data(count: bytesPerRow * h)
        let ok = buf.withUnsafeMutableBytes { raw -> Bool in
            guard
                let ctx = CGContext(
                    data: raw.baseAddress, width: w, height: h, bitsPerComponent: 8,
                    bytesPerRow: bytesPerRow, space: CGColorSpaceCreateDeviceRGB(),
                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
            else { return false }
            ctx.draw(cg, in: CGRect(x: 0, y: 0, width: w, height: h))
            return true
        }
        return ok ? (w, h, buf) : nil
    }

    /// Re-lay-out the native video layer against the current canvas bounds (resize /
    /// fullscreen / display move). The player owns the frame/gravity math.
    func relayoutNativeVideo() {
        nativeVideo?.relayout()
        sampleBufferVideo?.relayout()
    }

    /// The theme-aware letterbox/background fill (sRGB) photos use — the video container's
    /// background, so a letterboxed / Original video sits on the user's chosen background.
    var videoLetterboxCGColor: CGColor {
        let rgb = core.effective_letterbox_rgb()
        return CGColor(
            srgbRed: CGFloat((rgb >> 16) & 0xFF) / 255,
            green: CGFloat((rgb >> 8) & 0xFF) / 255,
            blue: CGFloat(rgb & 0xFF) / 255,
            alpha: 1)
    }
    /// Last letterbox value pushed to the video container, so the pump repaints it only on a
    /// real change (a theme switch / a Settings edit), not every tick.
    @ObservationIgnored private var lastVideoLetterbox: UInt32 = 0xFFFF_FFFF

    /// Player → model: the scrubber's position/duration/playing (task 79.9 phase 5).
    /// Session-gated. Formats the time labels; the fraction drives the bar + knob.
    func updateVideoProgress(_ sessionId: UInt64, elapsed: Double, total: Double, playing: Bool) {
        guard nativeVideo?.sessionId == sessionId || sampleBufferVideo?.sessionId == sessionId
        else { return }
        // Feed the core's session-only resume map (task #94.2): the core keeps no native
        // clock, so this ~20 Hz report is where "where you left off" comes from. Cheap +
        // session-gated core-side; a near-start/end position is forgotten there.
        core.native_video_progress(sessionId, elapsed, total)
        // The fraction updates ~20 Hz to track the real playhead (no animation — that lagged
        // a sample behind and jumped forward on pause). The labels change ~1 Hz, so guard
        // them to avoid needless re-renders at the fraction's rate.
        let reported = total > 0 ? min(1.0, max(0.0, elapsed / total)) : 0.0
        // Scrubber pin (plan §H3): while a sample-buffer seek is settling, hold the target on
        // BOTH knob and time so they agree, and don't let a pre-seek report snap them back.
        // Clear when the real playhead reaches the target — an absolute ~0.5 s window, which
        // is **direction-independent**. (The old `reported >=/<= target` direction test was
        // fragile: a scrubber click fires two seeks and the 2nd recomputed direction against
        // the already-pinned target, flipping a BACKWARD seek to "forward", clearing the pin
        // early, and flashing the knob to the old position — owner, 2026-07-15.)
        if let target = pendingSeekTarget, total > 0 {
            if abs(elapsed - target * total) <= 0.5 { pendingSeekTarget = nil }
        }
        if let target = pendingSeekTarget {
            videoFraction = target
            let e = Self.formatTime(target * total)
            if e != videoElapsed { videoElapsed = e }
        } else {
            videoFraction = reported
            let e = Self.formatTime(elapsed)
            if e != videoElapsed { videoElapsed = e }
        }
        let t = total > 0 ? Self.formatTime(total) : ""
        if t != videoTotal { videoTotal = t }
        if playing != videoPlaying { videoPlaying = playing }
    }

    /// Zero the scrubber for a fresh video so it never inherits the previous clip's position
    /// (and never glides down to 0). Plain assignments → instant.
    private func resetVideoControls() {
        videoElapsed = "0:00"
        videoTotal = ""
        videoFraction = 0
        videoPlaying = false
        pendingSeekTarget = nil  // a fresh clip cancels any settling-seek pin (plan §H3)
    }

    /// The session-backed (FFmpeg, task #84 §8) twin of `updateVideoProgress`: the pump
    /// polls the core's session clock while a session is active. Same change-guards so
    /// the ~1 Hz labels don't re-render at the pump's rate.
    private func updateSessionVideoProgress() {
        let elapsed = core.video_session_elapsed_secs()
        let total = core.video_session_duration_secs()
        videoFraction = total > 0 ? min(1.0, max(0.0, elapsed / total)) : 0.0
        let e = Self.formatTime(elapsed)
        if e != videoElapsed { videoElapsed = e }
        let t = total > 0 ? Self.formatTime(total) : ""
        if t != videoTotal { videoTotal = t }
        let playing = core.video_session_playing()
        if playing != videoPlaying { videoPlaying = playing }
    }

    /// The info-line scrubber was dragged/clicked to `fraction` of the duration. A native
    /// video seeks its player directly (it owns the clock); a session-backed video routes
    /// through the core's fractional seek (task #84 §8 — the same `video_seek_fraction`
    /// the winit shell's playback bar uses). The play/pause button routes through the
    /// core action so it matches `P`.
    func seekVideoFraction(_ fraction: Double) {
        if let nv = nativeVideo {
            nv.seek(toFraction: fraction)  // AVPlayer owns its clock; no pin (plan §H3)
        } else if let sbv = sampleBufferVideo {
            // Pin the scrubber to the target until the seek visually lands (plan §H3), so a
            // pre-seek progress report can't snap the knob back. The latest seek wins.
            pendingSeekTarget = fraction
            videoFraction = fraction
            sbv.seek(toFraction: fraction)
        } else if sessionVideoActive {
            core.video_seek_fraction(Float(fraction))
            kick()
            drainEffects()
        }
    }

    /// The current item's video-layer placement (physical px, top-left origin) — the
    /// still renderer's geometry, so the `AVPlayerLayer` tracks Fit/Fill/Original + zoom +
    /// pan + rotation like a photo. `NativeVideoPlayer.relayout()` consumes it.
    func videoPlacement() -> VideoPlacementFfi {
        core.video_placement()
    }

    /// The scrubber drag started/ended. While scrubbing, `pump()` pins the controls up (the
    /// drag captures the pointer, so the hover flash stops refreshing). On release, re-arm the
    /// core flash so the controls fade out on the normal 1.8s timer instead of snapping away.
    func scrubbingChanged(_ active: Bool) {
        videoScrubbing = active
        if !active {
            core.flash_video_controls()
        }
        kick()
    }

    /// The playback row's play/pause button — same path as `P` / the toolbar.
    func toggleVideoPlay() {
        menuAction("play_pause")
    }

    /// The track picker's popover opened/closed (task #99). While open it pins the controls
    /// up (the pointer is in the popover, not over the canvas, so the hover flash stops
    /// refreshing); on close, re-arm the core flash so the bar fades on its normal timer
    /// instead of snapping away — exactly the scrubber's contract.
    func pickerOpenChanged(_ open: Bool) {
        videoPickerOpen = open
        if !open {
            core.flash_video_controls()
        }
        kick()
    }

    /// The subtitle track picker's rows (task #99), pulled **fresh**.
    ///
    /// Read at the moment the surface opens rather than pushed and cached: the list is
    /// per-file, and a cached one is a list that is wrong exactly when the user navigates.
    /// The core snapshots it so the rows can't shift between the count and the labels.
    func subtitleTrackRows() -> [SubtitleTrackRow] {
        core.subtitle_picker_refresh()
        let n = Int(core.subtitle_track_count())
        return (0..<n).map { i in
            SubtitleTrackRow(
                id: i,
                label: core.subtitle_track_label(UInt(i)).toString(),
                active: core.subtitle_track_active(UInt(i))
            )
        }
    }

    /// Has the track probe landed for the video on screen? `false` = still reading, which is
    /// **not** the same answer as "this file has no subtitle tracks" and must not be drawn
    /// as one.
    var subtitleTracksKnown: Bool { core.subtitle_tracks_known() }

    /// Is a video on screen at all — on **either** backend? The Playback menu's enable
    /// check. (`videoControlsVisible` answers a different question: whether the *chrome* is
    /// revealed, which is false for a playing video whose info line has faded out.)
    var videoShowing: Bool { core.video_showing() }

    // MARK: - The audio track picker (task #99)

    /// The Playback ▸ Audio rows, pulled fresh — same per-file reasoning as the subtitle
    /// flyout: read at open, never cached.
    ///
    /// **The order here is the fix, not a detail.** The rows are built first, then what is
    /// playing is resolved *into* one of them. Resolving earlier — when the player opened —
    /// could not work: the row list needs the track catalog, whose probe is still in flight
    /// at that moment, so the lookup ran against an empty list, concluded "nothing is
    /// playing", and stuck there. The menu then had no tick until you picked a track by
    /// hand. Both halves only exist together at menu-open, so that is where they are joined.
    func audioTrackRows() -> [AudioTrackRow] {
        core.audio_picker_refresh()
        resolveActiveAudioRow()
        let n = Int(core.audio_track_count())
        return (0..<n).map { i in
            AudioTrackRow(
                id: i,
                label: core.audio_track_label(UInt(i)).toString(),
                active: core.audio_track_active(UInt(i))
            )
        }
    }

    /// Ask whichever presenter owns this file what it is playing, and tick that row.
    ///
    /// Each route answers in its own currency and on its own schedule: the sample-buffer
    /// and Session routes cache a raw stream index (their decoders live behind a serial
    /// queue, so they cannot be asked synchronously), while AVPlayer's current selection
    /// is readable on the spot. All resolve to a row only here, against rows that
    /// definitely exist.
    private func resolveActiveAudioRow() {
        if let sbv = sampleBufferVideo {
            reportActiveAudioStream(sbv.activeAudioStream)
        } else if let nv = nativeVideo {
            reportActiveAudioRow(nv.currentAudioRow())
        } else if let sa = sessionAudio {
            reportActiveAudioStream(sa.activeAudioStream)
        } else {
            reportActiveAudioRow(-1)
        }
    }

    /// Row `i`'s FFmpeg stream index, or `-1` — the sample-buffer route's currency.
    func audioRowFfStream(_ i: Int) -> Int { Int(core.audio_track_ff_stream(UInt(i))) }

    /// Row `i`'s serialized `AVMediaSelectionOption`, or `nil` — AVPlayer's currency.
    func audioRowAvPlist(_ i: Int) -> Data? {
        let v = core.audio_track_av_plist(UInt(i))
        let n = Int(v.len())
        guard n > 0 else { return nil }
        // One bulk copy off the RustVec's buffer — a per-byte `get` would be O(n) FFI calls
        // for a plist that can run to hundreds of bytes, on every menu open.
        return Data(UnsafeBufferPointer(start: v.as_ptr(), count: n))
    }

    /// The picker row whose stored property list equals `plist`, or `-1` — how the AVPlayer
    /// route turns a live selection back into a row.
    func audioRowMatching(plist: Data) -> Int {
        let n = Int(core.audio_track_count())
        for i in 0..<n where audioRowAvPlist(i) == plist { return i }
        return -1
    }

    /// The sample-buffer route reports the container stream index it is playing; find its
    /// row and tell the core. `nil`/no match ticks nothing, which is the honest answer.
    func reportActiveAudioStream(_ stream: Int?) {
        guard let stream else {
            reportActiveAudioRow(-1)
            return
        }
        let n = Int(core.audio_track_count())
        for i in 0..<n where audioRowFfStream(i) == stream {
            reportActiveAudioRow(i)
            return
        }
        reportActiveAudioRow(-1)
    }

    /// Report which row is **actually playing** (`-1` = unknown). The tick's only source.
    func reportActiveAudioRow(_ row: Int) {
        core.set_active_audio_row(Int64(row))
    }

    /// Report a switch's outcome — the core toasts only when `ok`.
    func audioTrackSwitched(row: Int, ok: Bool) {
        core.audio_track_switched(UInt(row), ok)
        kick()
    }

    /// Route the selection to whichever presenter owns this file.
    func selectAudioTrack(_ row: Int) {
        if let sbv = sampleBufferVideo {
            sbv.selectAudioTrack(row: row)
        } else if let nv = nativeVideo {
            nv.selectAudioTrack(row: row)
        } else if let sa = sessionAudio {
            // The Session route's currency is the FFmpeg stream index, end-to-end
            // (macos-video-smoothness §2). A row with no ff-stream is a refusal —
            // the tick must not move on a switch that cannot happen.
            let stream = audioRowFfStream(row)
            guard stream >= 0 else {
                audioTrackSwitched(row: row, ok: false)
                return
            }
            sa.switchTrack(ffStream: stream) { [weak self, weak sa] ok in
                MainActor.assumeIsolated {
                    // A stale callback (the video changed mid-switch) must not
                    // touch the tick or toast for whatever plays now.
                    guard let self, let sa, self.sessionAudio === sa else { return }
                    // Refresh the tick from what is ACTUALLY playing first — on a
                    // refusal that is the old track, on a stale pick the decoder's
                    // policy choice; never the request (same rule as the
                    // sample-buffer route).
                    self.reportActiveAudioStream(sa.activeAudioStream)
                    self.audioTrackSwitched(row: row, ok: ok)
                }
            }
        }
    }

    /// Apply a picker row — the index into whatever `subtitleTrackRows()` last returned.
    func selectSubtitleTrack(_ row: Int) {
        core.select_subtitle_track(UInt(row))
        kick()
    }

    /// `m:ss` under an hour, `h:mm:ss` above — mirrors the core's `format_video_duration`.
    static func formatTime(_ seconds: Double) -> String {
        let t = Int(seconds.rounded())
        let (h, m, s) = (t / 3600, (t % 3600) / 60, t % 60)
        return h > 0 ? String(format: "%d:%02d:%02d", h, m, s) : String(format: "%d:%02d", m, s)
    }

    // ── Native video callbacks (task 79.9 phase 2): the player reports its authoritative
    //    state back so the core proxy advances (P/toolbar pause/resume/replay, failures).
    //    Same drain pattern as menuAction — the core may emit effects (a toast, StopVideo).
    func nativeVideoOpened(_ sessionId: UInt64, durationMs: Int64, hasAudio: Bool) {
        core.native_video_opened(sessionId, durationMs, hasAudio)
        kick()
        drainEffects()
    }
    func nativeVideoStateChanged(_ sessionId: UInt64, state: UInt8) {
        core.native_video_state_changed(sessionId, state)
        kick()
        drainEffects()
        // Re-sync the toolbar Play glyph: unlike an animation (whose pump ticks
        // continuously and keeps syncToolbar running), a video's play/pause is an
        // isolated state change — without this the button only reflects motion_playing()
        // on the next unrelated event (a key press), so it never re-lights after a pause.
        syncToolbar()
    }
    func nativeVideoEnded(_ sessionId: UInt64) {
        core.native_video_ended(sessionId)
        pendingSeekTarget = nil  // EOS clears any settling-seek pin (plan §H3)
        kick()
        drainEffects()
        syncToolbar() // ended → no longer playing → drop the blue
    }
    func nativeVideoSeekCompleted(_ sessionId: UInt64, generation: UInt64, finished: Bool) {
        core.native_video_seek_completed(sessionId, generation, finished)
        kick()
        drainEffects()
    }
    func nativeVideoFailed(_ sessionId: UInt64, error: String, recoverable: Bool) {
        // A `recoverable` demux/codec failure retries through the FFmpeg session
        // core-side before any error surfaces (task #84 §8a level 2).
        core.native_video_failed(sessionId, error, recoverable)
        pendingSeekTarget = nil  // a failed seek/session clears the pin (plan §H3)
        kick()
        drainEffects()
        syncToolbar() // failure cleared the session → drop the blue
    }

    /// `CoreEffect::WriteClipboard` (via the marker + accessors): text goes on as a string;
    /// an image goes on as one pasteboard item carrying BOTH the rendered TIFF and — when
    /// the photo is a real on-disk file — its file URL, mirroring the Windows
    /// CF_DIBV5 + CF_HDROP pairing. The host toasts afterwards (winit-shell parity).
    private func writeClipboard() {
        let pb = NSPasteboard.general
        // A core-supplied toast (recognized-image text, task #45: "Copied 214 characters" /
        // "Copied text + 1 QR code") wins over the generic fallback. Read it BEFORE taking
        // the payload — `take_clipboard_text` consumes both. "" = none (e.g. Copy File Path).
        let coreToast = core.clipboard_text_toast().toString()
        let text = core.take_clipboard_text().toString()
        if !text.isEmpty {
            pb.clearContents()
            pb.setString(text, forType: .string)
            // Every text copy now carries its own specific toast (details / text / path /
            // description); this is just a safety net for any that doesn't.
            toast(coreToast.isEmpty ? "Copied to clipboard" : coreToast)
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
            toast("Copy failed")
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
        toast("Copied image")
    }

    /// `CoreEffect::SetWindowMode` — the borderless fullscreen **speed mode** (F), NOT
    /// macOS native Spaces fullscreen. Chromeless-but-key: keep `.titled` (a truly
    /// borderless NSWindow can't become key) and hide the titlebar + traffic lights
    /// instead; menu bar + Dock auto-hide while frontmost. NS3 replaces this with the
    /// bespoke window treatment.
    private func setWindowMode(fullscreen: Bool) {
        guard let window = hostWindow else { return }
        pbTrace("setWindowMode fullscreen=\(fullscreen) frame=\(window.frame)")
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
                // Grow to the screen and render the fullscreen-sized frame *inside one
                // CoreAnimation transaction*, so the small windowed drawable is never
                // stretched up to fill the enlarged layer for a frame — the "buttons
                // balloon" flash. The CAMetalLayer's default `.resize` gravity would
                // scale the stale contents to the new bounds; `display: false` suppresses
                // AppKit's immediate stretched composite, and `reportSizeNow` reads the
                // settled bounds to resize the surface + redraw before the commit.
                CATransaction.begin()
                CATransaction.setDisableActions(true)
                window.setFrame(screen.frame, display: false)
                // Settle the SwiftUI-driven canvas bounds *synchronously* so `reportSizeNow`
                // reads the new (fullscreen) size, not the stale windowed one — otherwise it
                // would render a small frame that the layer then stretches up to fill.
                window.contentView?.layoutSubtreeIfNeeded()
                canvasView?.reportSizeNow()
                CATransaction.commit()
            }
            window.makeKeyAndOrderFront(nil)
        } else {
            NSApp.presentationOptions = []
            if let mask = savedStyleMask {
                window.styleMask = mask
            }
            borderlessOK = false
            // Shrink back to the windowed frame and redraw at the new size inside one
            // CoreAnimation transaction: `display: false` + a synchronous `reportSizeNow`
            // means the fullscreen drawable is never squeezed into the smaller layer for a
            // frame, and — because `reportSizeNow` reads the settled bounds directly rather
            // than waiting for AppKit to re-fire the canvas view's `layout()` — the
            // empty-state Open panel re-centers immediately instead of staying placed for
            // the old (fullscreen) surface until a manual resize or hover.
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            if let frame = savedFrame {
                // Belt-and-braces: re-fit the remembered windowed frame to the window's
                // current screen before restoring, so an F exit can never re-impose a
                // frame larger than the screen it lands on (shrink-only; the common
                // same-screen round trip restores exactly).
                let visible = (window.screen ?? NSScreen.main)?.visibleFrame
                window.setFrame(
                    visible.map { Self.shrunkToFit(frame, in: $0) } ?? frame,
                    display: false
                )
            }
            // Settle the SwiftUI-driven canvas bounds *synchronously* before rendering:
            // on the shrink (+ restoring the titled style mask) the canvas view's bounds
            // don't update in-line with `setFrame`, so without this `reportSizeNow` reads
            // the stale fullscreen size and the layer squeezes that frame into the smaller
            // window — the shrunken-buttons regression.
            window.contentView?.layoutSubtreeIfNeeded()
            canvasView?.reportSizeNow()
            CATransaction.commit()
        }
        // The in-transaction report above read the bounds `layoutSubtreeIfNeeded` settled;
        // when SwiftUI instead defers the canvas layout to a later runloop pass, this
        // reconcile (and the per-tick one in `pump()`) re-reports the drifted size then —
        // a no-op when the synchronous path already got it right.
        DispatchQueue.main.async { [weak self] in
            self?.canvasView?.reconcileSizeIfNeeded()
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
        // Never mutate the window while a menu is open — a styleMask (or stoplight)
        // write cancels the tracking session, closing the menu in the user's face.
        // The didEndTracking observer re-runs this the moment the menu closes.
        guard menuTrackingDepth == 0 else { return }
        guard let window = hostWindow else { return }
        let fs = speedModeFullscreen
        // Translucent windowed toolbar (task #59 spike, PB_GLASS_TOOLBAR): extend the content
        // under a transparent toolbar so a zoomed/cropped photo shows under the glass — but
        // keep the title bar, traffic lights, and shadow (unlike the borderless speed mode).
        let glass = glassToolbar && !fs
        let wantFullSize = fs || glass
        let wantTransparent = fs || glass
        if fs, borderlessOK {
            // True borderless mode: keep it that way if SwiftUI re-adds `.titled`.
            if window.styleMask.contains(.titled) {
                log("chrome: styleMask ← [.borderless, .fullSizeContentView] (SwiftUI re-added .titled)")
                window.styleMask = [.borderless, .fullSizeContentView]
            }
        } else if window.styleMask.contains(.fullSizeContentView) != wantFullSize {
            log("chrome: styleMask.fullSizeContentView ← \(wantFullSize)")
            if wantFullSize {
                window.styleMask.insert(.fullSizeContentView)
            } else {
                window.styleMask.remove(.fullSizeContentView)
            }
        }
        // No shadow in F mode — its rim highlight reads as a border at the screen edge.
        if window.hasShadow != !fs {
            log("chrome: hasShadow ← \(!fs)")
            window.hasShadow = !fs
        }
        if window.titlebarAppearsTransparent != wantTransparent {
            log("chrome: titlebarAppearsTransparent ← \(wantTransparent)")
            window.titlebarAppearsTransparent = wantTransparent
        }
        let vis: NSWindow.TitleVisibility = fs ? .hidden : .visible
        if window.titleVisibility != vis {
            log("chrome: titleVisibility ← \(fs ? "hidden" : "visible")")
            window.titleVisibility = vis
        }
        let sep: NSTitlebarSeparatorStyle = (fs || glass) ? .none : .automatic
        if window.titlebarSeparatorStyle != sep {
            log("chrome: titlebarSeparatorStyle ← \((fs || glass) ? "none" : "automatic")")
            window.titlebarSeparatorStyle = sep
        }
        for kind in [NSWindow.ButtonType.closeButton, .miniaturizeButton, .zoomButton] {
            if let button = window.standardWindowButton(kind), button.isHidden != fs {
                log("chrome: stoplight \(kind.rawValue) isHidden ← \(fs)")
                button.isHidden = fs
            }
        }
        // Re-assert the toolbar if SwiftUI swapped its own in during a scene update — the
        // same defeat-the-framework pattern used above (identity check preserves the user's
        // Hide-Toolbar choice, which keeps *our* object, just hidden). Only after the
        // deferred initial install has landed: a synchronous re-assert before then would
        // re-trigger the realization-time crash the defer exists to avoid.
        if toolbarInstalled {
            toolbarController?.reassertIfClobbered(on: window, speedMode: fs)
        }
        // Tell the renderer how tall the glass bar is, so the photo fits below it (task #59).
        updateContentTopInset()
    }

    /// The transparent-toolbar mode (task #59): windowed mode extends the canvas under a
    /// translucent toolbar so a zoomed/cropped photo shows under the glass (fit mode unchanged),
    /// with a legibility scrim. Driven by the `glass_toolbar` setting (default on) — refreshed
    /// on load + on every settings edit (`refreshGlassToolbar`), so the Settings toggle applies
    /// live.
    private(set) var glassToolbar = true

    func refreshGlassToolbar() {
        glassToolbar = core.settings_form().glass_toolbar
    }

    /// Last inset pushed to the core (physical px), so we only re-send + repaint on a change.
    @ObservationIgnored private var lastContentTopInsetPx: UInt32 = .max

    /// The glass bar's height in **points** (0 when off) — drives the SwiftUI legibility scrim.
    private(set) var glassTopInsetPoints: CGFloat = 0

    /// Whether the top legibility scrim should show: the glass toolbar is on, windowed, and
    /// there's an actual bar to sit under.
    var glassScrimVisible: Bool {
        glassToolbar && !speedModeFullscreen && glassTopInsetPoints > 1
    }

    /// Compute the glass bar's height from `contentLayoutRect` (the area below the titlebar +
    /// toolbar) and hand it to the renderer (px) + the scrim overlay (points). `0` when the
    /// spike is off or in speed mode.
    private func updateContentTopInset() {
        guard let window = hostWindow else { return }
        var pts: CGFloat = 0
        if glassToolbar, !speedModeFullscreen, let content = window.contentView {
            pts = max(0, content.bounds.height - window.contentLayoutRect.height)
        }
        if abs(pts - glassTopInsetPoints) > 0.5 { glassTopInsetPoints = pts }
        let px = UInt32((pts * window.backingScaleFactor).rounded())
        guard px != lastContentTopInsetPx else { return }
        lastContentTopInsetPx = px
        core.set_content_top_inset(px)
        core.render() // repaint at the new inset
    }
}
