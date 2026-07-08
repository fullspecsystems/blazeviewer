// The "Press [F] to exit fullscreen" hint (task #55). Shown briefly (6s) when the borderless
// speed mode is entered **by mouse** (toolbar/menu) — where all chrome auto-hides, so a mouse
// user has no visible way back out. A keyboard user who pressed the key themselves never sees
// it (see `CoreModel.fullscreenHintFromMouse`). It's a native view, not a toast, so the key
// renders as a keycap (`ShortcutView`) — the same pill styling as the welcome screen and the
// help panel — instead of plain text. The key shown is the live primary binding (`F` by
// default; ⌥⏎ also works, as does F11 — a low-priority alternate on macOS, so it never
// shows as the primary).

import SwiftUI

struct FullscreenHintView: View {
    let model: CoreModel

    /// The primary fullscreen binding as a keycap string ("F"); a sane fallback if it's ever
    /// unbound, so the pill never reads "Press  to exit fullscreen".
    private var key: String {
        let k = model.shortcut("fullscreen")
        return k.isEmpty ? "F" : k
    }

    var body: some View {
        HStack(spacing: 8) {
            Text("Press").font(.callout.weight(.medium))
            ShortcutView(shortcut: key)
            Text("to exit Quick Full Screen").font(.callout.weight(.medium))
        }
        .foregroundStyle(.primary)
        .padding(.horizontal, 16)
        .padding(.vertical, 11)
        .panelBackground(opacity: model.panelOpacity)
    }
}
