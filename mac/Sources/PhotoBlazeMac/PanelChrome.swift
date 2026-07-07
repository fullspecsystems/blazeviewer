// Shared chrome helpers for the native panels (task #54).

import AppKit
import SwiftUI

/// The single spacing knob the whole chrome is laid out on, so margins read as one intentional
/// system rather than per-overlay guesses: panels inset from the window edges by `edge`, the
/// info line sits `edge` off the bottom, and stacked overlays (panel → info line, toast → info
/// line) keep the same `edge` gap between them.
enum Layout {
    static let edge: CGFloat = 24

    /// The single fade every chrome overlay shows/hides on — the corner panels (folder
    /// tree, Inspector), the info line, and Help. Enabling/disabling/hiding any of them
    /// (or hitting Tab to hide them all at once) reads as one smooth system instead of a
    /// mix of instant pops and coincidental fades. ~0.2s easeInOut matches the info line's
    /// established feel; driven from `CoreModel` via `withAnimation` so it fires on every
    /// path — a direct close, a Tab hide/reveal, or a settings toggle.
    static let chromeFade: Animation = .easeInOut(duration: 0.2)
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

extension View {
    /// The hover cue for a control that floats **directly on the photo** (not a panel or
    /// dialog control, which stay native mac — `.bordered`/system hover): a subtle brightness
    /// lift + 1% grow, animated. macOS has no built-in hover style for a bespoke on-image
    /// control (nudging the near-opaque panel material's opacity was imperceptible), so this
    /// is the shared language for "this is alive, it responds to you" — originally the play
    /// hint, now also the welcome screen's Open buttons.
    ///
    /// `extraActive` ORs in a caller-owned condition that should also read as lit (e.g. the
    /// play hint holding its look through a click-triggered fade) without duplicating the
    /// hover-tracking state. `onHoverChange` lets the caller observe hover too (instead of a
    /// second stacked `.onHover`) for side effects like the play hint's hold-open timer.
    func onImageHoverGlow(
        extraActive: Bool = false,
        onHoverChange: @escaping (Bool) -> Void = { _ in }
    ) -> some View {
        modifier(OnImageHoverGlow(extraActive: extraActive, onHoverChange: onHoverChange))
    }
}

private struct OnImageHoverGlow: ViewModifier {
    let extraActive: Bool
    let onHoverChange: (Bool) -> Void
    @State private var hovering = false
    // A disabled control (e.g. an Open button while its panel is already up) shouldn't
    // read as "alive" — read from the environment so callers don't have to gate manually.
    @Environment(\.isEnabled) private var isEnabled

    func body(content: Content) -> some View {
        let active = isEnabled && (hovering || extraActive)
        content
            .brightness(active ? 0.08 : 0)
            .scaleEffect(active ? 1.01 : 1.0)
            .animation(.easeInOut(duration: 0.13), value: active)
            .onHover { inside in
                hovering = inside
                onHoverChange(inside)
            }
    }
}
