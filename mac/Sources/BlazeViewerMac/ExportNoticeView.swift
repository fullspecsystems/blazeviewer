// The export notice (task #89): "Exported clip-0h01m23s.png" as a pill you click to show the
// file in Finder — the toast that became actionable. Bottom-center like the toast and the
// Play hint, styled like the Play hint (same pill geometry, the same hover glow), with a
// magnifying glass — Safari Downloads' "Show in Finder" mark — as the affordance.
//
// The core owns its lifetime: 5 s, held while the pointer is on it (leaving restarts the
// hold), and gone the moment you navigate or run any other command, so it never sits over a
// photo you've moved on to. File ▸ Show Last Export in Finder is the keyboard / VoiceOver
// route, and still works after the pill is gone.

import SwiftUI

struct ExportNoticeView: View {
    let model: CoreModel
    /// Set on click so the pill keeps its lit look while it fades (the Play hint's trick).
    @State private var dismissing = false

    private let pillRadius = PanelMetrics.cornerRadius
    private let inset: CGFloat = 6

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "checkmark.circle.fill")
                .font(.callout)
                .foregroundStyle(.secondary)
            Text(model.exportNoticeMessage)
                .font(.callout)
                .foregroundStyle(.primary)
                .lineLimit(1)
                .truncationMode(.middle)
            Image(systemName: "magnifyingglass")
                .font(.callout.weight(.semibold))
                .foregroundStyle(.primary)
        }
        .padding(.horizontal, inset + 4)
        .padding(.vertical, inset)
        .panelBackground(cornerRadius: pillRadius, opacity: model.panelOpacity)
        // The whole pill is the hover/click target, padding included.
        .contentShape(RoundedRectangle(cornerRadius: pillRadius))
        .onImageHoverGlow(extraActive: dismissing) { model.exportNoticeHover($0) }
        .onTapGesture {
            dismissing = true
            model.exportNoticeClicked()
        }
        .help("Show in Finder")
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(model.exportNoticeMessage). Show in Finder")
        .accessibilityAddTraits(.isButton)
        .accessibilityAction { model.exportNoticeClicked() }
    }
}
