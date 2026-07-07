import AppKit
import PbMacFfi

/// The window toolbar (task #55) — the mouse-driven discoverability layer over the
/// keyboard-first core. AppKit sibling of `MenuBar.swift`: every control fires a stable
/// Action id through `CoreModel.menuAction(_:)` (the same path a keypress/menu item uses),
/// and `sync(...)` mirrors the live `MenuState` onto it.
///
/// **Rendering.** Momentary/selection clusters (nav, random, rotate, zoom, scale) are native
/// `NSToolbarItemGroup`s — the system draws them as glass capsules, correctly sized, no focus
/// ring. The **toggles** (Folders / Info / Inspector / Slideshow) are `pushOnPushOff` buttons
/// so their `.on` state shows the accent-tinted background macOS uses for active toolbar
/// controls (a plain toolbar item has no on-state). Slideshow additionally carries the live
/// interval text ("4s"), so it's a custom view either way.
///
/// **Overflow priority.** When the window is too narrow AppKit collapses items into the `»`
/// menu, lowest `visibilityPriority` first. The tiers encode the owner's essential-vs-rare
/// intuition: nav + info never drop (`.high`); rotate + fullscreen drop first (`.low`); power
/// features (save, pin, compare, copy, reveal, delete, describe, open, settings, parent) live
/// only in the Customize palette.
@MainActor
final class ToolbarController: NSObject, NSToolbarDelegate {
    private weak var model: CoreModel?

    /// The toolbar we own — re-asserted onto the window if SwiftUI swaps its own in.
    let toolbar: NSToolbar

    /// Item identifier → its Action id, for click dispatch. Identifier-keyed (not instance-
    /// keyed), so it's safe against the palette copies `willBeInsertedIntoToolbar: false`
    /// spawns. `sync` reaches live state by walking `toolbar.items` — never a cached instance,
    /// because opening Customize builds duplicate item/button instances and a cached reference
    /// would point at the wrong (palette/stale) one (the "slideshow time freezes in Customize"
    /// bug).
    private var actionForItem: [NSToolbarItem.Identifier: String] = [:]

    /// SF Symbols at a fixed moderate size — the toolbar-standard weight. Applied to every
    /// glyph via `symbol(_:)`. (v2 used `.large` scale, which made wide glyphs like `shuffle`
    /// look oversized next to thin chevrons; a fixed point size evens them out.)
    private static let symbolConfig = NSImage.SymbolConfiguration(pointSize: 15, weight: .regular)

    init(model: CoreModel) {
        self.model = model
        self.toolbar = NSToolbar(identifier: "PhotoBlazeToolbar")
        super.init()
        toolbar.delegate = self
        toolbar.allowsUserCustomization = true // the Customize sheet (drag items in/out)
        toolbar.autosavesConfiguration = true // remembers the user's set + Hide Toolbar choice
        toolbar.displayMode = .iconOnly
    }

    /// Attach to the window. `.unified` is the taller, more visible bar; the window title +
    /// "N of M" subtitle sit leading (`CoreModel.applyWindowTitle`), the centered nav cluster
    /// follows.
    func install(on window: NSWindow) {
        window.toolbar = toolbar
        window.toolbarStyle = .unified
    }

    /// Heal a SwiftUI clobber (identity check preserves the user's Hide-Toolbar choice, which
    /// keeps *our* object, just hidden). Skipped in the borderless speed mode (no titlebar).
    func reassertIfClobbered(on window: NSWindow, speedMode: Bool) {
        guard !speedMode else { return }
        if window.toolbar !== toolbar {
            install(on: window)
        }
    }

    // MARK: - Live state (menu-bar parity)

