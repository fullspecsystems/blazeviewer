import SwiftUI

/// PhotoBlaze's native macOS host (NS1, ADR-021) — slice 1: a minimal SwiftUI app that
/// owns the Rust `AppCore` over the swift-bridge FFI, forwards key events in, and drains
/// the effects out. The wgpu canvas (item 2), real photo source (item 3), and the rest of
/// the event/effect surface land in the following slices; the egui-on-Mac beta remains the
/// shippable Mac artifact until the NS3 cutover.
@main
struct PhotoBlazeMacApp: App {
    @State private var model = CoreModel()

    var body: some Scene {
        WindowGroup("PhotoBlaze (SwiftUI host)") {
            EffectLogView(model: model)
                .onAppear {
                    // `swift run` launches a bare executable; make sure it fronts like an app.
                    NSApp.setActivationPolicy(.regular)
                    NSApp.activate()
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
