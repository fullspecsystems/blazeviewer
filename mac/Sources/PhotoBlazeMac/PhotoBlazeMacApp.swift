import SwiftUI

/// PhotoBlaze's native macOS host (NS1, ADR-021) — slice 1: a minimal SwiftUI app that
/// owns the Rust `AppCore` over the swift-bridge FFI, forwards key events in, and drains
/// the effects out. The wgpu canvas (item 2), real photo source (item 3), and the rest of
/// the event/effect surface land in the following slices; the egui-on-Mac beta remains the
/// shippable Mac artifact until the NS3 cutover.
/// Makes a **bare-binary launch** (`swift run`, running the executable straight out of the
/// .app — the dev loop) behave like a Finder launch: without a proper activation policy in
/// place *before* SwiftUI reconciles its scenes, the initial `WindowGroup` window is never
/// created at all (the app runs windowless with a working menu bar — a fun one to debug).
/// `.onAppear` is too late to fix that, since with no window it never runs.
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationWillFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
    }
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.activate()
    }
}

@main
struct PhotoBlazeMacApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @State private var model = CoreModel()

    var body: some Scene {
        WindowGroup("PhotoBlaze (SwiftUI host)") {
            EffectLogView(model: model)
                .onAppear {
                    model.openLaunchPathIfAny()
                }
        }
    }
}

/// The slice-1 debug surface: shows the effects the Rust core returns for each key event —
/// the on-screen proof that Swift → CoreEvent → AppCore::handle → CoreEffect → Swift works.
struct EffectLogView: View {
    let model: CoreModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            // The wgpu canvas (NS1 item 2): Rust draws the letterbox + the "Press O to
            // open" hint into this CAMetalLayer — the empty-deck frame, GPU-rendered.
            MetalCanvas(model: model)
                .frame(minHeight: 220)

            Text("PhotoBlaze — NS1 slice-1 host")
                .font(.headline)
            Text(
                """
                Key events are forwarded to the Rust AppCore; the CoreEffects it returns \
                appear below. Space/Backspace/arrows drive nav on an empty deck; \
                Esc quits through ShellFlowAction("quit") — the full FFI round trip.
                """
            )
            .font(.caption)
            .foregroundStyle(.secondary)

            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 2) {
                        ForEach(model.effectLog) { line in
                            Text(line.text)
                                .font(.system(.caption, design: .monospaced))
                                .id(line.id)
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                .onChange(of: model.effectLog.last?.id) { _, last in
                    if let last { proxy.scrollTo(last, anchor: .bottom) }
                }
            }
            .background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 6))
        }
        .padding()
        .frame(minWidth: 520, minHeight: 360)
    }
}
