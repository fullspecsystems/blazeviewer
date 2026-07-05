// The empty-state / welcome surface (task #54, mac-first) — shown natively over the
// blank canvas when no photos are loaded. It replaces the HUD "Press O to open" panel;
// because it's a real SwiftUI view, its buttons carry their own hover/click (so the
// cursor no longer leaks through an overlaid panel to a HUD hit-rect), and it can grow
// into a proper home screen: identity, the primary open actions, a few essential keys,
// and a link to the full shortcuts panel. The core owns visibility (`open_panel_visible`)
// and the shortcut lookups (`action_shortcut`); this view just composes them.

import SwiftUI

/// The wider of the two open buttons' natural content widths, so both size to it (equal
/// width). Measured off the content itself — never a stretched frame — so there's no
/// feedback loop.
private struct OpenButtonWidth: PreferenceKey {
    static var defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = max(value, nextValue())
    }
}

struct EmptyStateView: View {
    let model: CoreModel

    /// The shared open-button width (0 until first layout → natural size meanwhile).
    @State private var openButtonWidth: CGFloat = 0

    var body: some View {
        VStack(spacing: 0) {
            // Primary actions — equal-weight (neither emphasized), each carrying its key.
            HStack(spacing: 12) {
                openButton("Open File", icon: "doc", key: model.shortcut("open_file")) {
                    model.openFile()
                }
                openButton("Open Folder", icon: "folder", key: model.shortcut("open_folder")) {
                    model.openFolder()
                }
            }
            .controlSize(.large)
            .onPreferenceChange(OpenButtonWidth.self) { openButtonWidth = $0 }

            // The drag-and-drop alternative sits just under the buttons (tightly related).
            Text("or drag and drop here")
                .font(.callout)
                .foregroundStyle(.secondary)
                .padding(.top, 12)

            // A wide gap sets the shortcut hints well apart — they're reference, not
            // actions, so they read as clearly secondary to everything above.
            VStack(spacing: 12) {
                HStack(spacing: 20) {
                    tip("next", "Next")
                    tip("prev", "Previous")
                    tip("random", "Random")
                }
                // A de-emphasized link (not a bordered button) — secondary to the opens.
                Button(action: { model.showAllShortcuts() }) {
                    HStack(spacing: 6) {
                        Text("Show Shortcuts")
                        ShortcutView(shortcut: "?")
                    }
                }
                .buttonStyle(.link)
            }
            .padding(.top, 48)
        }
        .padding(48)
        // No keyboard-focus ring on the opens — the core owns keys, and a blue outline
        // on the first button reads as an errant default.
        .focusEffectDisabled()
    }

    /// An equal-weight open button: icon + label on the left, shortcut keycap pushed to
    /// the right edge (menu-item style), both buttons sized to the wider one's content.
    ///
    /// The `Spacer` is what right-aligns the key — but a `Spacer` is greedy, so measuring
    /// it directly would stretch to the window and feed a runaway width. The trick:
    /// `.fixedSize` **only while measuring** (`openButtonWidth == 0`) collapses the Spacer
    /// to its minimum so the `GeometryReader` reads the true content width; once that width
    /// is known, the *fixed* frame bounds the Spacer, so it fills the slack (right-aligning
    /// the key) instead of ballooning. A fixed width can't grow → no feedback loop.
    private func openButton(
        _ label: String, icon: String, key: String, action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            HStack(spacing: 0) {
                Image(systemName: icon)
                Text(label).padding(.leading, 8)
                Spacer(minLength: 14)
                if !key.isEmpty {
                    ShortcutView(shortcut: key)
                }
            }
            .fixedSize(horizontal: openButtonWidth == 0, vertical: false)
            .frame(width: openButtonWidth > 0 ? openButtonWidth : nil, alignment: .leading)
            .background(
                GeometryReader { g in
                    Color.clear.preference(key: OpenButtonWidth.self, value: g.size.width)
                }
            )
        }
        .buttonStyle(.bordered)
    }

    /// A label + keycap tip (e.g. Next [Space]) — label first, key after, matching the
    /// buttons and the Show Shortcuts link. Non-interactive text (these keys do nothing
    /// with no photo loaded); hidden if the action is unbound.
    @ViewBuilder
    private func tip(_ actionId: String, _ label: String) -> some View {
        let key = model.shortcut(actionId)
        if !key.isEmpty {
            HStack(spacing: 6) {
                Text(label)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                ShortcutView(shortcut: key)
            }
        }
    }
}
