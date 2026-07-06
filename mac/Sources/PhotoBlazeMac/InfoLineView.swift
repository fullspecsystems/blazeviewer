// The one-line info readout (`i`) — the last HUD element converted to native SwiftUI. A small
// bottom-corner pill showing `folder/name · W×H · CODEC[· Live]`, reusing the shared
// panelBackground so it matches the rest of the chrome. Placement follows the Settings
// "info line alignment" (left / center / right, default right). Non-interactive.

import SwiftUI

struct InfoLineView: View {
    let model: CoreModel

    // The pill's geometry. The codec badge is inset from the pill by ~`inset` on the top,
    // right, and bottom (its own vertical padding makes it about the row height, and the pill's
    // trailing padding drops to match the vertical one), so a badge radius of `pillRadius −
    // inset` makes its corners run roughly concentric with the pill's — parallel curves.
    private let pillRadius: CGFloat = 11
    private let inset: CGFloat = 6

    var body: some View {
        let hasCodec = !model.infoLineCodec.isEmpty
        HStack(spacing: 8) {
            Text(model.infoLineText)
                .font(.callout)
                .foregroundStyle(.primary)
                .lineLimit(1)
                .truncationMode(.middle)
            // A Live Photo shows the livephoto mark by the codec instead of the word "Live".
            if model.infoLineIsLive {
                Image(systemName: "livephoto")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            if hasCodec {
                Text(model.infoLineCodec)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 6)
                    // Deliberately shorter than the callout row so the badge sits inside it and
                    // never drives the pill's height (do NOT use frame(maxHeight: .infinity) —
                    // it makes the pill fill the viewport).
                    .padding(.vertical, 2)
                    .background(
                        .quaternary,
                        in: RoundedRectangle(cornerRadius: pillRadius - inset)
                    )
            }
        }
        .padding(.leading, 11)
        // Trailing padding collapses to the vertical inset when the badge is present, so it's
        // spaced equally on the exposed sides and the concentric corners line up.
        .padding(.trailing, hasCodec ? inset : 11)
        .padding(.vertical, inset)
        .panelBackground(cornerRadius: pillRadius, opacity: model.panelOpacity)
    }
}
