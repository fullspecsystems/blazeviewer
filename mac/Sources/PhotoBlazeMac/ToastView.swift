// The unified native toast — one bottom-center SwiftUI pill for every `show_toast` in the app
// (copy, rotate, save rotation, pin/unpin, mute, delete, "Scan stopped", "No photos in …", …).
// The core suppresses its CPU-rasterized HUD toast when `native_toast` is set and hands the
// shell the message + a semantic icon instead, so all transient feedback is native and
// consistent. Icon-only (rotate/mute/save), text-only ("Copied"), or icon + text. Reuses the
// shared `panelBackground` so it matches the tree / inspector / scan pill.

import SwiftUI

struct ToastView: View {
    let model: CoreModel

    private var symbol: String? { model.toastSymbol(model.toastIcon) }
    private var message: String { model.toastMessage }

    var body: some View {
        content
            .foregroundStyle(.primary)
            .padding(.horizontal, 16)
            .padding(.vertical, 11)
            .panelBackground(cornerRadius: 12, opacity: model.panelOpacity)
    }

    @ViewBuilder
    private var content: some View {
        if let symbol, !message.isEmpty {
            // Icon + label → a vertical confirmation card: prominent glyph above the label.
            VStack(spacing: 5) {
                Image(systemName: symbol).font(.system(size: 26, weight: .medium))
                Text(message).font(.callout.weight(.medium))
            }
        } else if let symbol {
            // Icon only (e.g. rotate) — the glyph is the whole message, so make it big.
            Image(systemName: symbol).font(.system(size: 32, weight: .medium))
        } else {
            // Text only (e.g. "Copied image").
            Text(message).font(.callout.weight(.medium))
        }
    }
}
