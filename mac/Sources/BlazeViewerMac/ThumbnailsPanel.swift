// The native Thumbnails strip (⇧T, task #83) — the left pane's second tab beside
// Folders: a scrollable vertical strip of neighbor thumbnails with click-to-jump,
// auto-follow (manual scroll detaches; the next nav re-engages, generation-fenced
// so our own scroll animation is never mistaken for the user's), fixed uniform
// cells (image fit-within a 3:2 box + a middle-truncated filename), type badges,
// session rotation at draw, and a broken-image glyph for failed items. Pixels
// come from the core's RAM-only thumb store over per-cell pull-once FFI.

import SwiftUI

/// The shared left-pane tab bar (Folders | Thumbnails) — the Inspector's
/// segmented-control header, two segments, used by BOTH left-pane tabs so the
/// pane reads as one panel with two facets.
struct LeftPaneTabBar: View {
    let model: CoreModel
    /// The selected segment is supplied by the HOSTING panel (0 = Folders in
    /// FolderTreePanelView, 1 = Thumbnails in ThumbnailsPanelView): the mount
    /// condition already encodes which tab is showing, so the highlight can
    /// never go stale against the visible content (the ⇧F-switch bug class).
    let selected: Int
    let width: CGFloat
    let closeHelp: String
    let onClose: () -> Void

    /// Two segments fit icon+label down to ~233pt (the Inspector's 300 is for
    /// three); below that the icons hide and the labels stay. 225 left the
    /// glyphs kissing the pill edges at the floor (owner screenshot) — +8pt
    /// buys the breathing room without hiding icons any earlier than needed.
    private var compactTabs: Bool { width < 233 }
    private let segMargin: CGFloat = 6
    private let trackPad: CGFloat = 2
    private var segTrackHeight: CGFloat { PanelMetrics.headerHeight - 2 * segMargin }
    private var segRadius: CGFloat { PanelMetrics.cornerRadius - segMargin }
    private var pillRadius: CGFloat { segRadius - trackPad }

    var body: some View {
        HStack(spacing: 10) {
            HStack(spacing: 2) {
                tab(0, "Folders", "folder")
                tab(1, "Thumbnails", "photo.on.rectangle")
            }
            .frame(height: segTrackHeight - 2 * trackPad)
            .padding(trackPad)
            .background(.quaternary, in: RoundedRectangle(cornerRadius: segRadius))
            PanelCloseButton(action: onClose)
        }
        .padding(.leading, segMargin)
        .padding(.trailing, PanelMetrics.closeInset)
        .frame(height: PanelMetrics.headerHeight)
        .help(closeHelp)
    }

