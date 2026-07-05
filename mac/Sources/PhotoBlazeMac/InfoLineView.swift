// The one-line info readout (`i`) — the last HUD element converted to native SwiftUI. A small
// bottom-corner pill showing `folder/name · W×H · CODEC[· Live]`, reusing the shared
// panelBackground so it matches the rest of the chrome. Placement follows the Settings
// "info line alignment" (left / center / right, default right). Non-interactive.

import SwiftUI

struct InfoLineView: View {
    let model: CoreModel

    var body: some View {
        Text(model.infoLineText)
            .font(.caption)
            .foregroundStyle(.primary)
            .lineLimit(1)
            .truncationMode(.middle)
            .padding(.horizontal, 10)
            .padding(.vertical, 5)
            .panelBackground(cornerRadius: 7, opacity: model.panelOpacity)
    }
}
