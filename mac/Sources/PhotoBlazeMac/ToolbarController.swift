import AppKit
import PbMacFfi

/// The window toolbar (task #55) — the mouse-driven discoverability layer over the
/// keyboard-first core. It is the AppKit sibling of `MenuBar.swift`: every button fires a
/// stable Action id over the same `CoreModel.menuAction(_:)` path a keypress or a menu item
/// uses, and `sync(_:treeVisible:)` reflects the live `MenuState` (selected scale, panel /
/// slideshow on-state, enable flags) exactly like the menu bar's `sync(_:)`. No new core
/// state, no new orchestration seam — this is pure presenter chrome (task #55, item 5).
///
/// **Why an `NSToolbar`, not a floating overlay strip or a SwiftUI `.toolbar`:**
/// - A real toolbar lives in the titlebar, so in windowed mode it costs *zero* extra
///   fit-to-screen image area beyond the titlebar the window already has — the
///   prime-directive-safe answer the task's "never eats the image" constraint wanted.
/// - `View ▸ Hide Toolbar` (⌥⌘T) + the Customize sheet are native AppKit behaviors, so the
///   "hideable by power users / customizable" requirement is ~free (see `MenuBar.build`).
/// - In native fullscreen the OS auto-hides the toolbar and reveals it on mouse-to-top; in
///   the borderless F speed mode the titlebar (and thus the toolbar) is gone entirely — so
///   the chrome evaporates the moment you go flick-fast, and never touches the keypress→
///   photon path.
/// - Staying in AppKit (like the menu bar and the window-chrome management) avoids adding a
///   second SwiftUI-owns-the-titlebar fight on top of the menu-clobber one.
///
/// ⚠ SwiftUI owns the `WindowGroup` window's titlebar surface and can replace `window.toolbar`
/// on its scene-update passes (the same class of clobber `MenuBar.reassert` handles). So the
/// install is re-asserted from `CoreModel.assertWindowChrome` (compare-before-set, logged),
/// and needs an on-device smoke test to confirm it doesn't fight — the one seam here that a
/// headless build can't validate.
@MainActor
final class ToolbarController: NSObject, NSToolbarDelegate {
    private weak var model: CoreModel?

    /// The toolbar we own — re-asserted onto the window if SwiftUI swaps in its own.
    let toolbar: NSToolbar

    /// Every button/segment fires a stable Action id — the same vocabulary the menu bar and
    /// keymap share (`crates/pb-app-core/src/action.rs`). Looked up by toolbar-item id.
    private var actionForItem: [NSToolbarItem.Identifier: String] = [:]

    /// Plain (bordered) action items, kept so `sync` can flip their `isEnabled` (menu parity:
    /// Compare / Show in Finder / Delete are context-gated).
    private var plainItems: [NSToolbarItem.Identifier: NSToolbarItem] = [:]

    /// The toggle buttons (Inspector / Folder tree / Slideshow), kept so `sync` can show
    /// their live on-state (recessed + accent tint) from `MenuState`.
    private var toggleButtons: [NSToolbarItem.Identifier: NSButton] = [:]

    /// The scale segmented control, kept so `sync` can select Fit/Fill/1:1 from `MenuState.scale`.
    private var scaleSegment: NSSegmentedControl?

    init(model: CoreModel) {
        self.model = model
        self.toolbar = NSToolbar(identifier: "PhotoBlazeToolbar")
        super.init()
        toolbar.delegate = self
        toolbar.allowsUserCustomization = true // the Customize sheet (drag items in/out)
        toolbar.autosavesConfiguration = true // remembers the user's set + Hide Toolbar choice
        toolbar.displayMode = .iconOnly
    }

    /// Attach to the window. `.unifiedCompact` merges the toolbar into the titlebar (the
    /// Preview-on-Tahoe look: title + "N of M" subtitle leading, tool clusters trailing) and
    /// keeps the bar short so it eats the least image area.
    func install(on window: NSWindow) {
        window.toolbar = toolbar
        window.toolbarStyle = .unifiedCompact
    }

    /// Heal a SwiftUI clobber: if the window is showing a different toolbar object (SwiftUI
    /// swapped its own in during a scene update), put ours back. Never touches `isVisible` —
    /// the user's `Hide Toolbar` choice keeps *our* object (just hidden), so an identity check
    /// preserves it. Skipped in the borderless speed mode, which has no titlebar at all.
    func reassertIfClobbered(on window: NSWindow, speedMode: Bool) {
        guard !speedMode else { return }
        if window.toolbar !== toolbar {
            install(on: window)
        }
    }

    // MARK: - Live state (menu-bar parity)

