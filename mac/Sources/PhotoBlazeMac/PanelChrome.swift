// Shared chrome helpers for the native panels (task #54).

import AppKit
import SwiftUI

/// The single spacing knob the whole chrome is laid out on, so margins read as one intentional
/// system rather than per-overlay guesses: panels inset from the window edges by `edge`, the
/// info line sits `edge` off the bottom, and stacked overlays (panel → info line, toast → info
/// line) keep the same `edge` gap between them.
/// The shared geometry tokens for the floating panels (folder tree, Inspector, Help), so
/// their corners and header layout stay identical instead of each panel hard-coding its
/// own numbers — which is exactly how they drifted (Help's header ran taller, its ✕ sat
/// further from the edge). Change a value here and every panel moves together.
enum PanelMetrics {
    /// The one corner radius for the **entire** floating chrome — the panel cards (folder
    /// tree, Inspector, Help), the toast and fullscreen hint, and the pills (info line,
    /// play hint, scan pill). The header geometry below is *derived* from it, so turning it
    /// up/down keeps the close buttons perfectly concentric with the corners (and the inner
    /// badges, which derive as `cornerRadius − inset`, stay concentric too).
    static let cornerRadius: CGFloat = 18

    /// Half the close button's frame — `xmark.circle.fill` @ `.large` renders a 20pt box
    /// (measured) — so the derivations can place its *center*. Bump if the ✕ scale changes.
    static let closeButtonRadius: CGFloat = 10

    /// Every panel header is `2 · cornerRadius` tall, so a vertically-centered close button's
    /// center lands `cornerRadius` from the top edge — concentric with the top corners. The
    /// Inspector's taller segmented control simply centers within this same height.
    static let headerHeight = 2 * cornerRadius

    /// The close button's inset from the trailing edge, `cornerRadius − closeButtonRadius`,
    /// so its center lands `cornerRadius` from the right edge — concentric with the right
    /// corners (16 → 6pt). Turn `cornerRadius` up/down and the ✕ stays nested.
    static let closeInset = cornerRadius - closeButtonRadius

    /// The title / content leading inset — the roomier side; not part of the concentric math.
    static let headerLeadingInset: CGFloat = 14
}

/// Reads the window's **corner-adaptive safe-area inset** — the macOS 26 `NSView.LayoutRegion`
/// API Apple added so content stays clear of Tahoe's larger, style-varying rounded corners
/// (WWDC25 "Build an AppKit app with the new design"). Used as a full-bleed `.background`, so
/// its insets reflect the *window's* corners; it reports the horizontal corner clearance, which
/// the chrome uses as its shared edge gap. That lets the gap track whatever corner radius the OS
/// (and window style) currently uses instead of a hardcoded number that drifts every release.
/// macOS < 26 never instantiates this — the chrome falls back to `Layout.edge`.
@available(macOS 26.0, *)
struct WindowCornerInsetReader: NSViewRepresentable {
    let onChange: (CGFloat) -> Void

    func makeNSView(context: Context) -> NSView { Probe(onChange: onChange) }
    func updateNSView(_ nsView: NSView, context: Context) { (nsView as? Probe)?.onChange = onChange }

    final class Probe: NSView {
        var onChange: (CGFloat) -> Void
        private var last: CGFloat = -1
        private let trace = ProcessInfo.processInfo.environment["PB_TRACE"] != nil

        init(onChange: @escaping (CGFloat) -> Void) {
            self.onChange = onChange
            super.init(frame: .zero)
        }
        required init?(coder: NSCoder) { fatalError("no coder") }

        override func layout() {
            super.layout()
            report()
        }
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            report()
        }

        private func report() {
            let insets = edgeInsets(for: .safeArea(cornerAdaptation: .horizontal))
            // The horizontal corner clearance ≈ the window corner radius. Use the *smaller*
            // side: the other side is inflated by the window-control (traffic-light) region,
            // which we don't want to reserve for the gap. min() picks the control-free side
            // whether the controls are leading (LTR) or trailing (RTL).
            let inset = min(insets.left, insets.right)
            if trace {
                NSLog(
                    "[corner] L=%.1f R=%.1f T=%.1f B=%.1f -> edge=%.1f",
                    insets.left, insets.right, insets.top, insets.bottom, inset)
            }
            // Ignore a non-positive read (keeps the fallback) and no-op tiny jitters.
            guard inset > 0, abs(inset - last) > 0.5 else { return }
            last = inset
            onChange(inset)
        }
    }
}

