// The play hint (▶ / Live Photo) — the last on-image HUD overlay converted to native SwiftUI.
// It flashes bottom-center, just above the info line, when you settle on a motion item, and
// invites you to press P (or click it) to play. Built to match the info line: same pill, and
// the "P" key hint sits in the same concentric rounded badge as the info line's codec.
// Fades after ~3s (CoreModel owns the timer) unless the pointer holds it open.
//
// An archive door has no pill: its affordance is the door card (task #105), still to be
// built on this shell.

import SwiftUI

struct PlayHintView: View {
    let model: CoreModel
    /// Set on click so the pill keeps its lit look and fades away *with* it, instead of
    /// snapping back to the resting look before the fade (which read as janky).
    @State private var dismissing = false

    // Match InfoLineView's geometry so the "P" badge is concentric with the pill.
    private let pillRadius = PanelMetrics.cornerRadius
    private let inset: CGFloat = 6

    /// A Live Photo gets Apple's livephoto mark; any other animation gets play ▶.
    /// Kinds come from `AppCore::play_hint_kind`.
    private var symbol: String {
        model.playHintKind == 1 ? "livephoto" : "play.fill"
    }

    private var label: String { "Play" }

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: symbol)
                .font(.callout)
                .foregroundStyle(.primary)
            Text(label)
                .font(.callout)
                .foregroundStyle(.primary)
            Keycap(text: "P")
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
        // `dismissing` keeps the lit look through the click's fade-out.
        .onImageHoverGlow(extraActive: dismissing) { model.playHintHover($0) }
        .onTapGesture {
            dismissing = true  // hold the lit look while it fades away
            model.triggerPlay()
        }
    }
}
