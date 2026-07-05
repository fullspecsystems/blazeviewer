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
            // canvas) while the rest falls through to pan/zoom/nav. Help is the pilot.
            .overlay {
                if model.helpVisible {
                    // GeometryReader gives the panel the window's available height so it
                    // sizes to its content up to that, then scrolls; centered within.
                    GeometryReader { geo in
                        HelpPanelView(
                            sections: model.helpSections,
                            onClose: { model.closeHelp() },
                            maxHeight: geo.size.height - 48
                        )
                        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .center)
                    }
                    .transition(.opacity)
                }
            }
            // The Inspector rides the trailing edge like a sidebar (Help is mutually
            // exclusive with it in the core, so they never co-show). GeometryReader hands
            // it the available height for fit-to-content-then-scroll.
            .overlay {
                if model.inspectorVisible {
                    GeometryReader { geo in
                        InspectorPanelView(model: model, maxHeight: geo.size.height - 48)
                            .frame(
                                maxWidth: .infinity, maxHeight: .infinity,
                                alignment: .trailing
                            )
                            .padding(.trailing, 24)
                    }
                    .transition(.opacity)
                }
            }
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