    /// Apply a fresh `MenuState`: the selected scale segment, the panel/slideshow on-states,
    /// and the context enable flags — the toolbar twin of `MenuBar.sync(_:)`. `treeVisible`
    /// comes from the model (the folder tree isn't in `MenuState`; it's refreshed on
    /// `PanelsChanged`).
    func sync(_ s: MenuStateFfi, treeVisible: Bool) {
        scaleSegment?.selectedSegment = Int(s.scale)
        setToggle(.inspector, s.info_full)
        setToggle(.folderTree, treeVisible)
        setToggle(.slideshow, s.slideshow)
        plainItems[.compare]?.isEnabled = s.compare_toggle_enabled
        plainItems[.reveal]?.isEnabled = s.reveal_enabled
        plainItems[.delete]?.isEnabled = s.reveal_enabled // both need a real on-disk file
    }

    private func setToggle(_ id: NSToolbarItem.Identifier, _ on: Bool) {
        guard let button = toggleButtons[id] else { return }
        button.state = on ? .on : .off
        // Belt to the pushOnPushOff recess: an accent tint makes the active state
        // unmistakable over a busy photo, where the recess alone can read as subtle.
        button.contentTintColor = on ? .controlAccentColor : nil
    }

    // MARK: - Firing

    @objc private func itemFired(_ sender: NSToolbarItem) {
        guard let id = actionForItem[sender.itemIdentifier] else { return }
        model?.menuAction(id)
    }

    @objc private func toggleFired(_ sender: NSButton) {
        // The button's own pushOnPushOff flip is cosmetic; the core owns the real toggle and
        // the next `sync` sets the authoritative state. Fire by the id stashed on the button.
        guard let raw = sender.identifier?.rawValue else { return }
        model?.menuAction(raw)
    }

    @objc private func navFired(_ sender: NSSegmentedControl) {
        model?.menuAction(sender.selectedSegment == 0 ? "prev" : "next")
    }

    @objc private func scaleFired(_ sender: NSSegmentedControl) {
        switch sender.selectedSegment {
        case 0: model?.menuAction("scale_fit")
        case 1: model?.menuAction("scale_fill")
        default: model?.menuAction("scale_original")
        }
    }

    @objc private func zoomFired(_ sender: NSSegmentedControl) {
        model?.menuAction(sender.selectedSegment == 0 ? "zoom_out" : "zoom_in")
    }

    // MARK: - NSToolbarDelegate

