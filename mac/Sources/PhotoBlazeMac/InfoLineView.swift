// The one-line info readout (`i`) — the last HUD element converted to native SwiftUI. A small
// bottom-corner pill showing `folder/name · W×H · CODEC[· Live]`, reusing the shared
// panelBackground so it matches the rest of the chrome. Placement follows the Settings
// "info line alignment" (left / center / right, default right). Non-interactive.

import SwiftUI

struct InfoLineView: View {
    let model: CoreModel

    var body: some View {
        HStack(spacing: 8) {
            Text(model.infoLineText)
                .font(.callout)
                .foregroundStyle(.primary)
                .lineLimit(1)
                .truncationMode(.middle)
            // The codec sits in a small rounded-rect badge (not a full pill) so its corners
            // echo — and stay roughly concentric with — the info pill that contains it.
            if !model.infoLineCodec.isEmpty {
                Text(model.infoLineCodec)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 7)
                    .padding(.vertical, 2)
                    .background(.quaternary, in: RoundedRectangle(cornerRadius: 5))
            }
        }
        .padding(.horizontal, 11)
        .padding(.vertical, 6)
        .panelBackground(cornerRadius: 9, opacity: model.panelOpacity)
    }
}
