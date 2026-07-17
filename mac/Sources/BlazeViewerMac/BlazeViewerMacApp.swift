import SwiftUI

/// Blaze Viewer's native macOS host (NS1, ADR-021) — slice 1: a minimal SwiftUI app that
/// owns the Rust `AppCore` over the swift-bridge FFI, forwards key events in, and drains
/// the effects out. The wgpu canvas (item 2), real photo source (item 3), and the rest of
/// the event/effect surface land in the following slices; the egui-on-Mac beta remains the
/// shippable Mac artifact until the NS3 cutover.
/// Early activation for **bare-binary launches** (`swift run`, running the executable
/// straight out of the .app — the dev loop), so they front + focus like a Finder launch.
/// Also the future home of `application:openURLs:`/`openFile:` (NS1 item 4).
///
/// NOTE the launch gotcha that was chased for a whole evening: it was NOT activation —
/// AppKit treats a bare path in `argv[1]` as a document-open launch and (racily)
/// suppresses the initial `WindowGroup` window (windowless app, live menu bar).
/// **Resolved** (task #78.10): `BlazeViewerMacApp.init` registers
/// `NSTreatUnknownArgumentsAsOpen = NO`, so AppKit never converts argv paths to
/// document-opens — the shared pb-cli parser owns argv, and `blaze ~/Photos`
/// works bare. `--pb-open <path>` survives as a hidden compat alias.
final class AppDelegate: NSObject, NSApplicationDelegate {
    /// Finder / Dock / "Open with" URLs. May fire before the model installs its handler
    /// (a cold double-click launch), so early arrivals are buffered and replayed.
    @MainActor private static var openHandler: (([URL]) -> Void)?
    @MainActor private static var pendingURLs: [URL] = []

    @MainActor static func installOpenHandler(_ handler: @escaping ([URL]) -> Void) {
        openHandler = handler
        if !pendingURLs.isEmpty {
            handler(pendingURLs)
            pendingURLs.removeAll()
        }
    }

    func applicationWillFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
        // A single-window viewer: no window tabs — this also stops AppKit auto-injecting
        // "Show Tab Bar"/"Show All Tabs" atop our View menu (winit-menu parity).
        NSWindow.allowsAutomaticWindowTabbing = false
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.activate()
        // Start Sparkle's background update scheduler (task #65). No-op unless this bundle
        // carries a feed URL (a real assembled .app, not a bare `swift run`) — see Updater.
        Updater.shared.startIfEnabled()
    }

    /// PhotoBlaze is a single-window app by design (task #48, owner call): once the last
    /// window closes there is nothing left to interact with — ContentView's `.onAppear`
    /// wiring (menu bar, FFI bridge, open handlers) never re-runs, so the alternative is
    /// a dead windowless process where File ▸ Open silently does nothing. Quitting routes
    /// through the exact `NSApp.terminate` path Esc already uses: RAM caches drop with the
    /// process, no flush-to-disk step exists (the no-trace guarantee). ⌘W on Settings or
    /// About while the viewer is open still just closes that window — this only fires for
    /// the *last* one.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    func application(_ application: NSApplication, open urls: [URL]) {
        MainActor.assumeIsolated {
            if let handler = Self.openHandler {
                handler(urls)
            } else {
                Self.pendingURLs.append(contentsOf: urls)
            }
        }
    }
}

/// The product's display name for every Swift-side surface — the app menu, window titles,
/// alerts, Settings copy.
///
/// Read from the bundle (`CFBundleName`, set in `packaging/macos/Info-swift-host.plist`)
/// rather than hardcoded, so it can never drift from the bundle's own identity. Mirrors
/// `pb_app_core::APP_NAME` on the Rust side; the fallback covers a bare `swift run` with
/// no assembled bundle. The rename (task #101) found this hardcoded in eight places,
/// including the macOS app menu — hence the single source.
let appName: String =
    Bundle.main.object(forInfoDictionaryKey: "CFBundleName") as? String ?? "Blaze Viewer"

