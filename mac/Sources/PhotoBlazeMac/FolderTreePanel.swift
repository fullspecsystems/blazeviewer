// The native folder tree (⇧F, task #54) — the current photo's folder in its hierarchy
// (up affordance, root, ancestor chain, the current folder, siblings, children), so you
// can see "where am I" and jump around. The core derives the rows + navigation targets
// (unchanged from the HUD tree); this view renders them as a depth-indented, scrolling
// list on the leading edge. A native list scrolls, so the HUD's windowing / "… n more"
// paging is gone — every folder shows. Clicking a row with a target navigates (full Open
// Folder semantics / archive re-scope), routed back through the core.

import SwiftUI

/// One folder-tree row. `depth` sets the indent; `isCurrent` is "you are here"
/// (highlighted, not clickable); `isUp` is the leading "up to parent" affordance;
/// `count` is the photo-count badge (-1 = none); `hasTarget` = clickable.
struct FolderTreeRow: Identifiable {
    let id: Int
    let depth: Int
    let name: String
    let isCurrent: Bool
    let isUp: Bool
    let count: Int
    let hasTarget: Bool
}

/// The floating folder-tree card: a titled panel over a scrollable, indented row list.
struct FolderTreePanelView: View {
    let model: CoreModel
    let maxHeight: CGFloat

    @State private var contentHeight: CGFloat = 0

    private let chromeHeight: CGFloat = 42

    private var scrollHeight: CGFloat {
        let available = max(120, maxHeight - chromeHeight)
        let content = contentHeight > 0 ? contentHeight : available
        return min(content, available)
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("Folders")
                    .font(.headline)
                Spacer()
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 11)

            Rectangle()
                .fill(Color.primary.opacity(0.08))
                .frame(height: 1)

            ScrollView {
                VStack(alignment: .leading, spacing: 1) {
                    ForEach(model.treeRows) { row in
                        rowView(row)
                    }
                }
                .padding(.vertical, 6)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(
                    GeometryReader { g in
                        Color.clear.preference(
                            key: TreeContentHeight.self, value: g.size.height)
                    }
                )
            }
            .frame(height: scrollHeight)
            .onPreferenceChange(TreeContentHeight.self) { contentHeight = $0 }
        }
        .frame(width: 280)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        .overlay(
            RoundedRectangle(cornerRadius: 12)
                .strokeBorder(.separator, lineWidth: 0.5)
        )
        .shadow(radius: 24, y: 8)
    }

    @ViewBuilder
    private func rowView(_ row: FolderTreeRow) -> some View {
        let content = HStack(spacing: 6) {
            Image(systemName: icon(row))
                .foregroundStyle(row.isCurrent ? Color.accentColor : .secondary)
                .frame(width: 16)
            Text(row.name)
                .lineLimit(1)
                .truncationMode(.middle)
                .fontWeight(row.isCurrent ? .semibold : .regular)
            Spacer(minLength: 6)
            if row.count >= 0 {
                Text("\(row.count)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 1)
                    .background(.quaternary, in: Capsule())
            }
        }
        .padding(.leading, CGFloat(row.depth) * 14 + 12)
        .padding(.trailing, 12)
        .padding(.vertical, 4)
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())

        if row.hasTarget {
            Button(action: { model.activateTreeRow(row.id) }) { content }
                .buttonStyle(.plain)
        } else {
            content
                .background(
                    row.isCurrent ? Color.accentColor.opacity(0.14) : Color.clear
                )
        }
    }

    private func icon(_ row: FolderTreeRow) -> String {
        if row.isUp { return "arrow.up.left" }
        if row.isCurrent { return "folder.fill" }
        return "folder"
    }
}

/// Measures the row list's natural height for fit-to-content-then-scroll.
private struct TreeContentHeight: PreferenceKey {
    static var defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = max(value, nextValue())
    }
}
