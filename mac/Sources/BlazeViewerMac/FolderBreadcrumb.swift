// The Thumbnails-panel folder breadcrumb / path bar (task #129) — a thin, Finder-path-bar-style
// strip below the tab header that shows the CURRENT photo's folder (tracked live as you blaze,
// even with the Folders tab hidden) and lets you jump UP the tree in one click. Down is what the
// strip/tree are for; the bar's value-add is up.
//
// A custom SwiftUI control, not `NSPathControl`: these panels are translucent, tokenized, custom-
// hover chrome, and NSPathControl draws its own opaque background / focus ring / drag behavior and
// would do filesystem icon/metadata lookups on network paths. `NSPathControl` is the idiom to
// emulate, not the control to embed.
//
// The click routes through `CoreModel.openFolderPath` → the `open_tree_folder` FFI →
// `AppCore::fs_tree_open` — the SAME folder-open the Folders tree performs, so the two surfaces
// can't drift. It ASYNC-queues a recursive scan; broad ancestors (`/`, `/Volumes`, a volume/share
// root) are shown as context but are NOT one-click scan targets (a whole-volume/SMB rescan is the
// expensive case — see `isInteractive`).

import AppKit
import PbBreadcrumb
import SwiftUI

// `Crumb` + `FolderBreadcrumbModel` (the pure path→crumbs derivation and the boundary rule) live
// in the FFI-free `PbBreadcrumb` package so they `swift test` in isolation; the fitting/measurement
// and presentation below stay here because they need AppKit text metrics + SwiftUI.

/// The breadcrumb strip: an optional leading overflow menu (the ancestors that don't fit) + the
/// trailing crumbs that fit, current last. Laid out right-to-left so the current folder is always
/// visible; the beginning truncates into the menu.
struct FolderBreadcrumbView: View {
    let model: CoreModel
    /// The strip's usable width (the pane width minus the panel's side insets).
    let width: CGFloat

    /// Font used for both measurement and display, kept in lockstep so the fit is exact.
    private static let font = NSFont.systemFont(ofSize: 11)
    private let sepWidth: CGFloat = 13 // a `chevron.compact.right` at this size
    private let overflowWidth: CGFloat = 24 // the leading "…" menu button
    private let sidePad: CGFloat = 10
    private let rowHeight: CGFloat = 22

    var body: some View {
        let crumbs = FolderBreadcrumbModel.crumbs(for: model.breadcrumbPath)
        let split = fit(crumbs, into: max(0, width - 2 * sidePad))
        HStack(spacing: 4) {
            if !split.overflow.isEmpty {
                overflowMenu(split.overflow)
            }
            ForEach(split.visible, id: \.path) { crumb in
                // A separator before every crumb except the very first *when nothing precedes it*
                // (no overflow menu). The current folder is always the last visible crumb.
                if crumb.path != split.visible.first?.path || !split.overflow.isEmpty {
                    Image(systemName: "chevron.compact.right")
                        .font(.system(size: 10))
                        .foregroundStyle(Color.panelSecondary.opacity(0.7))
                }
                crumbLabel(crumb, isCurrent: crumb.path == crumbs.last?.path)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, sidePad)
        .frame(height: rowHeight)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Current folder: \(crumbs.last?.name ?? "")")
    }

    // A single crumb: the current folder is bold and inert (clicking it would only re-open/narrow
    // the deck — deliberately prevented, not a silent no-op); interactive ancestors open on click;
    // broad roots render as dim, inert context.
    @ViewBuilder
    private func crumbLabel(_ crumb: Crumb, isCurrent: Bool) -> some View {
        let interactive = !isCurrent && FolderBreadcrumbModel.isInteractive(crumb.path)
        if interactive {
            Button(action: { model.openFolderPath(crumb.path) }) {
                Text(crumb.name)
                    .font(.caption)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .foregroundStyle(Color.panelSecondary)
            }
            .buttonStyle(.plain)
            .help("Open “\(crumb.name)”")
        } else {
            Text(crumb.name)
                .font(.caption)
                .fontWeight(isCurrent ? .semibold : .regular)
                .lineLimit(1)
                .truncationMode(.middle)
                .foregroundStyle(isCurrent ? Color.primary : Color.panelSecondary.opacity(0.7))
        }
    }

    // The leading overflow: nearest ancestor first, down toward the root. Non-interactive roots
    // stay listed (as disabled context) so the menu always shows the full ancestry.
    @ViewBuilder
    private func overflowMenu(_ overflow: [Crumb]) -> some View {
        Menu {
            ForEach(overflow.reversed(), id: \.path) { crumb in
                Button(crumb.name) { model.openFolderPath(crumb.path) }
                    .disabled(!FolderBreadcrumbModel.isInteractive(crumb.path))
            }
        } label: {
            Image(systemName: "ellipsis")
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(Color.panelSecondary)
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .fixedSize()
        .help("Go up")
        .accessibilityLabel("Parent folders")
    }

    /// Greedy right-to-left fit: always keep the current crumb (truncating if it must), then add
    /// parents while they fit, reserving room for the overflow menu whenever ancestors remain
    /// hidden. Returns the hidden (leading) crumbs and the visible (trailing) crumbs, in order.
    private func fit(_ all: [Crumb], into available: CGFloat) -> (overflow: [Crumb], visible: [Crumb]) {
        guard !all.isEmpty else { return ([], []) }
        var visible: [Crumb] = []
        var used: CGFloat = 0
        for i in stride(from: all.count - 1, through: 0, by: -1) {
            let crumb = all[i]
            let w = measure(crumb.name) + (visible.isEmpty ? 0 : sepWidth)
            let hiddenRemain = i // crumbs strictly before this one
            let reserve: CGFloat = hiddenRemain > 0 ? overflowWidth + sepWidth : 0
            if visible.isEmpty || used + w + reserve <= available {
                used += w
                visible.insert(crumb, at: 0)
            } else {
                break
            }
        }
        let overflow = Array(all.prefix(all.count - visible.count))
        return (overflow, visible)
    }

    private func measure(_ s: String) -> CGFloat {
        (s as NSString).size(withAttributes: [.font: FolderBreadcrumbView.font]).width + 6
    }
}
