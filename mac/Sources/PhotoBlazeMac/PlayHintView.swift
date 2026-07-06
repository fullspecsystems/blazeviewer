// The play hint (▶ / Live Photo) — the last on-image HUD overlay converted to native SwiftUI.
// It flashes bottom-center, just above the info line, when you settle on a motion item, and
// invites you to press P (or click it) to play. Built to match the info line: same pill, and
// the "P" key hint sits in the same concentric rounded badge as the info line's codec.
// Fades after ~3s (CoreModel owns the timer) unless the pointer holds it open.

import SwiftUI

struct PlayHintView: View {
    let model: CoreModel
    @State private var hovering = false

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
        // The leading icon is inset the same as the top/bottom (it leads with a symbol, not
        // text, so it doesn't need the info line's roomier text gutter).
        .padding(.leading, inset)
        .padding(.trailing, inset)
        .padding(.vertical, inset)
        // Nudge the material ~10% more opaque on hover — a subtle "this is clickable" cue
        // (macOS has no built-in hover style for a bespoke pill like this).
        .panelBackground(
            cornerRadius: pillRadius,
            opacity: hovering ? min(1.0, model.panelOpacity + 0.1) : model.panelOpacity
        )
        .animation(.easeInOut(duration: 0.15), value: hovering)
        .onHover { inside in
            hovering = inside
            model.playHintHover(inside)
        }
        .onTapGesture { model.triggerPlay() }
    }
}