    /// Mirror the live state onto whatever items are **currently in the toolbar** (walking
    /// `toolbar.items`, not a cache): the selected scale segment, the toggle on-states (accent
    /// background), the slideshow glyph + interval, and the palette items' context enables.
    /// `treeVisible` and `slideshowInterval` live outside `MenuState`, so `CoreModel` passes
    /// them in.
    func sync(
        _ s: MenuStateFfi, treeVisible: Bool, slideshowInterval: String,
        hasMotion: Bool, playing: Bool
    ) {
        let toggleState = { (item: NSToolbarItem, on: Bool) in
            (item.view as? NSButton)?.state = on ? .on : .off
        }
        for item in toolbar.items {
            switch item.itemIdentifier {
            case .scale:
                (item as? NSToolbarItemGroup)?.selectedIndex = Int(s.scale)
            case .folderTree:
                toggleState(item, treeVisible)
            case .infoLine:
                toggleState(item, s.info_basic)
            case .inspector:
                toggleState(item, s.info_full)
            case .playAnimation:
                if let b = item.view as? NSButton {
                    b.isEnabled = hasMotion // dim on a still
                    b.state = playing ? .on : .off // light up while playing
                }
            case .slideshow:
                if let b = item.view as? NSButton {
                    b.state = s.slideshow ? .on : .off
                    b.title = "  \(slideshowInterval)"
                }
            case .saveRotation:
                item.isEnabled = s.save_rotation_enabled
            case .compare:
                item.isEnabled = s.compare_toggle_enabled
            case .pin:
                item.isEnabled = s.compare_pin_enabled
            case .reveal, .delete:
                item.isEnabled = s.reveal_enabled // both need a real on-disk file
            default:
                break
            }
        }
    }

    // MARK: - Firing

    @objc private func itemFired(_ sender: NSToolbarItem) {
        guard let id = actionForItem[sender.itemIdentifier] else { return }
        model?.menuAction(id)
    }

    /// Toggle buttons + slideshow carry their Action id on the button's `identifier` (the
    /// sender is the `NSButton`, not the toolbar item). The `pushOnPushOff` flip is cosmetic;
    /// the core owns the real toggle and the next `sync` sets the authoritative state.
    @objc private func toggleFired(_ sender: NSButton) {
        guard let id = sender.identifier?.rawValue else { return }
        model?.menuAction(id)
    }

    // Group actions fire with the group as sender; `selectedIndex` is the clicked segment.
    @objc private func navFired(_ g: NSToolbarItemGroup) {
        model?.menuAction(g.selectedIndex == 0 ? "prev" : "next")
    }

    @objc private func randomFired(_ g: NSToolbarItemGroup) {
        model?.menuAction(g.selectedIndex == 0 ? "random_prev" : "random")
    }

    @objc private func folderNavFired(_ g: NSToolbarItemGroup) {
        model?.menuAction(g.selectedIndex == 0 ? "prev_folder" : "next_folder")
    }

    @objc private func rotateFired(_ g: NSToolbarItemGroup) {
        model?.menuAction(g.selectedIndex == 0 ? "rotate_ccw" : "rotate_cw")
    }

    @objc private func zoomFired(_ g: NSToolbarItemGroup) {
        model?.menuAction(g.selectedIndex == 0 ? "zoom_out" : "zoom_in")
    }

    @objc private func scaleFired(_ g: NSToolbarItemGroup) {
        switch g.selectedIndex {
        case 0: model?.menuAction("scale_fit")
        case 1: model?.menuAction("scale_fill")
        default: model?.menuAction("scale_original")
        }
    }

    // MARK: - NSToolbarDelegate