    private func tab(_ index: Int, _ label: String, _ icon: String) -> some View {
        let selected = self.selected == index
        return Button(action: { model.showLeftTab(index) }) {
            HStack(spacing: 4) {
                if !compactTabs {
                    Image(systemName: icon)
                }
                Text(label)
            }
            .font(.callout)
            .foregroundStyle(selected ? Color.white : Color.panelSecondary)
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
}

/// The floating Thumbnails card: tab bar over a virtualized fixed-cell strip.
struct ThumbnailsPanelView: View {
    let model: CoreModel
    let maxHeight: CGFloat
    let maxWidth: CGFloat

    /// Cells the LazyVStack currently has realized (≈ visible + SwiftUI's own
    /// render-ahead) — the demand viewport reported to the core, debounced.
    @State private var realized = Set<Int>()
    @State private var reportTask: Task<Void, Never>? = nil
    /// The generation of our own in-flight follow-scroll animation, so observed
    /// scroll movement during it is not treated as the user grabbing the list.
    @State private var animatingGen: UInt64 = 0
    /// The item our last follow scroll centered — the smooth-vs-snap distance rule.
    @State private var lastCentered = -1

    private let chromeHeight = PanelMetrics.headerHeight + 2
    /// Strip side inset + the cell's own inner padding around the photo
    /// (owner polish #1: ONE cell background, photo breathing room inside it).
    private let stripPad: CGFloat = 8
    private let cellInnerPad: CGFloat = 7
    /// The label band's own gaps run TIGHTER than the image margin (4 vs 7):
    /// the caption's internal leading adds ~3pt of visual air, so equal
    /// constants read as a bottom-heavy cell (owner note, 2026-07-12).
    private let labelGap: CGFloat = 4
    private let labelHeight: CGFloat = 15
    private let cellGap: CGFloat = 6
    private var paneWidth: CGFloat { min(model.treeWidth, maxWidth) }
    private var cellWidth: CGFloat { max(80, paneWidth - 2 * stripPad) }
    private var boxWidth: CGFloat { cellWidth - 2 * cellInnerPad }
    /// Fixed ~3:2 landscape box (the camera-library common case); portraits and
    /// panoramas letterbox inside it — cells NEVER vary, so rotation can't
    /// reflow the strip and scroll↔index stays O(1).
    private var boxHeight: CGFloat { (boxWidth * 2 / 3).rounded() }
    private var cellHeight: CGFloat {
        // Photo box + its top margin + the label band with its two tighter gaps.
        boxHeight + cellInnerPad + labelHeight + 2 * labelGap
    }
    private var rowHeight: CGFloat { cellHeight + cellGap }
    private var visibleRows: Int { max(1, Int((maxHeight - chromeHeight) / rowHeight)) }

    var body: some View {
        VStack(spacing: 0) {
            LeftPaneTabBar(
                model: model, selected: 1, width: paneWidth,
                closeHelp: "Close (⇧T)"
            ) { model.closeThumbs() }
            PanelDivider()

            ScrollViewReader { proxy in
                scrollBody
                    .onChange(of: model.thumbScrollSeq, initial: true) { _, _ in
                        followScroll(proxy)
                    }
            }
        }
        .frame(width: paneWidth)
        .panelBackground(opacity: model.panelOpacity)
        .overlay(alignment: .trailing) {
            // The SHARED left-pane width (both tabs, one knob — the Inspector
            // idiom); 200pt floor for both, long folder names truncate.
            ResizeHandle(
                model: model,
                width: Binding(get: { model.treeWidth }, set: { model.treeWidth = $0 }),
                minWidth: 200, maxWidth: maxWidth, sign: 1)
        }
        .arrowCursorOnHover()
    }

    private var scrollBody: some View {
        let scroll = ScrollView {
            LazyVStack(spacing: cellGap) {
                ForEach(0..<max(model.thumbCount, 0), id: \.self) { i in
                    ThumbCell(
                        model: model,
                        index: i,
                        boxWidth: boxWidth,
                        boxHeight: boxHeight,
                        innerPad: cellInnerPad,
                        labelGap: labelGap,
                        labelHeight: labelHeight,
                        isCurrent: i == model.thumbCurrent,
                        dirty: model.thumbDirty
                    )
                    .id(i)
                    .frame(height: cellHeight)
                    .onAppear {
                        realized.insert(i)
                        scheduleViewportReport()
                    }
                    .onDisappear {
                        realized.remove(i)
                        scheduleViewportReport()
                    }
                }
            }
            .padding(.vertical, 4)
        }
        .frame(height: max(160, maxHeight - chromeHeight))
        // User-scroll detection (macOS 15+): scroll movement while our own
        // follow animation is NOT in flight is the user grabbing the list →
        // detach auto-follow. On macOS 14 the strip simply always follows.
        if #available(macOS 15.0, *) {
            return AnyView(
                scroll.onScrollGeometryChange(for: CGFloat.self, of: { $0.contentOffset.y }) {
                    old, new in
                    if old != new && animatingGen == 0 {
                        model.thumbsUserScrolled()
                    }
                })
        }
        return AnyView(scroll)
    }

    /// Report the realized-cell span (visible + SwiftUI's render-ahead) as the
    /// demand viewport, debounced so a fling doesn't spam FFI per cell.
    private func scheduleViewportReport() {
        reportTask?.cancel()
        reportTask = Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(120))
            guard !Task.isCancelled, let lo = realized.min(), let hi = realized.max() else {
                return
            }
            let over = visibleRows * 2
            model.thumbsSetViewport(lo, hi, max(0, lo - over), hi + over)
        }
    }

    /// Land a pending follow-scroll: smooth for short moves (≤ ~2 viewports),
    /// hard snap beyond (long jumps, wraps — animating through thousands of
    /// cells would be absurd). Reports the generation back when it lands so
    /// the core knows our animation ended (and stale ones are ignored).
    private func followScroll(_ proxy: ScrollViewProxy) {
        let item = model.thumbScrollItem
        let gen = model.thumbScrollGen
        guard item >= 0, gen != 0, gen != animatingGen else { return }
        animatingGen = gen
        let near = lastCentered >= 0 && abs(item - lastCentered) <= visibleRows * 2
        lastCentered = item
        if near {
            withAnimation(.easeOut(duration: 0.14)) {
                proxy.scrollTo(item, anchor: .center)
            } completion: {
                model.thumbsScrollDone(gen)
                if animatingGen == gen { animatingGen = 0 }
            }
        } else {
            proxy.scrollTo(item, anchor: .center)
            model.thumbsScrollDone(gen)
            if animatingGen == gen { animatingGen = 0 }
        }
    }
}