@main
struct BlazeViewerMacApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @State private var model: CoreModel

    init() {
        // Kill AppKit's argv-as-documents behavior (the "great windowless-app hunt",
        // resolved): with it on, a bare path in argv[1] becomes a document-open launch
        // that BOTH re-delivers the path as an odoc Apple Event AND — racily —
        // suppresses the initial WindowGroup window (release builds usually win that
        // race; debug builds reliably lose it: windowless app, live menu bar —
        // measured 2026-07-12). We parse argv ourselves (the shared pb-cli surface),
        // so the AppKit scan is pure hazard. Finder / Dock / `open` launches arrive
        // as Apple Events, not argv — unaffected. Registered before NSApplication
        // finishes launching, which is when the scan reads it.
        UserDefaults.standard.register(defaults: ["NSTreatUnknownArgumentsAsOpen": "NO"])
        // The CLI preflight (task #78) runs before ANYTHING ELSE: a terminal `--help` /
        // `--version` / usage error prints and exits here — no window, no Sparkle, no
        // decode-pool engine. `model` is deliberately constructed in this init (not a
        // property initializer, which would run first) so a non-proceed launch never
        // builds it. On proceed, CoreModel.init feeds the same argv through
        // `apply_launch_args` for the session overrides.
        Launch.act(on: Launch.preflight())
        // `--pb-door-shot <dir>`: render the door card to PNGs and exit — before the
        // window, deliberately. It shot from `onAppear` at first, and hung forever on a
        // Mac whose console was at the login window: no session, no scene, no `onAppear`.
        // The whole point of the harness is to see the card where the screen can't be
        // captured, so it must not need a screen to run.
        DoorShot.runIfRequested(artwork: CoreModel.loadDoorArtwork())
        _model = State(initialValue: CoreModel())
    }

    var body: some Scene {
        WindowGroup(appName) {
            ContentView(model: model)
        }
        // ⌘, / App menu ▸ Settings… — reached via ShowDialog("settings") →
        // `openSettingsAction` (installed by ContentView; only a view can grab the
        // environment's openSettings action).
        Settings {
            SettingsView(model: model)
        }
        // The window exactly fits SettingsView's fixed frame — not resizable (a
        // deliberate punt after five failed attempts at vertical-only resizing; see
        // the note on SettingsView's frame).
        .windowResizability(.contentSize)
    }
}

/// The viewer window: the wgpu canvas fills it edge to edge (the NS1 effect-log debug
/// chrome retired with the NS2 dialogs — the FFI trace still prints on a terminal launch),
/// with the NS2 dialog sheets presented over it.
struct ContentView: View {
    let model: CoreModel
    @Environment(\.openSettings) private var openSettings

    // Measured geometry (in the "content" coordinate space) that drives the shared-margin
    // layout: the info line's pill frame, the two corner panels' frames, and the canvas size.
    // The info line is measured independently of the panels, so using it to size/position them
    // (and the toast) is a one-way dependency — no layout feedback loop.
    @State private var contentSize: CGSize = .zero
    @State private var infoLineFrame: CGRect = .zero
    @State private var treeFrame: CGRect = .zero
    @State private var inspectorFrame: CGRect = .zero

    /// The one shared spacing the whole chrome lays out on — panel↔edge, info-line↔edge,
    /// toast↔info-line, etc. On macOS 26+ `WindowCornerInsetReader` drives it from the OS
    /// window corner radius (so it tracks the corners and never drifts); seeded with — and, on
    /// older systems, held at — `Layout.edge`.
    @State private var edge: CGFloat = Layout.edge

    private static let contentSpace = "content"

    /// A corner panel only reserves room above the info line if it *actually* overlaps it
    /// horizontally (a wide tree + a right-aligned line don't; a narrow one leaves the line
    /// alone). Vertical overlap is a given — the panel is exactly what we're capping. A small
    /// tolerance keeps a hairline touch from shrinking a panel (margin of error is fine here).
    private func overlapsInfoLine(_ panel: CGRect) -> Bool {
        guard model.infoLineVisible, !infoLineFrame.isEmpty, !panel.isEmpty else { return false }
        // Fire the cap a little BEFORE the panel actually touches the info line — the measured
        // frames don't fully account for the panel's border/shadow, so a small safety margin
        // guarantees they never visibly overlap (cheaper than under-shrinking and touching).
        let safety: CGFloat = 8
        return panel.maxX > infoLineFrame.minX - safety && panel.minX < infoLineFrame.maxX + safety
    }

    /// A corner panel's max height: the full window height minus the top+bottom edge insets,
    /// but capped to stop `edge` above the info line when it would otherwise collide with it.
    private func panelMaxHeight(_ panel: CGRect) -> CGFloat {
        let full = max(120, contentSize.height - 2 * edge)
        guard overlapsInfoLine(panel) else { return full }
        return max(120, min(full, infoLineFrame.minY - 2 * edge))
    }

