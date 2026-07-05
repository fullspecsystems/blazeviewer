// Shared chrome helpers for the native panels (task #54).

import AppKit
import SwiftUI

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