    /// The out-of-the-box set: navigation + random leading (paired with the title/counter),
    /// then a flexible gap, then the view/panel cluster trailing. Deliberately sparse — the
    /// menu bar carries the full 50-action vocabulary; the Customize sheet holds the long tail.
    func toolbarDefaultItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [
            .navigation, .random,
            .flexibleSpace,
            .scale, .rotateLeft, .inspector, .folderTree, .slideshow,
        ]
    }

    /// Everything the user may drag into the bar (the Customize palette). Order = palette order.
    func toolbarAllowedItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [
            .navigation, .random,
            .scale, .rotateLeft, .rotateRight, .zoom,
            .inspector, .folderTree, .slideshow, .playPause, .compare,
            .copy, .copyPath, .reveal, .delete, .describe,
            .openFile, .openFolder, .settings, .fullscreen,
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
            return segmented(
                id, symbols: ["chevron.left", "chevron.right"], mode: .momentary,
                action: #selector(navFired(_:)),
                label: "Navigate", tips: "Previous / Next"
            )
        case .scale:
            let item = segmentedLabels(
                id, labels: ["Fit", "Fill", "1:1"], mode: .selectOne,
                action: #selector(scaleFired(_:)),
                label: "Scale", tips: "Fit / Fill / Actual size"
            )
            scaleSegment = item.view as? NSSegmentedControl
            return item
        case .zoom:
            return segmented(
                id, symbols: ["minus.magnifyingglass", "plus.magnifyingglass"], mode: .momentary,
                action: #selector(zoomFired(_:)),
                label: "Zoom", tips: "Zoom out / in"
            )
        case .inspector:
            return toggle(id, action: "full_exif", symbol: "info.circle", label: "Info")
        case .folderTree:
            return toggle(id, action: "folder_tree", symbol: "sidebar.left", label: "Folders")
        case .slideshow:
            return toggle(id, action: "slideshow", symbol: "play.rectangle", label: "Slideshow")

        case .random:
            return button(id, action: "random", symbol: "shuffle", label: "Random")
        case .rotateLeft:
            return button(id, action: "rotate_ccw", symbol: "rotate.left", label: "Rotate Left")
        case .rotateRight:
            return button(id, action: "rotate_cw", symbol: "rotate.right", label: "Rotate Right")
        case .playPause:
            return button(id, action: "play_pause", symbol: "play.fill", label: "Play")
        case .compare:
            return button(id, action: "compare_toggle", symbol: "rectangle.on.rectangle",
                          label: "Compare")
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
        case .openFile:
            return button(id, action: "open_file", symbol: "doc", label: "Open File")
        case .openFolder:
            return button(id, action: "open_folder", symbol: "folder.badge.plus",
                          label: "Open Folder")
        case .settings:
            return button(id, action: "settings", symbol: "gearshape", label: "Settings")
        case .fullscreen:
            return button(id, action: "fullscreen",
                          symbol: "arrow.up.left.and.arrow.down.right", label: "Fullscreen")
        default:
            return nil
        }
    }

    // MARK: - Item builders

    /// A plain bordered action button item — the standard toolbar look, native customization,
    /// explicit enable control (`autovalidates = false`, `sync` owns `isEnabled`).
    private func button(
        _ id: NSToolbarItem.Identifier, action: String, symbol: String, label: String
    ) -> NSToolbarItem {
        let item = NSToolbarItem(itemIdentifier: id)
        item.image = symbolImage(symbol, label)
        item.label = label
        item.paletteLabel = label
        item.toolTip = label
        item.target = self
        item.action = #selector(itemFired(_:))
        item.isBordered = true
        item.autovalidates = false
        actionForItem[id] = action
        plainItems[id] = item
        return item
    }

    /// A toggle item (Inspector / Folder tree / Slideshow): a `pushOnPushOff` button so the
    /// active panel shows the recessed toolbar-toggle look; `sync` sets the authoritative
    /// on-state from `MenuState` / the model's panel visibility.
    private func toggle(
        _ id: NSToolbarItem.Identifier, action: String, symbol: String, label: String
    ) -> NSToolbarItem {
        let b = NSButton(title: "", image: symbolImage(symbol, label) ?? NSImage(), target: self,
                         action: #selector(toggleFired(_:)))
        b.setButtonType(.pushOnPushOff)
        b.bezelStyle = .texturedRounded
        b.imagePosition = .imageOnly
        b.identifier = NSUserInterfaceItemIdentifier(action) // toggleFired reads this
        let item = NSToolbarItem(itemIdentifier: id)
        item.view = b
        item.label = label
        item.paletteLabel = label
        item.toolTip = label
        actionForItem[id] = action
        toggleButtons[id] = b
        return item
    }

    /// A segmented control (nav / zoom) wrapped in a toolbar item — renders as one grouped
    /// glass capsule on Tahoe.
    private func segmented(
        _ id: NSToolbarItem.Identifier, symbols: [String],
        mode: NSSegmentedControl.SwitchTracking, action: Selector, label: String, tips: String
    ) -> NSToolbarItem {
        let images = symbols.map { symbolImage($0, label) ?? NSImage() }
        let seg = NSSegmentedControl(images: images, trackingMode: mode, target: self, action: action)
        return wrap(seg, id: id, label: label, tips: tips)
    }

    private func segmentedLabels(
        _ id: NSToolbarItem.Identifier, labels: [String],
        mode: NSSegmentedControl.SwitchTracking, action: Selector, label: String, tips: String
    ) -> NSToolbarItem {
        let seg = NSSegmentedControl(labels: labels, trackingMode: mode, target: self, action: action)
        return wrap(seg, id: id, label: label, tips: tips)
    }

    private func wrap(
        _ seg: NSSegmentedControl, id: NSToolbarItem.Identifier, label: String, tips: String
    ) -> NSToolbarItem {
        seg.segmentStyle = .separated
        let item = NSToolbarItem(itemIdentifier: id)
        item.view = seg
        item.label = label
        item.paletteLabel = label
        item.toolTip = tips
        return item
    }

    /// An SF Symbol as a template toolbar image (tints with the accent / theme), with an
    /// accessibility description for VoiceOver.
    private func symbolImage(_ name: String, _ desc: String) -> NSImage? {
        NSImage(systemSymbolName: name, accessibilityDescription: desc)
    }
}

// The toolbar-item vocabulary — one identifier per affordance (raw values are the autosave
// keys, so keep them stable).
extension NSToolbarItem.Identifier {
    static let navigation = NSToolbarItem.Identifier("pb.navigation")
    static let random = NSToolbarItem.Identifier("pb.random")
    static let scale = NSToolbarItem.Identifier("pb.scale")
    static let zoom = NSToolbarItem.Identifier("pb.zoom")
    static let rotateLeft = NSToolbarItem.Identifier("pb.rotateLeft")
    static let rotateRight = NSToolbarItem.Identifier("pb.rotateRight")
    static let inspector = NSToolbarItem.Identifier("pb.inspector")
    static let folderTree = NSToolbarItem.Identifier("pb.folderTree")
    static let slideshow = NSToolbarItem.Identifier("pb.slideshow")
    static let playPause = NSToolbarItem.Identifier("pb.playPause")
    static let compare = NSToolbarItem.Identifier("pb.compare")
    static let copy = NSToolbarItem.Identifier("pb.copy")
    static let copyPath = NSToolbarItem.Identifier("pb.copyPath")
    static let reveal = NSToolbarItem.Identifier("pb.reveal")
    static let delete = NSToolbarItem.Identifier("pb.delete")
    static let describe = NSToolbarItem.Identifier("pb.describe")
    static let openFile = NSToolbarItem.Identifier("pb.openFile")
    static let openFolder = NSToolbarItem.Identifier("pb.openFolder")
    static let settings = NSToolbarItem.Identifier("pb.settings")
    static let fullscreen = NSToolbarItem.Identifier("pb.fullscreen")
}
