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
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.activate()
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
        // Settings windows default to rigid content-size; let this one resize from the
        // content minimums up (the Shortcuts tab is a long list).
        .windowResizability(.contentMinSize)
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
                model.openLaunchPathIfAny()
                model.openSettingsAction = { openSettings() }
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
                case .loading: LoadingSheetView(model: model)
                case .scanning: ScanningSheetView(model: model)
                }
            }
    }
}
