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

    /// Fixed width of the middle text column — the whole reason the spinner + Cancel stay put.
    private let textWidth: CGFloat = 300

    var body: some View {
        HStack(spacing: 12) {
            ProgressView()
                .controlSize(.small)
                // The default spinner is nearly invisible on a dark material — tint it to the
                // same legible adaptive gray as the secondary text (light in dark mode, dark in
                // light mode) so it reads in both.
                .tint(Color.panelSecondary)
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
                        .foregroundStyle(Color.panelSecondary)
                        .layoutPriority(1)
                }
                .font(.callout)

                // The sub-folder currently walked. A non-breaking space holds the row's height
                // when it's blank (still at the root), so the pill doesn't grow/shrink a line.
                Text(model.scanPillCurrent.isEmpty ? "\u{00A0}" : model.scanPillCurrent)
                    .font(.caption)
                    .foregroundStyle(Color.panelSecondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            .frame(width: textWidth, alignment: .leading)

            // Native accent text button (blue), like a notification action — no divider.
            Button("Cancel") { model.scanPillCancel() }
                .buttonStyle(.borderless)
                .help("Stop scanning — keeps what's already loaded")
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
        // A thick material reads more like a macOS notification/HUD and keeps the secondary
        // text legible over a bright photo (the translucent-faint problem on `.regularMaterial`).
        .background(.thickMaterial, in: RoundedRectangle(cornerRadius: 14))
        .overlay(
            RoundedRectangle(cornerRadius: 14)
                .strokeBorder(.separator, lineWidth: 0.5)
        )
        .shadow(radius: 12, y: 3)
        .arrowCursorOnHover()
    }
}