    /// The default set: a leading gap centers the "moving through photos" cluster (nav ·
    /// random · folder-nav · slideshow) off the title; the rotate pair + panel toggles trail,
    /// and Fullscreen sits on its own past a fixed space (not clustered with the panels).
    func toolbarDefaultItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [
            .flexibleSpace,
            .navigation, .random, .folderNav, .slideshow, .playAnimation,
            .flexibleSpace,
            .rotate, .folderTree, .infoLine, .inspector,
            .space, .fullscreen,
        ]
    }

    func toolbarAllowedItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [
            .navigation, .random, .folderNav, .slideshow, .scale, .rotate, .saveRotation, .zoom,
            .folderTree, .infoLine, .inspector, .fullscreen, .playAnimation, .compare, .pin,
            .copy, .copyPath, .reveal, .delete, .describe, .showText,
            .openFile, .openFolder, .settings, .openParent,
            .space, .flexibleSpace,
        ]
    }

    func toolbar(
        _ toolbar: NSToolbar,
        itemForItemIdentifier id: NSToolbarItem.Identifier,
        willBeInsertedIntoToolbar flag: Bool
    ) -> NSToolbarItem? {
        switch id {
        case .navigation:
            return group(id, images: [symbol("chevron.left"), symbol("chevron.right")],
                         labels: ["Previous", "Next"], tips: ["Previous", "Next"],
                         mode: .momentary, action: #selector(navFired(_:)), priority: .high,
                         palette: "Navigate")
        case .random:
            // No mirrored-shuffle glyph exists in SF Symbols — flip `shuffle` horizontally so
            // the "previous random" segment pairs with it. BOTH segments go through the same
            // draw pipeline: mixing a raw symbol image (which the segmented control scales up,
            // → oversized) with a hand-drawn one (left at natural size, → smaller) made the
            // pair huge AND mismatched. Same pipeline → same size.
            //
            // Labels must stay SHORT. Counter-intuitively, a native image group in the unified
            // toolbar reserves each segment's width to fit its **label** even in `.iconOnly`
            // mode (where the label never renders). A long first label ("Previous Random", 15
            // chars) ballooned this group to 134pt vs nav's 81pt — nothing to do with the wide
            // `shuffle` glyph. "Random"/"Random" sizes it to ~78pt, matching the nav pair. The
            // per-segment tooltips ("Previous random" / "Random") carry the direction for
            // VoiceOver without paying the label width.
            return group(id, images: [shuffleImage(flipped: true), shuffleImage(flipped: false)],
                         labels: ["Random", "Random"],
                         tips: ["Previous random", "Random"],
                         mode: .momentary, action: #selector(randomFired(_:)), palette: "Random")
        case .folderNav:
            // Previous / next FOLDER (the sibling-folder jump, `Alt+←`/`Alt+→`). Double
            // chevrons distinguish it from the single-chevron photo nav ("jump a whole group").
            // Labels stay SHORT ("Folder"/"Folder") for the same reason as .random — segment
            // width tracks the label even in `.iconOnly`; the tooltips carry the direction.
            return group(id, images: [symbol("chevron.left.2"), symbol("chevron.right.2")],
                         labels: ["Folder", "Folder"],
                         tips: ["Previous folder", "Next folder"],
                         mode: .momentary, action: #selector(folderNavFired(_:)), palette: "Folder Navigation")
        case .rotate:
            return group(id, images: [symbol("rotate.left"), symbol("rotate.right")],
                         labels: ["Rotate Left", "Rotate Right"],
                         tips: ["Rotate left", "Rotate right"],
                         mode: .momentary, action: #selector(rotateFired(_:)), priority: .low,
                         palette: "Rotate")
        case .zoom:
            return group(id, images: [symbol("minus.magnifyingglass"), symbol("plus.magnifyingglass")],
                         labels: ["Zoom Out", "Zoom In"], tips: ["Zoom out", "Zoom in"],
                         mode: .momentary, action: #selector(zoomFired(_:)), palette: "Zoom")
        case .scale:
            return titleGroup(id, titles: ["Fit", "Fill", "1:1"],
                              labels: ["Fit", "Crop to Fill", "Actual Size"],
                              tips: ["Fit to window", "Crop to fill", "Actual size (1:1)"],
                              action: #selector(scaleFired(_:)), palette: "Scale")

        case .slideshow:
            return slideshowControl(id)
        case .folderTree:
            return toggle(id, action: "folder_tree", symbol: "folder.circle", label: "Folders")
        case .infoLine:
            return toggle(id, action: "info", symbol: "info", label: "Image Info")
        case .inspector:
            return toggle(id, action: "full_exif", symbol: "info.circle", label: "Details",
                          priority: .high)

        case .fullscreen:
            // `.high`: Fullscreen is one of the most-used controls, so it should be among the
            // LAST items shed into the `»` overflow when the window narrows — never the first.
            // (AppKit still parks the `»` chevron to its right; a titlebar accessory could put
            // it to the left like Preview's search, but an accessory doesn't get the toolbar's
            // Liquid Glass, so the button looked bare — the glass look wins here.)
            return button(id, action: "fullscreen",
                          symbol: "arrow.up.left.and.arrow.down.right", label: "Fullscreen",
                          priority: .high)
        case .saveRotation:
            return button(id, action: "save_rotation", symbol: "square.and.arrow.down",
                          label: "Save Rotation")
        case .playAnimation:
            // A toggle so it lights (accent background) while playing; `sync` also dims it
            // (disabled) when the current image has no motion.
            return toggle(id, action: "play_pause", symbol: "livephoto.play", label: "Play Animation")
        case .compare:
            return button(id, action: "compare_toggle", symbol: "rectangle.on.rectangle",
                          label: "Compare")
        case .pin:
            return button(id, action: "compare_pin", symbol: "pin", label: "Pin")
        case .copy:
            return button(id, action: "copy", symbol: "doc.on.doc", label: "Copy")
        case .copyPath:
            return button(id, action: "copy_path", symbol: "doc.on.clipboard", label: "Copy Path")
        case .reveal:
            return button(id, action: "reveal", symbol: "folder", label: "Show in Finder")
        case .delete:
            return button(id, action: "delete", symbol: "trash", label: "Delete")
        case .describe:
            return button(id, action: "describe", symbol: "sparkles", label: "Describe")
        case .showText:
            return button(id, action: "show_text", symbol: "text.viewfinder", label: "Text")
        case .openFile:
            return button(id, action: "open_file", symbol: "doc", label: "Open File")
        case .openFolder:
            return button(id, action: "open_folder", symbol: "folder.badge.plus", label: "Open Folder")
        case .settings:
            return button(id, action: "settings", symbol: "gearshape", label: "Settings")
        case .openParent:
            return button(id, action: "open_parent", symbol: "arrow.up", label: "Enclosing Folder")
        default:
            return nil
        }
    }

    // MARK: - Builders

    /// A native image group (one glass segmented capsule). `tips` set **per-segment** tooltips
    /// — a group-level `toolTip` would show for every segment, which reads wrong.
    private func group(
        _ id: NSToolbarItem.Identifier, images: [NSImage], labels: [String], tips: [String],
        mode: NSToolbarItemGroup.SelectionMode, action: Selector,
        priority: NSToolbarItem.VisibilityPriority = .standard, palette: String
    ) -> NSToolbarItemGroup {
        let g = NSToolbarItemGroup(itemIdentifier: id, images: images, selectionMode: mode,
                                   labels: labels, target: self, action: action)
        g.visibilityPriority = priority
        // Always render the segmented capsule; never let AppKit collapse it into a pull-down
        // popup when the window narrows (`.automatic` does — a useless dropdown-of-buttons for a
        // momentary pair). `.expanded` keeps the buttons; when there's truly no room the whole
        // group drops into the `»` overflow instead.
        g.controlRepresentation = .expanded
        g.label = palette
        g.paletteLabel = palette
        for (i, tip) in tips.enumerated() where i < g.subitems.count {
            g.subitems[i].toolTip = tip
        }
        return g
    }

    /// A native title group (text segments — Fit/Fill/1:1).
    private func titleGroup(
        _ id: NSToolbarItem.Identifier, titles: [String], labels: [String], tips: [String],
        action: Selector, palette: String
    ) -> NSToolbarItemGroup {
        let g = NSToolbarItemGroup(itemIdentifier: id, titles: titles, selectionMode: .selectOne,
                                   labels: labels, target: self, action: action)
        g.controlRepresentation = .expanded // stay segmented; don't collapse to a pull-down
        g.label = palette
        g.paletteLabel = palette
        for (i, tip) in tips.enumerated() where i < g.subitems.count {
            g.subitems[i].toolTip = tip
        }
        return g
    }

    /// A plain bordered action button (glass) — native customization, explicit enable control.
    private func button(
        _ id: NSToolbarItem.Identifier, action: String, symbol name: String, label: String,
        priority: NSToolbarItem.VisibilityPriority = .standard
    ) -> NSToolbarItem {
        let item = NSToolbarItem(itemIdentifier: id)
        item.image = symbol(name)
        item.label = label
        item.paletteLabel = label
        item.toolTip = label
        item.target = self
        item.action = #selector(itemFired(_:))
        item.isBordered = true
        item.autovalidates = false
        item.visibilityPriority = priority
        actionForItem[id] = action
        return item
    }

    /// A toggle: a `pushOnPushOff` button whose `.on` state shows the accent-tinted background
    /// macOS uses for active toolbar controls (`sync` drives the state). View-based (the only
    /// way to get the on-state), sized to fit, focus ring off.
    private func toggle(
        _ id: NSToolbarItem.Identifier, action: String, symbol name: String, label: String,
        priority: NSToolbarItem.VisibilityPriority = .standard
    ) -> NSToolbarItem {
        let b = NSButton(image: symbol(name), target: self, action: #selector(toggleFired(_:)))
        b.setButtonType(.pushOnPushOff)
        b.bezelStyle = .texturedRounded
        b.imagePosition = .imageOnly
        b.focusRingType = .none
        b.identifier = NSUserInterfaceItemIdentifier(action)
        // Intrinsic sizing (the system measures the button) — the modern replacement for the
        // deprecated item.minSize/maxSize.
        let item = NSToolbarItem(itemIdentifier: id)
        item.view = b
        item.label = label
        item.paletteLabel = label
        item.toolTip = label
        item.visibilityPriority = priority
        return item
    }

    /// The slideshow control: a `pushOnPushOff` button (accent background while running) that
    /// also shows the live interval. Fixed width so the button doesn't jiggle as the interval
    /// changes with `[` / `]`. The one custom view (no native item shows a live value).
    private func slideshowControl(_ id: NSToolbarItem.Identifier) -> NSToolbarItem {
        let b = NSButton(title: "  4s", image: symbol("play.square.stack"), target: self,
                         action: #selector(toggleFired(_:)))
        b.setButtonType(.pushOnPushOff)
        b.bezelStyle = .texturedRounded
        b.imagePosition = .imageLeading
        b.focusRingType = .none
        b.identifier = NSUserInterfaceItemIdentifier("slideshow")
        // A fixed width (sized for the widest interval, "60.0s") so adjusting `[`/`]` doesn't
        // resize the button. A width constraint is the modern replacement for item.minSize/
        // maxSize; height stays intrinsic.
        b.translatesAutoresizingMaskIntoConstraints = false
        b.widthAnchor.constraint(equalToConstant: 78).isActive = true
        let item = NSToolbarItem(itemIdentifier: id)
        item.view = b
        item.label = "Slideshow"
        item.paletteLabel = "Slideshow"
        item.toolTip = "Start / stop slideshow (adjust with [ and ])"
        return item
    }

    /// An SF Symbol as a template toolbar image at the shared size.
    private func symbol(_ name: String) -> NSImage {
        (NSImage(systemSymbolName: name, accessibilityDescription: nil)?
            .withSymbolConfiguration(Self.symbolConfig)) ?? NSImage()
    }

    /// The `shuffle` glyph, optionally horizontally flipped, drawn into a layout-neutral
    /// template image. Both random segments use this (see `.random`) so they're the same image
    /// *type* and therefore the same size — the fix for the oversized/mismatched pair. Kept
    /// template so it still tints with the theme/accent.
    private func shuffleImage(flipped: Bool) -> NSImage {
        let base = symbol("shuffle")
        let out = NSImage(size: base.size, flipped: false) { rect in
            guard let ctx = NSGraphicsContext.current?.cgContext else { return false }
            if flipped {
                ctx.translateBy(x: rect.width, y: 0)
                ctx.scaleBy(x: -1, y: 1)
            }
            base.draw(in: rect)
            return true
        }
        out.isTemplate = true
        return out
    }
}

// The toolbar-item vocabulary — one identifier per affordance (raw values are the autosave
// keys, so keep them stable).
extension NSToolbarItem.Identifier {
    static let navigation = NSToolbarItem.Identifier("pb.navigation")
    static let random = NSToolbarItem.Identifier("pb.random")
    static let folderNav = NSToolbarItem.Identifier("pb.folderNav")
    static let slideshow = NSToolbarItem.Identifier("pb.slideshow")
    static let scale = NSToolbarItem.Identifier("pb.scale")
    static let rotate = NSToolbarItem.Identifier("pb.rotate")
    static let saveRotation = NSToolbarItem.Identifier("pb.saveRotation")
    static let zoom = NSToolbarItem.Identifier("pb.zoom")
    static let folderTree = NSToolbarItem.Identifier("pb.folderTree")
    static let infoLine = NSToolbarItem.Identifier("pb.infoLine")
    static let inspector = NSToolbarItem.Identifier("pb.inspector")
    static let fullscreen = NSToolbarItem.Identifier("pb.fullscreen")
    static let playAnimation = NSToolbarItem.Identifier("pb.playAnimation")
    static let compare = NSToolbarItem.Identifier("pb.compare")
    static let pin = NSToolbarItem.Identifier("pb.pin")
    static let copy = NSToolbarItem.Identifier("pb.copy")
    static let copyPath = NSToolbarItem.Identifier("pb.copyPath")
    static let reveal = NSToolbarItem.Identifier("pb.reveal")
    static let delete = NSToolbarItem.Identifier("pb.delete")
    static let describe = NSToolbarItem.Identifier("pb.describe")
    static let showText = NSToolbarItem.Identifier("pb.showText")
    static let openFile = NSToolbarItem.Identifier("pb.openFile")
    static let openFolder = NSToolbarItem.Identifier("pb.openFolder")
    static let settings = NSToolbarItem.Identifier("pb.settings")
    static let openParent = NSToolbarItem.Identifier("pb.openParent")
}
