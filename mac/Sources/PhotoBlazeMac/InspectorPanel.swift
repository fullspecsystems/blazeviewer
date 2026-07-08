// The native Inspector panel (task #54, ADR-023) — the single tabbed content panel that
// replaces the HUD's separate Info / Text / Describe overlays: Details (full EXIF), Text
// (recognized text + QR), Describe (the AI description, rendered as Markdown). The core
// owns the state machine (which tab, when to scan) and the semantic rows; this view
// renders the active tab's rows (flattened over FFI as typed `(kind, a, b)` rows). The
// tab bar doubles as the header (no redundant "Inspector" title) with an inline ✕; tab
// switches and ✕ route back to the core, which re-signals `PanelsChanged` — including when
// an async OCR / describe result lands.

import SwiftUI

/// Measures the content's natural height so the panel fits its content up to the window's
/// available height, then scrolls.
private struct InspectorContentHeight: PreferenceKey {
    static var defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = max(value, nextValue())
    }
}

/// One flattened Inspector row. `kind`: 0 header (bold span — filename / section title),
/// 1 label/value pair (metadata), 2 body paragraph (OCR text, or Describe prose rendered
/// as Markdown), 3 status (muted — "Reading text…", "Press D to describe", errors),
/// 4 sub-header (regular-weight span tucked directly under its header — the folder path
/// under the filename). `a` = header/body/status text or the pair label; `b` = the pair
/// value (empty otherwise).
struct InspectorRow: Identifiable {
    let id: Int
    let kind: Int
    let a: String
    let b: String
}

/// The floating Inspector card: a custom icon+text tab bar (Details / Text / Describe)
/// with an inline ✕, over the active tab's scrollable rows. Top-right anchored (parallel
/// to the folder tree, no layout shift on tab switch); content is selectable.
struct InspectorPanelView: View {
    let model: CoreModel
    /// The available height the overlay grants (window height minus insets). The panel
    /// grows to fit its content up to this, then scrolls.
    let maxHeight: CGFloat
    /// The widest the panel may be dragged (the window minus a margin).
    let maxWidth: CGFloat

    @State private var contentHeight: CGFloat = 0
    /// The copy button briefly swaps to a checkmark to confirm the copy landed.
    @State private var copied = false

    /// Tab bar + groove — subtracted from `maxHeight` for the scroll budget.
    private let chromeHeight = PanelMetrics.headerHeight + 2  // header + groove

    private var scrollHeight: CGFloat {
        let available = max(140, maxHeight - chromeHeight)
        let content = contentHeight > 0 ? contentHeight : available
        return min(content, available)
    }

    /// Below this width the segmented control's icon + label pair overflows (the
    /// "Describe" segment is the tightest), so drop the icons and keep the labels —
    /// the words carry the meaning; the glyphs are only reinforcement. Lets the panel
    /// shrink to the 280pt floor cleanly. (Threshold sits just above the ~294pt where
    /// icon+label starts to clip.)
    private var compactTabs: Bool {
        min(model.inspectorWidth, maxWidth) < 300
    }

    /// The segmented control is inset from the header's top, bottom, and leading edges by one
    /// `segMargin` (a symmetric inset), with a corner radius concentric with the panel corner
    /// (`cornerRadius − segMargin`). Because the header is `2·cornerRadius` tall, that works out
    /// to a capsule track (height = 2·radius); the selected pill nests one more step in, by the
    /// track's inner padding, so it's concentric with the track.
    private let segMargin: CGFloat = 6
    private let trackPad: CGFloat = 2
    private var segTrackHeight: CGFloat { PanelMetrics.headerHeight - 2 * segMargin }
    private var segRadius: CGFloat { PanelMetrics.cornerRadius - segMargin }
    private var pillRadius: CGFloat { segRadius - trackPad }

