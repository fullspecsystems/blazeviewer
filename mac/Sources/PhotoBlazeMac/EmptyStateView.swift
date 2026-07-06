// The empty-state / welcome surface (task #54, mac-first) — shown natively over the
// blank canvas when no photos are loaded. It replaces the HUD "Press O to open" panel;
// because it's a real SwiftUI view, its buttons carry their own hover/click (so the
// cursor no longer leaks through an overlaid panel to a HUD hit-rect). Kept deliberately
// minimal (owner call 2026-07-05): just the two opens + drag-and-drop — see the note in
// `body` on why the nav/shortcut tips were removed. The core owns visibility
// (`open_panel_visible`) and the shortcut lookups (`action_shortcut`); this view just
// composes them.

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
            // Primary actions — equal-weight (neither emphasized), each carrying its key;
            // stacked vertically so the two pills line up.
            VStack(spacing: 10) {
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

            // Owner call (2026-07-05): the Next/Previous/Random tips + "Show Shortcuts"
            // link that used to live here made the welcome screen feel cluttered. This
            // screen should stay just the two opens + drag-and-drop for a clean first
            // impression; a togglable toolbar (task #55) is the planned home for
            // discoverable nav/panel affordances instead — visible by default for new
            // users, hideable for advanced ones.
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
        // These sit directly on the photo canvas (not a panel), so they get the on-image
        // hover cue too — the same "alive" language as the play hint, not a panel's.
        .onImageHoverGlow()
        // Disable both while an NSOpenPanel is already up (either button spawns one) — else
        // a fast double-click, or clicking the other button, stacked a second panel on top
        // of the first. `.disabled` must come after `.onImageHoverGlow` so the environment
        // reaches it and the glow doesn't light up a control that can't respond right now.
        .disabled(model.panelOpen)
    }
}
