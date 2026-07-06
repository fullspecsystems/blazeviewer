// The play hint (▶ / Live Photo) — the last on-image HUD overlay converted to native SwiftUI.
// It flashes bottom-center, just above the info line, when you settle on a motion item, and
// invites you to press P (or click it) to play. Built to match the info line: same pill, and
// the "P" key hint sits in the same concentric rounded badge as the info line's codec.
// Fades after ~3s (CoreModel owns the timer) unless the pointer holds it open.

import SwiftUI

struct PlayHintView: View {
    let model: CoreModel

    // Match InfoLineView's geometry so the "P" badge is concentric with the pill.
    private let pillRadius: CGFloat = 11
    private let inset: CGFloat = 6

    var body: some View {
        HStack(spacing: 8) {
            // A Live Photo gets Apple's livephoto mark; any other animation gets play ▶.
            Image(systemName: model.playHintKind == 1 ? "livephoto" : "play.fill")
                .font(.callout)
                .foregroundStyle(.primary)
            Text("Play")
                .font(.callout)
                .foregroundStyle(.primary)
            Text("P")
                .font(.caption2)
                .foregroundStyle(.secondary)
                .padding(.horizontal, 6)
                .padding(.vertical, 2)
                .background(.quaternary, in: RoundedRectangle(cornerRadius: pillRadius - inset))
        }
        .padding(.leading, 11)
        .padding(.trailing, inset)
        .padding(.vertical, inset)
        .panelBackground(cornerRadius: pillRadius, opacity: model.panelOpacity)
        .onHover { model.playHintHover($0) }
        .onTapGesture { model.triggerPlay() }
    }
}