/// One fixed-size strip cell: the thumb fit-within a letterbox box, a type
/// badge, the session rotation, the current-item highlight, a hover affordance,
/// and the middle-truncated filename. `dirty` is observed so a landing thumb
/// re-renders the cell (the image pull itself is cached per generation).
struct ThumbCell: View {
    let model: CoreModel
    let index: Int
    let boxWidth: CGFloat
    let boxHeight: CGFloat
    let innerPad: CGFloat
    let labelGap: CGFloat
    let labelHeight: CGFloat
    let isCurrent: Bool
    let dirty: UInt64

    @State private var hovered = false

    var body: some View {
        VStack(spacing: labelGap) {
            ZStack {
                cellImage
                badge
            }
            .frame(width: boxWidth, height: boxHeight)
            .clipShape(RoundedRectangle(cornerRadius: 5))

            // An exact-height, centered label band: the gap above the label
            // (the VStack spacing) and below it (the cell padding) both equal
            // the top-of-cell gap, so the filename sits visually centered.
            Text(model.thumbName(index))
                .font(.caption)
                .lineLimit(1)
                .truncationMode(.middle)
                .foregroundStyle(isCurrent ? Color.primary : Color.panelSecondary)
                .frame(width: boxWidth, height: labelHeight)
        }
        .padding(.top, innerPad)
        .padding(.horizontal, innerPad)
        .padding(.bottom, labelGap)
        // ONE background for the whole cell (owner polish #1): a subtle
        // translucent card with a hairline border; the current item tints the
        // same card accent instead of stacking a second box.
        .background(
            RoundedRectangle(cornerRadius: 8)
                .fill(
                    isCurrent
                        ? AnyShapeStyle(Color.accentColor.opacity(0.24))
                        : hovered
                            ? AnyShapeStyle(Color.primary.opacity(0.09))
                            : AnyShapeStyle(.quaternary.opacity(0.45)))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 8)
                .strokeBorder(
                    isCurrent ? Color.accentColor.opacity(0.8) : Color.primary.opacity(0.08),
                    lineWidth: isCurrent ? 1.5 : 1)
        )
        .contentShape(Rectangle())
        .onHover { hovered = $0 }
        .onTapGesture { model.thumbClick(index) }
        .help(model.thumbName(index))
    }

    @ViewBuilder
    private var cellImage: some View {
        // `dirty` participates so a freshly landed thumb invalidates this view.
        let _ = dirty
        // An archive door has no thumbnail to decode — its frame is a 1×1 transparent
        // sentinel (task #105), so the pixel path below would draw an empty cell. Draw the
        // same artwork the door card uses, from the one image cached for the process. The
        // archive itself is never touched: the strip has no more right to decompress one
        // than the prefetch ring does.
        if model.thumbArchive(index), let art = model.doorArtwork() {
            Image(nsImage: art)
                .resizable()
                .interpolation(.high)
                .aspectRatio(contentMode: .fit)
                .frame(width: boxWidth, height: boxHeight)
        } else if let image = model.thumbImage(index) {
            let quarter = model.thumbRotation(index)
            Image(nsImage: image)
                .resizable()
                .interpolation(.high)
                .aspectRatio(contentMode: .fit)
                // Session rotation at draw: swap the fitting box for odd quarter
                // turns so the rotated image still letterboxes inside the cell.
                .frame(
                    width: quarter % 2 == 1 ? boxHeight : boxWidth,
                    height: quarter % 2 == 1 ? boxWidth : boxHeight
                )
                .rotationEffect(.degrees(Double(quarter) * 90))
                .frame(width: boxWidth, height: boxHeight)
        } else if model.thumbFailed(index) {
            Image(systemName: "photo.badge.exclamationmark")
                .font(.title2)
                .foregroundStyle(Color.panelSecondary)
        } else {
            Image(systemName: "photo")
                .font(.title3)
                .foregroundStyle(.quaternary)
        }
    }

    @ViewBuilder
    private var badge: some View {
        let kind = model.thumbBadge(index)
        if kind != 0 {
            VStack {
                Spacer()
                HStack {
                    Image(systemName: badgeIcon(kind))
                        .font(.caption2)
                        .foregroundStyle(.white)
                        .padding(4)
                        .background(.black.opacity(0.45), in: Circle())
                        .padding(4)
                    Spacer()
                }
            }
        }
    }

    private func badgeIcon(_ kind: Int) -> String {
        switch kind {
        case 1: return "play.fill"
        case 2: return "livephoto"
        default: return InfoLineView.animationMark
        }
    }
}
