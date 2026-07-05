// The ambient scan pill (task #54, ④) — a non-blocking, top-center progress element shown
// while a folder walk is in flight, once it has outlasted the brief reveal delay so a fast
// folder never flashes it. It replaces the blocking "Scanning…" modal AND the old in-canvas
// HUD chip: you keep browsing the photos already streaming in while the rest scans, and
// Cancel keeps everything that's already loaded.
//
// Layout is deliberately **fixed-geometry** so nothing jitters as the count ticks and the
// folder changes: the spinner and Cancel sit at fixed ends, the middle text block has a
// fixed width, the folder name is left-aligned (truncating tail), and the live count is
// right-anchored + monospaced so its right edge never moves.

import SwiftUI

struct ScanPillView: View {
    let model: CoreModel
    @Environment(\.colorScheme) private var scheme

    /// Fixed width of the middle text column — the whole reason the spinner + Cancel stay put.
    private let textWidth: CGFloat = 300

    /// The folder-tree icon's secondary gray, but resolved from SwiftUI's `colorScheme`
    /// instead of a dynamic `NSColor`: inside the pill's material the dynamic one resolved to
    /// its *light-mode* value even in dark mode, which made this text near-invisible and the
    /// spinner black-on-black. (Slightly brighter in dark so it clearly reads over a photo.)
    private var secondary: Color {
        scheme == .dark ? Color(white: 0.7) : Color(white: 0.4)
    }

    var body: some View {
        HStack(spacing: 12) {
            // A pure-SwiftUI spinner: the native `ProgressView` spinner is an AppKit
            // `NSProgressIndicator` whose spoke color follows its own appearance — which, like
            // `panelSecondary`, resolves to light-mode inside the pill's material, so it stayed
            // dark-on-dark and `.tint` couldn't override it. This one's color we fully control.
            ScanSpinner(color: scheme == .dark ? Color(white: 0.9) : Color(white: 0.35))
                .frame(width: 16, height: 16)

            VStack(alignment: .leading, spacing: 2) {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text("Scanning \(model.scanPillName)")
                        .fontWeight(.semibold)
                        .lineLimit(1)
                        .truncationMode(.tail)
                    Spacer(minLength: 8)
                    // Right-anchored, monospaced — a growing count never shoves the layout.
                    Text("\(model.scanPillFound) found")
                        .monospacedDigit()
                        .foregroundStyle(secondary)
                        .layoutPriority(1)
                }
                .font(.callout)

                // The sub-folder currently walked. A non-breaking space holds the row's height
                // when it's blank (still at the root), so the pill doesn't grow/shrink a line.
                Text(model.scanPillCurrent.isEmpty ? "\u{00A0}" : model.scanPillCurrent)
                    .font(.caption)
                    .foregroundStyle(secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            .frame(width: textWidth, alignment: .leading)

            // A subtle grooved rule (the inspector/tree heading divider, run vertically) —
            // not the pure-black default `Divider()`.
            Rectangle()
                .fill(Color.primary.opacity(0.12))
                .frame(width: 1, height: 26)

            // A real, clickable accent action — reads as a macOS blue link, not disabled gray.
            Button("Cancel") { model.scanPillCancel() }
                .buttonStyle(.plain)
                .font(.callout.weight(.medium))
                .foregroundStyle(Color.accentColor)
                .help("Stop scanning — keeps what's already loaded")
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
        // A thick material reads more like a macOS notification/HUD and keeps the secondary
        // text legible over a bright photo.
        .background(.thickMaterial, in: RoundedRectangle(cornerRadius: 14))
        .overlay(
            RoundedRectangle(cornerRadius: 14)
                .strokeBorder(.separator, lineWidth: 0.5)
        )
        .shadow(radius: 12, y: 3)
        .arrowCursorOnHover()
    }
}

/// A small indeterminate spinner in the macOS fading-spokes style, drawn in pure SwiftUI so
/// its color is exactly what we pass — the native `ProgressView` spinner rendered dark-on-dark
/// inside the pill's material (its AppKit appearance resolved to light even in dark mode).
private struct ScanSpinner: View {
    let color: Color
    private let spokes = 8
    @State private var spinning = false

    var body: some View {
        ZStack {
            ForEach(0..<spokes, id: \.self) { i in
                Capsule()
                    .fill(color)
                    // A graduated trail: brightest leading spoke, fading behind it.
                    .opacity(Double(i + 1) / Double(spokes))
                    .frame(width: 1.6, height: 4.5)
                    .offset(y: -4.5)
                    .rotationEffect(.degrees(360.0 / Double(spokes) * Double(i)))
            }
        }
        .frame(width: 15, height: 15)
        .rotationEffect(.degrees(spinning ? 360 : 0))
        .onAppear {
            withAnimation(.linear(duration: 0.85).repeatForever(autoreverses: false)) {
                spinning = true
            }
        }
    }
}
