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
    /// The window's backing scale — text metrics differ between 1x and 2x (glyph
    /// advances round differently), so a width measured on one display can be a point
    /// short on another. Watched below to re-measure on a display move.
    @Environment(\.displayScale) private var displayScale

    var body: some View {
        VStack(spacing: 0) {
            // Primary actions — equal-weight (neither emphasized), each carrying its key;
            // stacked vertically so the two pills line up. Gap is the app's shared spacing
            // (`Layout.edge`) — the same used between stacked chrome elements everywhere else.
            VStack(spacing: Layout.edge) {
                openButton("Open File", icon: "doc", key: model.shortcut("open_file")) {
                    model.openFile()
                }
                openButton("Open Folder", icon: "folder", key: model.shortcut("open_folder")) {
                    model.openFolder()
                }
            }
            .onPreferenceChange(OpenButtonWidth.self) { openButtonWidth = $0 }
            // Once measured, the fixed frame BOUNDS the content, so the GeometryReader
            // can never report "the text got wider" — a clamp measured on a 1x display
            // wrapped "Open Folder" after a drag to a 2x display (owner report,
            // 2026-07-11). Re-enter measuring mode whenever the metrics' inputs change:
            // the backing scale (display move) or the keycap strings (keymap edit).
            .onChange(of: displayScale) { openButtonWidth = 0 }
            .onChange(of: model.shortcut("open_file") + model.shortcut("open_folder")) {
                openButtonWidth = 0
            }

            // The drag-and-drop alternative sits under the buttons — the shared gap, pulled in
            // 8pt since it's a lighter, secondary line and reads better a touch closer.
            Text("or drag and drop here")
                .font(.callout)
                .foregroundStyle(.secondary)
                .padding(.top, Layout.edge - 6)

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
                // One line, always: if a stale width clamp is ever a point short (the
                // single frame before a re-measure lands), truncate — never wrap the
                // pill to two lines.
                Text(label).lineLimit(1).padding(.leading, 8)
                Spacer(minLength: 14)
                if !key.isEmpty {
                    ShortcutView(shortcut: key)
                }
            }
            .font(.callout)
            // Equal width: measure only the *content* (both buttons size to the wider), so the
            // padding + material below wrap an already-equal width — the pills match.
            .fixedSize(horizontal: openButtonWidth == 0, vertical: false)
            .frame(width: openButtonWidth > 0 ? openButtonWidth : nil, alignment: .leading)
            .background(
                GeometryReader { g in
                    Color.clear.preference(key: OpenButtonWidth.self, value: g.size.width)
                }
            )
            // Same pill as the play hint: the shared chrome material / border / shadow at the
            // chrome radius, not a system bordered button — so the on-canvas controls read as
            // one design language.
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .panelBackground(opacity: model.panelOpacity)
            .contentShape(RoundedRectangle(cornerRadius: PanelMetrics.cornerRadius))
        }
        .buttonStyle(.plain)
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
