import AppKit
import AVFoundation
import Observation
import PbMacFfi
import QuartzCore
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
    /// User-resizable panel widths (drag the inner edge). The defaults are the minimums;
    /// session-persistent (survive close/reopen) — disk persistence is a later slice.
    var treeWidth: CGFloat = 280
    var inspectorWidth: CGFloat = 360
    /// An NSAlert sheet (confirm/message) is up — gates the key monitor like `panelOpen`.
    @ObservationIgnored private var alertUp = false
    /// Opens the SwiftUI Settings scene — injected by the root view (`openSettings` is an
    /// Environment action only a view can reach).
    @ObservationIgnored var openSettingsAction: (() -> Void)?

    @ObservationIgnored private var keyMonitor: Any?
    @ObservationIgnored private var focusObserver: NSObjectProtocol?
    @ObservationIgnored private var keyLossObserver: NSObjectProtocol?
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
            return event.modifierFlags.contains(.command) ? event : nil
        }

        // The focus-loss release net: held keys are cleared so nothing keeps flying —
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
            self?.openPaths(urls.map(\.path))
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
                after(1.5) { [self] in
                    dump("final")
                    NSApp.terminate(nil)
                }
            }
        }
    }

    @ObservationIgnored private var fSmokeStarted = false

    /// A native menu item fired (by stable Action id) — same dispatch as the keyboard.
    func menuAction(_ id: String) {
        core.menu_action(id)
        kick()
        drainEffects()
    }

    /// Re-pull the native Help panel model after a `PanelsChanged` marker: its
    /// visibility and (when visible) its rows, flattened from the core's live keymap.
    private func refreshHelp() {
        core.help_refresh()
        helpVisible = core.help_visible()
        guard helpVisible else {
            helpSections = []
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
        inspectorVisible = core.inspector_visible()
        guard inspectorVisible else {
            inspectorRows = []
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

    /// Re-pull the native folder tree after a `PanelsChanged` marker: visibility and (when
    /// visible) the current folder's hierarchy rows, as derived by the core.
    private func refreshTree() {
        treeVisible = core.tree_visible()
        guard treeVisible else {
            treeRows = []
            currentTreePath = ""
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

    // MARK: - Empty-state panel actions (task #54)

    /// Open File / Open Folder from the welcome surface — the same commands as the menu
    /// and the O / ⇧O keys (open state stays in the core).
    func openFile() { menuAction("open_file") }
    func openFolder() { menuAction("open_folder") }
    /// The welcome surface's "See all shortcuts" link toggles the Help panel.
    func showAllShortcuts() { menuAction("help") }

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
                string: "github.com/jdlien/photoblaze",
                attributes: [
                    .font: NSFont.systemFont(ofSize: NSFont.smallSystemFontSize),
                    .link: URL(string: "https://github.com/jdlien/photoblaze")!,
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
        kick()
        drainEffects()
    }

    /// Apply the Appearance preference (#46) to the whole app: forced Light/Dark set
    /// an `NSApp.appearance` override (so native chrome matches the HUD); System
    /// clears it, letting the OS theme through. The canvas's
    /// `viewDidChangeEffectiveAppearance` then reports the resulting effective theme
    /// back to the core, which keeps `Appearance: System` resolving live.
    func applyAppearancePreference() {
        switch core.settings_form().appearance_mode {
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
        applyStartupWindowState()
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
        case .SetWindowMode(let fullscreen):
            log("SetWindowMode(fullscreen: \(fullscreen))")
            setWindowMode(fullscreen: fullscreen)
        case .HideWindow:
            hostWindow?.orderOut(nil)
        case .MenuStateChanged:
            menuBar?.sync(core.menu_state())
            reassertMenuBar() // belt-and-braces beside the KVO watch
        case .PanelsChanged:
            // A natively-presented panel changed — re-pull the Help model + visibility,
            // the empty-state Open panel's visibility, and the Inspector (visibility +
            // tab + rows; async OCR / describe results re-signal here too).
            refreshHelp()
            openPanelVisible = core.open_panel_visible()
            refreshInspector()
            refreshTree()
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
            NSLog("PhotoBlaze: NSApp.terminate was deferred — hard exit (zombie watchdog)")
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
            let toastMsg =
                !coreToast.isEmpty
                ? coreToast
                : (text.contains("\n")
                    ? "Copied to clipboard"
                    : "Copied \((text as NSString).lastPathComponent)")
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
        if fs, borderlessOK {
            // True borderless mode: keep it that way if SwiftUI re-adds `.titled`.
            if window.styleMask.contains(.titled) {
                log("chrome: styleMask ← [.borderless, .fullSizeContentView] (SwiftUI re-added .titled)")
                window.styleMask = [.borderless, .fullSizeContentView]
            }
        } else if window.styleMask.contains(.fullSizeContentView) != fs {
            log("chrome: styleMask.fullSizeContentView ← \(fs)")
            if fs {
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
        if window.titlebarAppearsTransparent != fs {
            log("chrome: titlebarAppearsTransparent ← \(fs)")
            window.titlebarAppearsTransparent = fs
        }
        let vis: NSWindow.TitleVisibility = fs ? .hidden : .visible
        if window.titleVisibility != vis {
            log("chrome: titleVisibility ← \(fs ? "hidden" : "visible")")
            window.titleVisibility = vis
        }
        let sep: NSTitlebarSeparatorStyle = fs ? .none : .automatic
        if window.titlebarSeparatorStyle != sep {
            log("chrome: titlebarSeparatorStyle ← \(fs ? "none" : "automatic")")
            window.titlebarSeparatorStyle = sep
        }
        for kind in [NSWindow.ButtonType.closeButton, .miniaturizeButton, .zoomButton] {
            if let button = window.standardWindowButton(kind), button.isHidden != fs {
                log("chrome: stoplight \(kind.rawValue) isHidden ← \(fs)")
                button.isHidden = fs
            }
        }
    }
}