enum Layout {
    /// The chrome's shared edge/gap fallback — used verbatim on macOS < 26, and as the seed
    /// before `WindowCornerInsetReader` reports the OS corner-adaptive value on macOS 26+.
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
    func panelBackground(cornerRadius: CGFloat = PanelMetrics.cornerRadius, opacity: Double = 1.0)
        -> some View
    {
        self
            // Clip the *content* to the panel shape first, so full-bleed row backgrounds (e.g.
            // the folder tree's current-folder highlight) don't poke square corners out past
            // the rounded edge. The material, border, and shadow are added after, so they stay
            // rounded and the shadow isn't clipped.
            .clipShape(RoundedRectangle(cornerRadius: cornerRadius))
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

/// A panel header's ✕ close button — one definition so the glyph, tone, and size match
/// across every panel (folder tree, Inspector, Help).
struct PanelCloseButton: View {
    let action: () -> Void
    var help: String = "Close"

    var body: some View {
        Button(action: action) {
            Image(systemName: "xmark.circle.fill")
                .foregroundStyle(Color.panelSecondary)
                .imageScale(.large)
        }
        .buttonStyle(.plain)
        .help(help)
    }
}

/// A keyboard-shortcut keycap — the soft "concentric pill" the play hint introduced, now shared
/// by every shortcut key (the play hint, the welcome opens, the Help cheatsheet, the fullscreen
/// hint). A `.quaternary` capsule so a key reads the same wherever it's shown; `minWidth` keeps
/// single characters an even width.
struct Keycap: View {
    let text: String

    var body: some View {
        Text(text)
            .font(.caption2)
            .foregroundStyle(.secondary)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .frame(minWidth: 16)
            .background(.quaternary, in: Capsule())
    }
}

/// The faint groove under a panel header — subtler than a hard divider on the material.
struct PanelDivider: View {
    var body: some View {
        Rectangle()
            .fill(Color.primary.opacity(0.08))
            .frame(height: 1)
    }
}

/// A panel's title bar: a bold title, a trailing equidistant ✕, the shared insets, and the
/// groove beneath. The folder tree and Help share this verbatim; the Inspector has a
/// bespoke tab-bar header but draws from the same `PanelMetrics` tokens + `PanelCloseButton`.
struct PanelHeader: View {
    let title: String
    var closeHelp: String = "Close"
    let onClose: () -> Void

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Text(title)
                    .font(.headline)
                Spacer()
                PanelCloseButton(action: onClose, help: closeHelp)
            }
            .padding(.leading, PanelMetrics.headerLeadingInset)
            .padding(.trailing, PanelMetrics.closeInset)
            .frame(height: PanelMetrics.headerHeight)
            PanelDivider()
        }
    }
}

/// A drag strip on a panel's inner edge that resizes its width. `sign` is +1 for a
/// trailing edge (a leading-anchored panel — the folder tree — widens dragging right) or
/// -1 for a leading edge (a trailing-anchored panel — the Inspector — widens dragging
/// left). Shows the horizontal-resize cursor; clamps to `[minWidth, maxWidth]`.
struct ResizeHandle: View {
    let model: CoreModel
    @Binding var width: CGFloat
    let minWidth: CGFloat
    let maxWidth: CGFloat
    let sign: CGFloat
    @State private var startWidth: CGFloat?

    /// Total grab-strip width. Big enough that an invisible border target isn't fiddly,
    /// but trimmed back once the resize cursor reliably shows across the whole strip
    /// (which made the old 18pt feel oversized).
    private let hitWidth: CGFloat = 12
    /// How far the strip pokes *past* the panel edge into the transparent inset gap, so
    /// aiming right at the visible border lands inside the zone (not on its outer lip —
    /// which is what made the old edge-aligned strip feel one-pixel wide). The rest stays
    /// inside the panel, so hover/drag still works even where the outside sliver can't be
    /// hit-tested. `sign` points the poke outward: +x past a trailing edge, −x past a leading one.
    private let outset: CGFloat = 5

    var body: some View {
        Rectangle()
            .fill(Color.clear)
            .frame(width: hitWidth)
            .contentShape(Rectangle())
            // Route through the model, not a bare `NSCursor.set()` — the canvas re-asserts
            // `desiredCursor` on every cursorUpdate (even under the panel), which would
            // otherwise stomp a directly-set resize cursor back to the arrow instantly.
            .onHover { inside in model.setPanelResizeCursor(inside) }
            // `.global`, not the default `.local`: the handle is anchored to the very edge it
            // resizes, so it moves *with* the panel as `width` changes. A local translation is
            // then measured against a frame that shifts under it — each frame's resize cancels
            // the last, and the width flip-flops between two values (a visible drag flicker).
            // Global (window) space is fixed, so the translation is the raw pointer delta and
            // the width converges.
            .gesture(
                DragGesture(minimumDistance: 1, coordinateSpace: .global)
                    .onChanged { value in
                        let base = startWidth ?? width
                        if startWidth == nil { startWidth = width }
                        width = min(max(base + sign * value.translation.width, minWidth), maxWidth)
                    }
                    .onEnded { _ in startWidth = nil }
            )
            // Straddle the border: shift outward by `outset` so the strip centers nearer the
            // edge line. `.offset` moves the hit region without disturbing the panel's layout.
            .offset(x: sign * outset)
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
