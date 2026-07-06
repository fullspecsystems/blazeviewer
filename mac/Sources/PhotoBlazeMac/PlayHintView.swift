// The play hint (▶ / Live Photo) — the last on-image HUD overlay converted to native SwiftUI.
// It flashes bottom-center, just above the info line, when you settle on a motion item, and
// invites you to press P (or click it) to play. Built to match the info line: same pill, and
// the "P" key hint sits in the same concentric rounded badge as the info line's codec.
// Fades after ~3s (CoreModel owns the timer) unless the pointer holds it open.

import SwiftUI

struct PlayHintView: View {
    let model: CoreModel
    @State private var hovering = false
    /// Set on click so the pill keeps its lit look and fades away *with* it, instead of
    /// snapping back to the resting look before the fade (which read as janky).
    @State private var dismissing = false

    // Match InfoLineView's geometry so the "P" badge is concentric with the pill.
    private let pillRadius: CGFloat = 11
    private let inset: CGFloat = 6

    var body: some View {
        let active = hovering || dismissing
        return HStack(spacing: 8) {
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
        .panelBackground(cornerRadius: pillRadius, opacity: model.panelOpacity)
        // Make the *whole* pill (padding + material, not just the text) the hover/click region —
        // without this the padded areas don't hit-test, so hover often never fires.
        .contentShape(RoundedRectangle(cornerRadius: pillRadius))
        // Hover cue that's actually visible: a subtle brightness lift + a 1% grow (nudging the
        // near-opaque material's opacity was imperceptible). macOS has no built-in hover style
        // for a bespoke pill like this. `active` keeps the look through the click's fade-out.
        .brightness(active ? 0.08 : 0)
        .scaleEffect(active ? 1.01 : 1.0)
        .animation(.easeInOut(duration: 0.13), value: active)
        .onHover { inside in
            hovering = inside
            model.playHintHover(inside)
        }
        .onTapGesture {
            dismissing = true  // hold the lit look while it fades away
            model.triggerPlay()
        }
    }
}
