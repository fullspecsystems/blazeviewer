// Shared chrome helpers for the native panels (task #54).

import AppKit
import SwiftUI

extension Color {
    /// A solid, dimmer stand-in for `.secondary` on the panels — light gray in dark mode,
    /// dark gray in light mode. Opaque (unlike the translucent `.secondary`) so icons /
    /// labels / ✕ stay legible over the panel material with bright content behind, while
    /// still reading as secondary to the primary label text.
    static let panelSecondary = Color(
        nsColor: NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
                ? NSColor(white: 0.64, alpha: 1.0)
                : NSColor(white: 0.42, alpha: 1.0)
        })
}

extension View {
    /// Force the arrow cursor while the pointer is over this view. The photo canvas drives
    /// the cursor (a grab hand when zoomed) via `desiredCursor` and doesn't know a panel is
    /// layered on top, so without this a panel inherits the stale grab. A pointer-gating
    /// stopgap until the canvas suppresses pointer handling inside a presenter's frame.
    func arrowCursorOnHover() -> some View {
        onHover { inside in
            if inside { NSCursor.arrow.set() }
        }
    }
}
