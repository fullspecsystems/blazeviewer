// Shared chrome helpers for the native panels (task #54).

import AppKit
import SwiftUI

/// The single spacing knob the whole chrome is laid out on, so margins read as one intentional
/// system rather than per-overlay guesses: panels inset from the window edges by `edge`, the
/// info line sits `edge` off the bottom, and stacked overlays (panel → info line, toast → info
/// line) keep the same `edge` gap between them.
enum Layout {
    static let edge: CGFloat = 24
}

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
    /// The one shared translucent backdrop for every native panel (folder tree, inspector,
    /// scan pill, toast) — a single place to tune the material (blur) and the user opacity, so
    /// the panels stay consistent instead of each hard-coding its own material / border /
    /// shadow (which is how the pill drifted to `.thick` while the others were `.regular`).
    /// `.regularMaterial` is a notch less blur than the pill's old `.thick`; `opacity` (< 1)
    /// lets more of the photo show through and is fed from Settings once the slider lands.
    func panelBackground(cornerRadius: CGFloat = 12, opacity: Double = 1.0) -> some View {
        self
            .background(
                RoundedRectangle(cornerRadius: cornerRadius)
                    .fill(.regularMaterial)
                    .opacity(opacity)
            )
            .overlay(
                RoundedRectangle(cornerRadius: cornerRadius)
                    .strokeBorder(.separator, lineWidth: 0.5)
            )
            .shadow(radius: 18, y: 5)
    }
}

/// A thin drag strip on a panel's inner edge that resizes its width. `sign` is +1 for a
/// trailing edge (a leading-anchored panel — the folder tree — widens dragging right) or
/// -1 for a leading edge (a trailing-anchored panel — the Inspector — widens dragging
/// left). Shows the horizontal-resize cursor; clamps to `[minWidth, maxWidth]`.
struct ResizeHandle: View {
    @Binding var width: CGFloat
    let minWidth: CGFloat
    let maxWidth: CGFloat
    let sign: CGFloat
    @State private var startWidth: CGFloat?

    var body: some View {
        Rectangle()
            .fill(Color.clear)
            .frame(width: 9)
            .contentShape(Rectangle())
            .onHover { inside in
                if inside { NSCursor.resizeLeftRight.set() } else { NSCursor.arrow.set() }
            }
            .gesture(
                DragGesture(minimumDistance: 1)
                    .onChanged { value in
                        let base = startWidth ?? width
                        if startWidth == nil { startWidth = width }
                        width = min(max(base + sign * value.translation.width, minWidth), maxWidth)
                    }
                    .onEnded { _ in startWidth = nil }
            )
    }
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
