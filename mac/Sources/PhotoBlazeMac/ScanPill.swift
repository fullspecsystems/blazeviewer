// The ambient scan pill (task #54, ④) — a non-blocking, top-center progress element shown
// while a folder (or archive) walk is in flight, once it has outlasted the brief reveal
// delay so a fast folder never flashes it. It replaces the blocking "Scanning…" modal AND
// the old in-canvas HUD chip: you keep browsing the photos already streaming in while the
// rest of a big folder scans, and Cancel keeps everything that's already loaded.

import SwiftUI

struct ScanPillView: View {
    let model: CoreModel

    var body: some View {
        HStack(spacing: 11) {
            ProgressView()
                .controlSize(.small)
                .scaleEffect(0.85)

            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 6) {
                    Text("Scanning \(model.scanPillName)")
                        .fontWeight(.medium)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Text("· \(model.scanPillFound) found")
                        .foregroundStyle(Color.panelSecondary)
                        .monospacedDigit()
                }
                .font(.callout)
                // The sub-folder currently being walked (blank while it's still the root).
                if !model.scanPillCurrent.isEmpty {
                    Text(model.scanPillCurrent)
                        .font(.caption)
                        .foregroundStyle(Color.panelSecondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
            }

            Divider().frame(height: 22)

            Button(action: { model.scanPillCancel() }) {
                Text("Cancel").fontWeight(.medium)
            }
            .buttonStyle(.plain)
            .foregroundStyle(Color.accentColor)
            .help("Stop scanning (keeps what's loaded)")
        }
        .padding(.horizontal, 15)
        .padding(.vertical, 9)
        .frame(maxWidth: 460)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))
        .overlay(
            RoundedRectangle(cornerRadius: 14)
                .strokeBorder(.separator, lineWidth: 0.5)
        )
        .shadow(radius: 16, y: 4)
        .arrowCursorOnHover()
    }
}
