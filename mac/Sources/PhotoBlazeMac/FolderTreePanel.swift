// The native folder tree (⇧F, task #54) — a Finder-style browser for disk decks (chevron
// expand/collapse + name-to-open, decoupled so you can walk photo-less folders to find
// one), falling back to the v1 flat list for archive/empty decks (click-to-activate). The
// core owns the resident tree model (FsTree) + navigation; this view renders its flattened
// rows on the leading edge. `treeUsesFs` picks the interaction: chevrons + separate
// open/toggle (Finder) vs a single activate per row (v1).

import SwiftUI

/// One folder-tree row. `depth` sets the indent; `isCurrent` = "you are here"; `isUp` =
/// the v1 up affordance; `hasChildren` = worth a chevron (Finder); `expanded`/`loading` =
/// chevron state; `count` = photo badge (-1 none); `hasTarget` = openable.
struct FolderTreeRow: Identifiable {
    let id: Int
    let depth: Int
    let name: String
    let isCurrent: Bool
    let isUp: Bool
    let hasChildren: Bool
    let expanded: Bool
    let loading: Bool
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
            HStack(spacing: 8) {
                Text("Folders")
                    .font(.headline)
                Spacer()
                Button(action: { model.closeTree() }) {
                    Image(systemName: "xmark.circle.fill")
                        .foregroundStyle(Color.panelSecondary)
                        .imageScale(.large)
                }
                .buttonStyle(.plain)
                .help("Close (⇧F)")
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)

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
        .arrowCursorOnHover()
    }

    @ViewBuilder
    private func rowView(_ row: FolderTreeRow) -> some View {
        HStack(spacing: 4) {
            // Disclosure chevron (Finder tree) — expand/collapse, no photo load.
            if model.treeUsesFs {
                disclosure(row)
            }
            // Icon + name — a name click opens the folder (loads its photos).
            Button(action: { model.activateTreeRow(row.id) }) {
                HStack(spacing: 6) {
                    // Solid (not `.secondary`) so folder glyphs stay legible over the
                    // translucent panel material when bright photo content is behind it.
                    Image(systemName: icon(row))
                        .foregroundStyle(row.isCurrent ? Color.accentColor : Color.panelSecondary)
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
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .disabled(!row.hasTarget)
        }
        .padding(.leading, CGFloat(row.depth) * 14 + 10)
        .padding(.trailing, 12)
        .padding(.vertical, 3)
        .background(row.isCurrent ? Color.accentColor.opacity(0.14) : Color.clear)
    }

    @ViewBuilder
    private func disclosure(_ row: FolderTreeRow) -> some View {
        if row.loading {
            ProgressView()
                .controlSize(.small)
                .scaleEffect(0.7)
                .frame(width: 16)
        } else if row.hasChildren {
            Button(action: { model.toggleTreeRow(row.id) }) {
                Image(systemName: "chevron.right")
                    .font(.caption2)
                    .foregroundStyle(Color.panelSecondary)
                    .rotationEffect(.degrees(row.expanded ? 90 : 0))
                    .frame(width: 16, height: 16)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
        } else {
            Color.clear.frame(width: 16)
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