    /// The area the door card lays itself out in: the whole window minus the edge insets.
    ///
    /// Deliberately **not** the space between open side panels. Centring in that gap makes
    /// the card drift toward whichever panel is narrower (owner, 2026-07-17: with a wide
    /// Details panel it "just mostly looks off"), a space-between feel where the eye expects
    /// dead-centre. So the card is always centred in the window, and if a wide panel on a
    /// narrow window ever overlaps it, the panel simply draws on top (the card sits below
    /// every panel in the stack) — a rare, graceful collision the owner prefers to a
    /// permanently off-centre card.
    private var doorCardArea: CGSize {
        CGSize(
            width: max(0, contentSize.width - 2 * edge),
            height: max(0, contentSize.height - 2 * edge))
    }

    /// The toast sits `edge` above the info line (or `edge` off the bottom when it's hidden),
    /// so the toast→line and line→bottom gaps match.
    private var toastBottomInset: CGFloat {
        guard model.infoLineVisible, !infoLineFrame.isEmpty else { return edge }
        return (contentSize.height - infoLineFrame.minY) + edge
    }

    var body: some View {
        MetalCanvas(model: model)
            // The borderless speed mode (F) keeps `.titled` (a borderless NSWindow can't
            // become key) and instead extends content under a transparent titlebar
            // (`.fullSizeContentView`). SwiftUI still insets for that titlebar's safe
            // area, which left a titlebar-height strip visible — ignoring it lets the
            // canvas truly fill the screen. A no-op in windowed mode (the content rect
            // excludes the titlebar there, so there's no inset to ignore).
            //
            // Subtitles (task #90) ride the canvas itself, not the chrome chain: the core
            // places them in the CANVAS's space (full window, physical px ÷ scale), and this
            // is the only place that shares it. Attached here — inside the canvas's own
            // safe-area escape, before any chrome overlay — the two agree exactly.
            //
            // Measured, because it isn't obvious: attached further down the chain the
            // overlay starts at y=32 and is 1734pt tall against the canvas's 1786, so every
            // subtitle rides ~52pt low. That's invisible until a zoomed (or Crop-to-Fill)
            // video clamps the block flush to the bottom — and then the last line is cut off
            // by exactly the inset. Putting it here fixes the cause instead of the symptom,
            // and leaves the panels / scrim / info line untouched.
            .overlay(alignment: .topLeading) {
                if let img = model.subtitleImage {
                    Image(nsImage: img)
                        .offset(x: model.subtitleRect.minX, y: model.subtitleRect.minY)
                        .allowsHitTesting(false)
                }
            }
            .ignoresSafeArea()
            .frame(minWidth: 520, minHeight: 360)
            // Track the OS window-corner radius for the shared edge gap (macOS 26+). Full-bleed
            // behind the canvas, so its corner-adaptive insets are the window's; older systems
            // keep `edge`.
            .background {
                if #available(macOS 26.0, *) {
                    WindowCornerInsetReader { edge = $0 }
                }
            }
            // The canvas fills the window, so its size is the layout's content size — the
            // basis for every overlay's shared-margin math.
            .onGeometryChange(for: CGSize.self) { $0.size } action: { contentSize = $0 }
            // The translucent-toolbar legibility scrim (task #59) — the lowest overlay, so the
            // panels / title / toast all draw over it rather than being tinted by it. Only
            // present in the glass-toolbar mode; sized to the bar.
            .overlay(alignment: .top) {
                if model.glassScrimVisible {
                    GlassTopScrim(height: model.glassTopInsetPoints)
                }
            }
            // The empty-state welcome surface (task #54): shown when no photos are
            // loaded; hidden while Help is up (Help takes the center). A native view, so
            // its buttons own their hover/click — no HUD hit-rect leaks under a panel.
            .overlay(alignment: .center) {
                if model.openPanelVisible && !model.helpVisible {
                    EmptyStateView(model: model)
                        .transition(.opacity)
                }
            }
            // The archive door card (task #105) is **content chrome**: it stands in for the
            // photo that isn't there (a door's frame is a 1×1 transparent sentinel), so it
            // draws here — above the empty canvas, below every real panel, which then
            // overlaps it exactly as it would overlap a photo. Centred in the *unobstructed*
            // area: an open side pane shifts it rather than sitting on top of it.
            //
            // No fade timer and no hover-to-hold, unlike the play hint it replaced for
            // archives: the card is the item's content, not a nag about it, so it stays for
            // as long as the door is on screen — including while blazing, where hiding it
            // would leave an entirely blank screen.
            .overlay {
                if model.doorVisible {
                    DoorCardView(model: model, available: doorCardArea)
                        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .center)
                        .transition(.opacity)
                }
            }
            // Native rich panels (task #54, mac-first) layer over the canvas here. The
            // core suppresses their HUD rasterization, so there's no double-draw; the
            // panel receives its own pointer/scroll (SwiftUI hit-tests it above the
            // canvas) while the rest falls through to pan/zoom/nav.
            //
            // The Inspector rides the top-trailing corner (parallel to the folder tree,
            // and top-anchored so switching tabs doesn't shift it). Help is mutually
            // exclusive with it in the core, so they never co-show. GeometryReader hands
            // it the available height for fit-to-content-then-scroll.
            .overlay {
                if model.inspectorVisible {
                    InspectorPanelView(
                        model: model,
                        maxHeight: panelMaxHeight(inspectorFrame),
                        maxWidth: max(280, contentSize.width - 80)
                    )
                    .onGeometryChange(for: CGRect.self) {
                        $0.frame(in: .named(Self.contentSpace))
                    } action: { inspectorFrame = $0 }
                    .frame(
                        maxWidth: .infinity, maxHeight: .infinity,
                        alignment: .topTrailing
                    )
                    .padding(.trailing, edge)
                    .padding(.top, edge)
                    .transition(.opacity)
                }
            }
            // The folder tree rides the leading edge (where the HUD tree sat), top-aligned.
            // The Thumbnails strip (task #83) is the same pane's second tab, so it mounts
            // in the same slot — the core guarantees at most one of the two is visible.
            .overlay {
                if model.treeVisible {
                    FolderTreePanelView(
                        model: model,
                        maxHeight: panelMaxHeight(treeFrame),
                        maxWidth: max(280, contentSize.width - 80)
                    )
                    .onGeometryChange(for: CGRect.self) {
                        $0.frame(in: .named(Self.contentSpace))
                    } action: { treeFrame = $0 }
                    .frame(
                        maxWidth: .infinity, maxHeight: .infinity,
                        alignment: .topLeading
                    )
                    .padding(.leading, edge)
                    .padding(.top, edge)
                    .transition(.opacity)
                } else if model.thumbsVisible {
                    ThumbnailsPanelView(
                        model: model,
                        maxHeight: panelMaxHeight(treeFrame),
                        maxWidth: max(280, contentSize.width - 80)
                    )
                    .onGeometryChange(for: CGRect.self) {
                        $0.frame(in: .named(Self.contentSpace))
                    } action: { treeFrame = $0 }
                    .frame(
                        maxWidth: .infinity, maxHeight: .infinity,
                        alignment: .topLeading
                    )
                    .padding(.leading, edge)
                    .padding(.top, edge)
                    .transition(.opacity)
                }
            }
            // The ambient scan pill (④) rides the top-center, above the canvas and the
            // corner panels but below Help — non-blocking, so you browse the streamed-in
            // photos while it walks. Shown only past the reveal delay (no flash on a fast
            // folder). Ignores the safe area so it clears the transparent titlebar strip.
            .overlay(alignment: .top) {
                if model.scanPillVisible {
                    ScanPillView(model: model)
                        .padding(.top, edge)
                        .transition(.move(edge: .top).combined(with: .opacity))
                }
            }
            .animation(.easeInOut(duration: 0.2), value: model.scanPillVisible)
            // The native one-line info readout (`i`), in the bottom corner the Settings
            // alignment picks (right by default). Non-interactive; the toast rides above it.
            .overlay(alignment: infoLineAlignment(model.infoLineAlign)) {
                if model.infoLineVisible {
                    InfoLineView(model: model)
                        .onGeometryChange(for: CGRect.self) {
                            $0.frame(in: .named(Self.contentSpace))
                        } action: { infoLineFrame = $0 }
                        .padding(.horizontal, edge)
                        .padding(.bottom, edge)
                        // Interactive only when the video playback row is present (its
                        // play/pause + scrubber need clicks/drags); otherwise the corner pill
                        // stays click-through so it never swallows canvas input (task 79.9).
                        .allowsHitTesting(model.videoControlsVisible)
                        // Explicit (was the implicit default) so it reads the same as the
                        // corner panels; the fade is driven by `withAnimation` in the model.
                        .transition(.opacity)
                }
            }
            // The play hint rides the same bottom-center spot as the toast, just above the info
            // line. Unlike the toast it IS interactive (hover holds it, click plays), so it
            // keeps hit-testing. They rarely co-occur; the toast layers above it when they do.
            .overlay(alignment: .bottom) {
                if model.playHintVisible {
                    PlayHintView(model: model)
                        .padding(.bottom, toastBottomInset)
                        .transition(.opacity)
                }
            }
            .animation(.easeInOut(duration: 0.22), value: model.playHintVisible)
            // The unified native toast rides the bottom-center, above where the info line
            // sits, and topmost so transient feedback (copy, rotate, "Scan stopped", …) is
            // never occluded by a panel. Non-interactive; it fades itself out.
            .overlay(alignment: .bottom) {
                if model.toastVisible {
                    ToastView(model: model)
                        .padding(.bottom, toastBottomInset)
                        .transition(.move(edge: .bottom).combined(with: .opacity))
                        .allowsHitTesting(false)
                }
            }
            .animation(.easeOut(duration: 0.22), value: model.toastVisible)
            .animation(.easeInOut(duration: 0.2), value: toastBottomInset)
            // The "Press F to exit fullscreen" hint — bottom-center (like the toast), shown
            // when speed mode is entered by mouse. Non-interactive; the model fades it after 6s.
            .overlay(alignment: .bottom) {
                if model.fullscreenHintVisible {
                    FullscreenHintView(model: model)
                        .padding(.bottom, toastBottomInset)
                        .transition(.opacity)
                        .allowsHitTesting(false)
                }
            }
            .animation(.easeInOut(duration: 0.3), value: model.fullscreenHintVisible)
            // Help last = topmost: it's an ephemeral reference sheet centered over the
            // photo, so it should occlude the tree/inspector (which it overlaps) rather
            // than slide under them.
            .overlay {
                if model.helpVisible {
                    HelpPanelView(
                        sections: model.helpSections,
                        onClose: { model.closeHelp() },
                        maxHeight: max(120, contentSize.height - 2 * edge)
                    )
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .center)
                    .transition(.opacity)
                }
            }
            // Frames above are measured in this space so the shared-margin math (panel caps,
            // toast offset) is all in one consistent coordinate system.
            .coordinateSpace(.named(Self.contentSpace))
            // SwiftUI owns the WindowGroup titlebar surface and repaints it over the
            // AppKit-side transparency during its own update passes — hide it through
            // SwiftUI itself while the F speed mode is on (the AppKit props are also
            // re-asserted each drain; both are needed to keep the bar gone on Tahoe).
            .toolbarBackground(
                model.speedModeFullscreen ? Visibility.hidden : .automatic,
                for: .windowToolbar
            )
            // Drive the titlebar filename + "N of M" subtitle through SwiftUI — it owns the
            // WindowGroup titlebar and would otherwise clobber a direct AppKit `window.subtitle`
            // write (the subtitle vanished; see `CoreModel.applyWindowTitle`).
            .navigationTitle(model.windowTitleText)
            .navigationSubtitle(model.windowSubtitleText)
            .onAppear {
                // After SwiftUI has installed its own main menu, replace it with ours.
                model.installMenuBarIfNeeded()
                // A forced Light/Dark Theme preference (#46) overrides the app's
                // appearance from the first frame; System leaves the OS in charge.
                model.applyAppearancePreference()
                model.refreshPanelOpacity()  // the saved "Panel opacity" for the chrome
                model.refreshGlassToolbar()  // the saved "Transparent toolbar" default (#59)
                model.openLaunchPathIfAny()
                model.openSettingsAction = { openSettings() }
                model.runFSmokeIfRequested()
            }
            // Password / Loading / Scanning ride one item-driven sheet. The binding's
            // nil-write is the *user* dismissal signal (Esc/etc. beyond the buttons);
            // a programmatic CloseDialog clears activeSheet directly and never lands here.
            .sheet(
                item: Binding(
                    get: { model.activeSheet },
                    set: { if $0 == nil { model.userDismissedSheet() } }
                )
            ) { kind in
                switch kind {
                case .password: PasswordSheetView(model: model)
                case .ask: AskSheetView(model: model)
                case .loading: LoadingSheetView(model: model)
                case .scanning: ScanningSheetView(model: model)
                }
            }
    }
}

/// Bottom-corner placement for the native info line, from the Settings alignment
/// (0 = left, 1 = center, 2 = right — the default).
private func infoLineAlignment(_ align: Int) -> Alignment {
    switch align {
    case 0: return .bottomLeading
    case 1: return .bottom
    default: return .bottomTrailing
    }
}
