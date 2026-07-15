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

    /// SF Symbols have wildly different intrinsic boxes (a wide `speaker.wave.2.fill` vs a
    /// narrow `rotate.left` vs a tall `doc.on.doc`), so a bare glyph makes every icon toast a
    /// different size and shape. Rendering each in a **fixed square** at one point size gives
    /// every icon-only toast the *same* square pill. `iconBox` fits the largest toast glyph at
    /// `iconPointSize` (measured max ≈ 40pt); equal padding keeps the pill square.
    private let iconPointSize: CGFloat = 28
    private let iconBox: CGFloat = 42
    private let iconOnlyPadding: CGFloat = 12

    private func toastIcon(_ symbol: String) -> some View {
        Image(systemName: symbol)
            .font(.system(size: iconPointSize, weight: .medium))
            .frame(width: iconBox, height: iconBox)
    }

    var body: some View {
        content
            .foregroundStyle(.primary)
            .panelBackground(opacity: model.panelOpacity)
    }

    @ViewBuilder
    private var content: some View {
        if let symbol, !message.isEmpty {
            // Icon + label → a vertical confirmation card: the glyph (same fixed square) above
            // the label; the pill width is driven by the label.
            VStack(spacing: 5) {
                toastIcon(symbol)
                Text(message).font(.callout.weight(.medium))
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 11)
        } else if let symbol {
            // Icon only (mute / unmute / rotate / save …) → a consistent square, whatever glyph.
            toastIcon(symbol)
                .padding(iconOnlyPadding)
        } else {
            // Text only (e.g. "Copied image").
            Text(message).font(.callout.weight(.medium))
                .padding(.horizontal, 16)
                .padding(.vertical, 11)
        }
    }
}
