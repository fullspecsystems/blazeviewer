// The unified native toast — one bottom-center SwiftUI pill for every `show_toast` in the app
// (copy, rotate, save rotation, pin/unpin, mute, delete, "Scan stopped", "No photos in …", …).
// The core suppresses its CPU-rasterized HUD toast when `native_toast` is set and hands the
// shell the message + a semantic icon instead, so all transient feedback is native and
// consistent. Icon-only (rotate/mute/save), text-only ("Copied"), or icon + text. Reuses the
// shared `panelBackground` so it matches the tree / inspector / scan pill.

import SwiftUI

struct ToastView: View {
    let model: CoreModel

    var body: some View {
        HStack(spacing: 8) {
            if let symbol = model.toastSymbol(model.toastIcon) {
                Image(systemName: symbol)
                    .font(.body)
                    .foregroundStyle(.primary)
            }
            if !model.toastMessage.isEmpty {
                Text(model.toastMessage)
                    .font(.callout.weight(.medium))
                    .foregroundStyle(.primary)
            }
        }
        .padding(.horizontal, 15)
        .padding(.vertical, 9)
        .panelBackground(cornerRadius: 11, opacity: model.panelOpacity)
    }
}