    var body: some View {
        VStack(spacing: 0) {
            // The tab bar is the header: three facets + an inline ✕ dismiss.
            HStack(spacing: 10) {
                // A segmented control: equal-width segments in a subtle track, the
                // selected one filled solid-accent with white text (the idiomatic macOS
                // selected-segment look — high contrast, and the fill can't wash out).
                HStack(spacing: 2) {
                    tab(0, "Details", "info.circle")
                    tab(1, "Text", "text.viewfinder")
                    tab(2, "Describe", "sparkles")
                }
                // Force the track to `segTrackHeight` (tabs + inner padding), so the vertical
                // margin equals `segMargin` — matching the leading inset below.
                .frame(height: segTrackHeight - 2 * trackPad)
                .padding(trackPad)
                .background(.quaternary, in: RoundedRectangle(cornerRadius: segRadius))
                // Copy the whole active tab (details / text / description) — one consistent
                // action across all three, so you never have to hand-select the text. It
                // swaps to a checkmark for ~1s so you can see the copy landed (a clearer,
                // in-place confirmation than the HUD toast).
                Button(action: { copyTab() }) {
                    Image(systemName: copied ? "checkmark" : "doc.on.doc")
                        .foregroundStyle(copied ? Color.green : Color.panelSecondary)
                        .imageScale(.medium)
                        .contentTransition(.symbolEffect(.replace))
                }
                .buttonStyle(.plain)
                .help(model.inspectorCopyLabel)
                .disabled(copied)
                PanelCloseButton(action: { model.closeInspector() })
            }
            // Same header geometry as the other panels: forced to the shared header height (so
            // the ✕ stays vertically concentric, matching Folders / Help). The leading inset is
            // `segMargin` so the segmented control's top/bottom/left margins are uniform; the ✕
            // keeps its own concentric trailing inset.
            .padding(.leading, segMargin)
            .padding(.trailing, PanelMetrics.closeInset)
            .frame(height: PanelMetrics.headerHeight)

            PanelDivider()

            ScrollView {
                VStack(alignment: .leading, spacing: 8) {
                    if model.inspectorRows.isEmpty {
                        Text("Nothing to show")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    } else {
                        ForEach(model.inspectorRows) { row in
                            rowView(row)
                        }
                    }
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 14)
                .frame(maxWidth: .infinity, alignment: .leading)
                // Enable selection at the container, not per row — so a drag selects across
                // rows (the whole readout) instead of one value at a time. The ⧉ button is the
                // one-click "copy everything" path; this is for grabbing a piece by hand.
                .textSelection(.enabled)
                .background(
                    GeometryReader { g in
                        Color.clear.preference(
                            key: InspectorContentHeight.self, value: g.size.height)
                    }
                )
            }
            .frame(height: scrollHeight)
            .onPreferenceChange(InspectorContentHeight.self) { contentHeight = $0 }
        }
        .frame(width: min(model.inspectorWidth, maxWidth))
        .panelBackground(opacity: model.panelOpacity)
        // Drag the leading edge to widen (280pt minimum — matches the Folders panel).
        .overlay(alignment: .leading) {
            ResizeHandle(
                model: model,
                width: Binding(
                    get: { model.inspectorWidth }, set: { model.inspectorWidth = $0 }),
                minWidth: 280, maxWidth: maxWidth, sign: -1)
        }
        .arrowCursorOnHover()
    }

    /// One segment: icon + label, equal-width. Selected → solid-accent fill + white text
    /// (idiomatic, high contrast); unselected → the dim panel-secondary. Constant weight,
    /// so the segment never resizes when it becomes active. Narrow panels drop the icon
    /// (see `compactTabs`) and keep the label.
    private func tab(_ index: Int, _ label: String, _ icon: String) -> some View {
        let selected = model.inspectorTab == index
        return Button(action: { model.showInspectorTab(index) }) {
            HStack(spacing: 4) {
                if !compactTabs {
                    Image(systemName: icon)
                }
                Text(label)
            }
            .font(.callout)
            .foregroundStyle(selected ? Color.white : Color.panelSecondary)
            // Fill the track (its fixed height sizes the segments now), so the selected pill
            // fills it; the pill radius is concentric with the track.
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background {
                if selected {
                    RoundedRectangle(cornerRadius: pillRadius).fill(Color.accentColor)
                }
            }
            .contentShape(RoundedRectangle(cornerRadius: pillRadius))
        }
        .buttonStyle(.plain)
    }

    /// Copy the active tab, then briefly flip the button to a checkmark as confirmation.
    private func copyTab() {
        model.copyInspectorTab()
        withAnimation(.easeInOut(duration: 0.12)) { copied = true }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.55) {
            withAnimation(.easeInOut(duration: 0.15)) { copied = false }
        }
    }

    @ViewBuilder
    private func rowView(_ row: InspectorRow) -> some View {
        switch row.kind {
        case 0:
            // A bold heading (the filename, or a Details section title). On the Describe tab
            // the heading is the always-present "Description" title, so it carries the Ask
            // button — a stable spot (the tab bar resized as the button showed/hid per tab).
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text(row.a)
                    .font(.headline)
                if model.inspectorTab == 2 {
                    Spacer(minLength: 8)
                    Button(action: { model.menuAction("ask_image") }) {
                        Label("Ask", systemImage: "questionmark.bubble")
                            .font(.callout)
                            .foregroundStyle(Color.accentColor)
                    }
                    .buttonStyle(.plain)
                    .help("Ask a question about this image")
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.top, 4)
        case 4:
            // A sub-header (the folder path) in regular weight, pulled tight under the
            // filename header — the negative top padding cancels the VStack's row gap so
            // it reads as one stacked "name / path" pair.
            Text(row.a)
                .font(.callout)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.top, -8)
        case 1:
            // A label / value metadata pair — label in a fixed leading column.
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Text(row.a)
                    .font(.callout)
                    .foregroundStyle(Color.panelSecondary)
                    .frame(width: 116, alignment: .leading)
                Text(row.b)
                    .font(.callout)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        case 3:
            // A muted status line (scanning / idle / error).
            Text(row.a)
                .font(.callout)
                .foregroundStyle(Color.panelSecondary)
                .frame(maxWidth: .infinity, alignment: .leading)
        default:
            // A body paragraph. Describe = AI text → block-level Markdown (headings, lists,
            // emphasis); Text/OCR stays literal.
            if model.inspectorTab == 2 {
                MarkdownBlocksView(text: row.a)
                    .frame(maxWidth: .infinity, alignment: .leading)
            } else {
                Text(row.a)
                    .font(.callout)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }
}
