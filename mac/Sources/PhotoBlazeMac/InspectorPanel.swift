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
/// as Markdown), 3 status (muted — "Reading text…", "Press D to describe", errors). `a` =
/// header/body/status text or the pair label; `b` = the pair value (empty otherwise).
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

    @State private var contentHeight: CGFloat = 0

    /// Tab bar + groove — subtracted from `maxHeight` for the scroll budget.
    private let chromeHeight: CGFloat = 54

    private var scrollHeight: CGFloat {
        let available = max(140, maxHeight - chromeHeight)
        let content = contentHeight > 0 ? contentHeight : available
        return min(content, available)
    }

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
                .padding(2)
                .background(.quaternary, in: RoundedRectangle(cornerRadius: 8))
                Button(action: { model.closeInspector() }) {
                    Image(systemName: "xmark.circle.fill")
                        .foregroundStyle(Color.panelSecondary)
                        .imageScale(.large)
                }
                .buttonStyle(.plain)
                .help("Close")
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 9)

            Rectangle()
                .fill(Color.primary.opacity(0.08))
                .frame(height: 1)

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
        .frame(width: 360)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        .overlay(
            RoundedRectangle(cornerRadius: 12)
                .strokeBorder(.separator, lineWidth: 0.5)
        )
        .shadow(radius: 24, y: 8)
        .arrowCursorOnHover()
    }

    /// One segment: icon + label, equal-width. Selected → solid-accent fill + white text
    /// (idiomatic, high contrast); unselected → the dim panel-secondary. Constant weight,
    /// so the segment never resizes when it becomes active.
    private func tab(_ index: Int, _ label: String, _ icon: String) -> some View {
        let selected = model.inspectorTab == index
        return Button(action: { model.showInspectorTab(index) }) {
            HStack(spacing: 4) {
                Image(systemName: icon)
                Text(label)
            }
            .font(.callout)
            .foregroundStyle(selected ? Color.white : Color.panelSecondary)
            .frame(maxWidth: .infinity)
            .padding(.vertical, 4)
            .background {
                if selected {
                    RoundedRectangle(cornerRadius: 6).fill(Color.accentColor)
                }
            }
            .contentShape(RoundedRectangle(cornerRadius: 6))
        }
        .buttonStyle(.plain)
    }

    @ViewBuilder
    private func rowView(_ row: InspectorRow) -> some View {
        switch row.kind {
        case 0:
            // A bold heading (the filename, or a Details section title).
            Text(row.a)
                .font(.headline)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.top, 4)
                .textSelection(.enabled)
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
                    .textSelection(.enabled)
            }
        case 3:
            // A muted status line (scanning / idle / error).
            Text(row.a)
                .font(.callout)
                .foregroundStyle(Color.panelSecondary)
                .frame(maxWidth: .infinity, alignment: .leading)
        default:
            // A body paragraph. Describe prose is Markdown; OCR text is literal.
            bodyText(row.a)
                .font(.callout)
                .frame(maxWidth: .infinity, alignment: .leading)
                .textSelection(.enabled)
        }
    }

    /// The Describe tab renders its prose as Markdown (inline emphasis + preserved
    /// paragraph breaks); every other body (OCR text) stays literal.
    private func bodyText(_ s: String) -> Text {
        guard model.inspectorTab == 2,
            let attributed = try? AttributedString(
                markdown: s,
                options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace))
        else {
            return Text(s)
        }
        return Text(attributed)
    }
}
