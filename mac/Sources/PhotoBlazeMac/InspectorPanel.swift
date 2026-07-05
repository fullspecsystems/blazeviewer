// The native Inspector panel (task #54, ADR-023) — the single tabbed content panel that
// replaces the HUD's separate Info / Text / Describe overlays: Details (full EXIF), Text
// (recognized text + QR), Describe (the AI description). The core owns the state machine
// (which tab, when to scan) and the semantic rows; this view renders the active tab's
// rows (flattened over FFI as typed `(kind, a, b)` rows) with the same chrome + keycap
// language as the Help panel. Tab switches and ✕ close route back to the core, which
// re-signals `PanelsChanged` — including when an async OCR / describe result lands.

import SwiftUI

/// Measures the content's natural height so the panel fits its content up to the window's
/// available height, then scrolls (mirrors the Help panel).
private struct InspectorContentHeight: PreferenceKey {
    static var defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = max(value, nextValue())
    }
}

/// One flattened Inspector row. `kind`: 0 header (bold span — filename / section title),
/// 1 label/value pair (metadata), 2 body paragraph (OCR / description prose), 3 status
/// (muted — "Reading text…", "Press D to describe", errors). `a` = header/body/status
/// text or the pair label; `b` = the pair value (empty otherwise).
struct InspectorRow: Identifiable {
    let id: Int
    let kind: Int
    let a: String
    let b: String
}

/// The floating Inspector card: a titled panel with a segmented tab bar (Details / Text /
/// Describe) over the active tab's scrollable rows. Sits on the trailing edge like an
/// inspector sidebar; its content is selectable so any value can be copied.
struct InspectorPanelView: View {
    let model: CoreModel
    /// The available height the overlay grants (window height minus insets). The panel
    /// grows to fit its content up to this, then scrolls.
    let maxHeight: CGFloat

    @State private var contentHeight: CGFloat = 0

    /// Title bar + tab bar + grooves — subtracted from `maxHeight` for the scroll budget.
    private let chromeHeight: CGFloat = 92

    private var scrollHeight: CGFloat {
        let available = max(140, maxHeight - chromeHeight)
        let content = contentHeight > 0 ? contentHeight : available
        return min(content, available)
    }

    private var tabSelection: Binding<Int> {
        Binding(get: { model.inspectorTab }, set: { model.showInspectorTab($0) })
    }

    var body: some View {
        VStack(spacing: 0) {
            // Title bar with the in-band ✕ dismiss.
            HStack {
                Text("Inspector")
                    .font(.headline)
                Spacer()
                Button(action: { model.closeInspector() }) {
                    Image(systemName: "xmark.circle.fill")
                        .foregroundStyle(.secondary)
                        .imageScale(.large)
                }
                .buttonStyle(.plain)
                .help("Close")
            }
            .padding(.horizontal, 16)
            .padding(.top, 12)
            .padding(.bottom, 10)

            // Tab bar: the three facets of "tell me about this image".
            Picker("", selection: tabSelection) {
                Text("Details").tag(0)
                Text("Text").tag(1)
                Text("Describe").tag(2)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding(.horizontal, 16)
            .padding(.bottom, 12)

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
                    .foregroundStyle(.secondary)
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
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, alignment: .leading)
        default:
            // A body paragraph (OCR text / description prose).
            Text(row.a)
                .font(.callout)
                .frame(maxWidth: .infinity, alignment: .leading)
                .textSelection(.enabled)
        }
    }
}
