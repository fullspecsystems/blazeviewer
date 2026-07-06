import SwiftUI

/// PhotoBlaze's native macOS host (NS1, ADR-021) — slice 1: a minimal SwiftUI app that
/// owns the Rust `AppCore` over the swift-bridge FFI, forwards key events in, and drains
/// the effects out. The wgpu canvas (item 2), real photo source (item 3), and the rest of
/// the event/effect surface land in the following slices; the egui-on-Mac beta remains the
/// shippable Mac artifact until the NS3 cutover.
/// Early activation for **bare-binary launches** (`swift run`, running the executable
/// straight out of the .app — the dev loop), so they front + focus like a Finder launch.
/// Also the future home of `application:openURLs:`/`openFile:` (NS1 item 4).
///
/// NOTE the launch gotcha that was chased for a whole evening: it was NOT activation —
/// AppKit treats a bare path in `argv[1]` as a document-open launch and suppresses the
/// initial `WindowGroup` window entirely (windowless app, live menu bar). Hence the
/// `--pb-open <path>` flag (see `CoreModel.openLaunchPathIfAny`).
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

@main
struct PhotoBlazeMacApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @State private var model = CoreModel()

    var body: some Scene {
        WindowGroup("PhotoBlaze") {
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

    private static let contentSpace = "content"

    /// A corner panel only reserves room above the info line if it *actually* overlaps it
    /// horizontally (a wide tree + a right-aligned line don't; a narrow one leaves the line
    /// alone). Vertical overlap is a given — the panel is exactly what we're capping. A small
    /// tolerance keeps a hairline touch from shrinking a panel (margin of error is fine here).
    private func overlapsInfoLine(_ panel: CGRect) -> Bool {
        guard model.infoLineVisible, !infoLineFrame.isEmpty, !panel.isEmpty else { return false }
        let tol: CGFloat = 8
        return panel.maxX > infoLineFrame.minX + tol && panel.minX < infoLineFrame.maxX - tol
    }

    /// A corner panel's max height: the full window height minus the top+bottom edge insets,
    /// but capped to stop `edge` above the info line when it would otherwise collide with it.
    private func panelMaxHeight(_ panel: CGRect) -> CGFloat {
        let full = max(120, contentSize.height - 2 * Layout.edge)
        guard overlapsInfoLine(panel) else { return full }
        return max(120, min(full, infoLineFrame.minY - 2 * Layout.edge))
    }

    /// The toast sits `edge` above the info line (or `edge` off the bottom when it's hidden),
    /// so the toast→line and line→bottom gaps match.
    private var toastBottomInset: CGFloat {
        guard model.infoLineVisible, !infoLineFrame.isEmpty else { return Layout.edge }
        return (contentSize.height - infoLineFrame.minY) + Layout.edge
    }

    var body: some View {
        MetalCanvas(model: model)
            // The borderless speed mode (F) keeps `.titled` (a borderless NSWindow can't
            // become key) and instead extends content under a transparent titlebar
            // (`.fullSizeContentView`). SwiftUI still insets for that titlebar's safe
            // area, which left a titlebar-height strip visible — ignoring it lets the
            // canvas truly fill the screen. A no-op in windowed mode (the content rect
            // excludes the titlebar there, so there's no inset to ignore).
            .ignoresSafeArea()
            .frame(minWidth: 520, minHeight: 360)
            // The canvas fills the window, so its size is the layout's content size — the
            // basis for every overlay's shared-margin math.
            .onGeometryChange(for: CGSize.self) { $0.size } action: { contentSize = $0 }
            // The empty-state welcome surface (task #54): shown when no photos are
            // loaded; hidden while Help is up (Help takes the center). A native view, so
            // its buttons own their hover/click — no HUD hit-rect leaks under a panel.
            .overlay(alignment: .center) {
                if model.openPanelVisible && !model.helpVisible {
                    EmptyStateView(model: model)
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
                        maxWidth: max(360, contentSize.width - 80)
                    )
                    .onGeometryChange(for: CGRect.self) {
                        $0.frame(in: .named(Self.contentSpace))
                    } action: { inspectorFrame = $0 }
                    .frame(
                        maxWidth: .infinity, maxHeight: .infinity,
                        alignment: .topTrailing
                    )
                    .padding(.trailing, Layout.edge)
                    .padding(.top, Layout.edge)
                    .transition(.opacity)
                }
            }
            // The folder tree rides the leading edge (where the HUD tree sat), top-aligned.
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
                    .padding(.leading, Layout.edge)
                    .padding(.top, Layout.edge)
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
                        .padding(.top, Layout.edge)
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
                        .padding(.horizontal, Layout.edge)
                        .padding(.bottom, Layout.edge)
                        .allowsHitTesting(false)
                }
            }
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
            // Help last = topmost: it's an ephemeral reference sheet centered over the
            // photo, so it should occlude the tree/inspector (which it overlaps) rather
            // than slide under them.
            .overlay {
                if model.helpVisible {
                    HelpPanelView(
                        sections: model.helpSections,
                        onClose: { model.closeHelp() },
                        maxHeight: max(120, contentSize.height - 2 * Layout.edge)
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
            .onAppear {
                // After SwiftUI has installed its own main menu, replace it with ours.
                model.installMenuBarIfNeeded()
                // A forced Light/Dark Theme preference (#46) overrides the app's
                // appearance from the first frame; System leaves the OS in charge.
                model.applyAppearancePreference()
                model.refreshPanelOpacity()  // the saved "Panel opacity" for the chrome
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
