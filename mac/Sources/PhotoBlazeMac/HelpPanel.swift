// The native Help panel (task #54, mac-first) — the pilot for the ADR-023 rich-panel
// migration. The core owns the semantic model (sections of command + shortcut rows,
// sourced from the live keymap); this view just renders it. It's a SwiftUI view layered
// over the Metal canvas (the panel model crosses FFI as flat rows via `help_row_*`,
// regrouped into sections host-side), with real chrome (title bar + ✕ close). Read-only,
// so it needs no keyboard focus — `?`/`/` still toggle it and Esc still quits.

import SwiftUI

/// Measures the shortcut list's natural height so the panel can fit-to-content up to the
/// window's available height (then scroll).
private struct HelpContentHeight: PreferenceKey {
    static var defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = max(value, nextValue())
    }
}

/// One command in the Help panel: a description and its shortcut string (e.g. "⇧ ↩").
struct HelpItem: Identifiable {
    let id: Int
    let label: String
    let shortcut: String
}

/// A titled group of commands (Browse / View & Zoom / …).
struct HelpSection: Identifiable {
    let id: Int
    let title: String
    let items: [HelpItem]
}

/// The floating Help card: a titled panel with a ✕ close over a scrollable list of the
/// shortcuts. Each section lays its rows into two column-major columns (so related
/// consecutive actions stay together) and renders shortcuts as keycap pills.
struct HelpPanelView: View {
    let sections: [HelpSection]
    let onClose: () -> Void
    /// The available height the overlay grants (window height minus insets). The panel
    /// grows to fit its content up to this, then scrolls — so a tall window shows the
    /// whole cheatsheet and a short one collapses it.
    let maxHeight: CGFloat

    /// Measured natural height of the shortcut list (via the preference key below).
    @State private var contentHeight: CGFloat = 0

    /// Title bar + groove height — subtracted from `maxHeight` for the scroll budget.
    private let chromeHeight = PanelMetrics.headerHeight + 2  // header + groove

    private var scrollHeight: CGFloat {
        let available = max(160, maxHeight - chromeHeight)
        // Assume full until measured, so the panel shrinks to fit rather than growing
        // from zero (no first-frame flash).
        let content = contentHeight > 0 ? contentHeight : available
        return min(content, available)
    }

    var body: some View {
        VStack(spacing: 0) {
            PanelHeader(title: "Keyboard Shortcuts", closeHelp: "Close (?)", onClose: onClose)

            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    ForEach(sections) { section in
                        sectionView(section)
                    }
                }
                .padding(.horizontal, 18)
                .padding(.vertical, 14)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(
                    GeometryReader { g in
                        Color.clear.preference(key: HelpContentHeight.self, value: g.size.height)
                    }
                )
            }
            .frame(height: scrollHeight)
            .onPreferenceChange(HelpContentHeight.self) { contentHeight = $0 }
        }
        .frame(width: 520)
        .panelBackground()
        .arrowCursorOnHover()
    }

    @ViewBuilder
    private func sectionView(_ section: HelpSection) -> some View {
        // Split column-major (ceil in the left column) so consecutive related rows —
        // e.g. Slideshow and Slideshow faster/slower — land in the same column.
        let mid = (section.items.count + 1) / 2
        let left = Array(section.items.prefix(mid))
        let right = Array(section.items.dropFirst(mid))
        VStack(alignment: .leading, spacing: 9) {
            // A tinted header bar (the keycap shading) makes the section stand out — a
            // real grouped heading, not a faint sidebar label. The text stays at the
            // section's leading edge (aligned with the command labels below); the tint
            // bar is *outset* around it (negative padding on the background) so the
            // capsule padding doesn't shove the heading right — the mac-idiomatic look.
            Text(section.title)
                .font(.headline)
                .foregroundStyle(.primary)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.vertical, 4)
                .background(
                    RoundedRectangle(cornerRadius: 6)
                        .fill(.quaternary)
                        .padding(.horizontal, -8)
                )
            HStack(alignment: .top, spacing: 28) {
                column(left)
                column(right)
            }
        }
    }

    private func column(_ items: [HelpItem]) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(items) { item in
                HStack(spacing: 8) {
                    Text(item.label)
                        .lineLimit(1)
                        .minimumScaleFactor(0.9) // shrink a hair before truncating
                    Spacer(minLength: 10)
                    ShortcutView(shortcut: item.shortcut)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// Renders a shortcut string as keycap pills with dim `/` separators between
/// alternatives. Parsing: split on `" / "` (the spaced slash) into alternative groups,
/// then split each group on spaces into individual keys — so a key `/` (in a pill) is
/// never confused with the alternatives separator (plain, dimmed). Handles "Space",
/// "⇧ ↩", "[ / ]", "← ↑ ↓ →", "R / ⇧ R", "/ / ?", "F11".
struct ShortcutView: View {
    let shortcut: String

    var body: some View {
        let groups = parse(shortcut)
        HStack(spacing: 5) {
            ForEach(Array(groups.enumerated()), id: \.offset) { gi, keys in
                if gi > 0 {
                    Text("/")
                        .font(.callout)
                        .foregroundStyle(.tertiary)
                }
                ForEach(Array(keys.enumerated()), id: \.offset) { _, key in
                    keycap(key)
                }
            }
        }
    }

    private func keycap(_ s: String) -> some View {
        // The shared soft-pill keycap (see `Keycap`) — same as the play hint and welcome opens.
        Keycap(text: s)
    }

    private func parse(_ s: String) -> [[String]] {
        s.components(separatedBy: " / ")
            .map { $0.split(separator: " ").map(String.init) }
            .filter { !$0.isEmpty }
    }
}
