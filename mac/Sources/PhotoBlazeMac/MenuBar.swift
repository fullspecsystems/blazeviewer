import AppKit
import PbMacFfi

/// The native menu bar (NS1 item 8) — the same structure and Action ids as the winit
/// shell's muda menu (`crates/pb-app/src/menu.rs`, the macOS `build_menu`): every item
/// fires a stable Action id over the FFI (the same dispatch path as the keyboard), and
/// `CoreEffect::SetMenuState` keeps the checks/enables in sync via `sync(_:)`.
///
/// ⌘-accelerators are real NSMenu key-equivalents (safe: an unbound ⌘-chord never falls
/// through to a bare key — the keymap's logo rule). Bare-key commands (Space, R, `,`/`.`,
/// …) show plain labels — the keymap owns those keys, and an NSMenu key-equivalent would
/// steal them (breaking hold-to-fly). macOS auto-injects its own "Enter Full Screen"
/// item at the end of View, so the native-fullscreen entry comes for free.
@MainActor
final class MenuBar: NSObject {
    private weak var model: CoreModel?
    private var items: [String: NSMenuItem] = [:]
    /// The menu we installed, retained so `reassert` can recognize a clobber.
    private var menu: NSMenu?
    /// A top-level item we created (the File holder) — the clobber canary. SwiftUI's
    /// scene updates gut the installed menu **in place** (verified with --pb-f-smoke:
    /// `NSApp.mainMenu` keeps its identity while our items vanish), so only a
    /// content check can detect it: if this item is no longer in our menu, SwiftUI
    /// rewrote the bar.
    private var sentinel: NSMenuItem?

    init(model: CoreModel) {
        self.model = model
        super.init()
        install(build())
    }

    /// The currently installed menu object — the model's clobber observer filters
    /// `NSMenu.didRemoveItemNotification` to exactly this instance.
    var currentMenu: NSMenu? { menu }

    /// Whether the main menu is still ours AND still intact (SwiftUI guts the object
    /// in place, so pointer identity alone lies).
    var isInstalled: Bool {
        guard let menu, NSApp.mainMenu === menu else { return false }
        return sentinel?.menu === menu
    }

    private func install(_ menu: NSMenu) {
        self.menu = menu
        // Not items[0]: SwiftUI's default bar also starts with an app menu, so the
        // File holder is the item its rewrite can never legitimately preserve.
        sentinel = menu.items.count > 1 ? menu.items[1] : menu.items.first
        NSApp.mainMenu = menu
        refreshShortcutBadges()
    }

    /// Rebuild + re-install our menu if SwiftUI rewrote the bar. Its scene updates
    /// (the F-mode toggle was the repro — the observable `speedModeFullscreen` flip
    /// re-evaluates the window scene) remove our items from the installed menu IN
    /// PLACE and insert its default set, which also orphans our item tree — so a
    /// clobber means rebuilding from scratch, not re-pointing `NSApp.mainMenu`.
    /// Intact = no-op, so this is cheap to call often. Callers must guard against
    /// open menus (`menuTrackingDepth`) — swapping the main menu mid-tracking snaps
    /// them shut. Returns whether a re-install happened (the model re-syncs state).
    @discardableResult
    func reassert() -> Bool {
        guard !isInstalled else { return false }
        install(build())
        return true
    }

    /// Right-aligned shortcut hints for the bare-key commands (Space, R, `[`, …) —
    /// egui-menu parity, where every item shows its key. These keys belong to the
    /// viewer's keymap, so they must NOT be NSMenu key-equivalents (the menu would
    /// steal them from hold-to-fly, and would fire while another window is key); an
    /// `NSMenuItemBadge` renders the hint without registering anything. Reads the
    /// LIVE keymap's primary slot (the same display the Shortcuts editor shows), so a
    /// rebind updates the menu bar too — called again after a Settings save.
    func refreshShortcutBadges() {
        guard let model else { return }
        for (id, item) in items {
            guard item.keyEquivalent.isEmpty else { continue } // ⌘-items keep the native column
            let hint = model.keymapSlotDisplay(id: id, slot: 0)
            item.badge = hint.isEmpty ? nil : NSMenuItemBadge(string: hint)
        }
    }

    @objc private func fire(_ sender: NSMenuItem) {
        guard let id = sender.representedObject as? String else { return }
        model?.menuAction(id)
    }

    /// Apply a fresh `SetMenuState`: check marks, enabled flags, the Undo label.
    func sync(_ s: MenuStateFfi) {
        let check = { (id: String, on: Bool) in
            self.items[id]?.state = on ? .on : .off
        }
        check("scale_fit", s.scale == 0)
        check("scale_fill", s.scale == 1)
        check("scale_original", s.scale == 2)
        check("info", s.info == 1)
        check("full_exif", s.info == 2)
        check("recursive", s.recursive)
        check("fullscreen", s.fullscreen)
        check("slideshow", s.slideshow)
        check("mute_live_audio", s.mute_live_audio)
        check("compare_pin", s.compare_pinned_here)
        items["compare_pin"]?.isEnabled = s.compare_pin_enabled
        items["compare_toggle"]?.isEnabled = s.compare_toggle_enabled
        items["save_rotation"]?.isEnabled = s.save_rotation_enabled
        items["reveal"]?.isEnabled = s.reveal_enabled
        items["cancel_scan"]?.isEnabled = s.cancel_scan_enabled
        items["undo"]?.isEnabled = s.undo_enabled
        items["undo"]?.title = s.undo_label.toString()
    }

    // MARK: - Construction

    private func item(
        _ id: String,
        _ title: String,
        key: String = "",
        mods: NSEvent.ModifierFlags = [.command],
        enabled: Bool = true
    ) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: #selector(fire(_:)), keyEquivalent: key)
        item.keyEquivalentModifierMask = key.isEmpty ? [] : mods
        item.target = self
        item.representedObject = id
        item.isEnabled = enabled
        items[id] = item
        return item
    }

    private func submenu(_ title: String, _ children: [NSMenuItem]) -> NSMenuItem {
        let menu = NSMenu(title: title)
        menu.autoenablesItems = false // SetMenuState owns the enabled flags
        children.forEach(menu.addItem)
        let holder = NSMenuItem()
        holder.submenu = menu
        return holder
    }

    private func system(_ title: String, _ action: Selector, key: String = "") -> NSMenuItem {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: key)
        item.target = nil // first responder / NSApp
        return item
    }

    private func build() -> NSMenu {
        let main = NSMenu()
        let sep = { NSMenuItem.separator() }
        let backspace = "\u{8}" // renders as the ⌫ glyph in the key-equivalent column

        // 1) The application menu: About / Settings / Quit per convention. Quit routes
        //    through our Action id (→ clean Esc-style teardown, privacy #6), not
        //    terminate: directly.
        main.addItem(submenu("PhotoBlaze", [
            item("about", "About PhotoBlaze"),
            sep(),
            item("settings", "Settings…", key: ","),
            sep(),
            system("Hide PhotoBlaze", #selector(NSApplication.hide(_:)), key: "h"),
            {
                let hideOthers = system(
                    "Hide Others",
                    #selector(NSApplication.hideOtherApplications(_:)),
                    key: "h"
                )
                hideOthers.keyEquivalentModifierMask = [.command, .option]
                return hideOthers
            }(),
            system("Show All", #selector(NSApplication.unhideAllApplications(_:))),
            sep(),
            item("quit", "Quit PhotoBlaze", key: "q"),
        ]))

        main.addItem(submenu("File", [
            item("open_file", "Open File…", key: "o"),
            item("open_folder", "Open Folder…", key: "O", mods: [.command, .shift]),
            item("cancel_scan", "Stop Scanning", enabled: false),
            sep(),
            // Standard ⌘W: closes the key window (Settings, About, the viewer). Routed
            // through the responder chain via performClose:, so it targets whichever
            // window is key — the custom NSMenu replaced SwiftUI's default File menu,
            // which had supplied this for free (its absence is why ⌘W stopped closing
            // Settings). Placed after the Open group per convention (Preview, TextEdit).
            system("Close Window", #selector(NSWindow.performClose(_:)), key: "w"),
            sep(),
            item("save_rotation", "Save Rotation", key: "s", enabled: false),
            item("reveal", "Show in Finder", enabled: false),
            sep(),
            // Finder idioms: Move to Trash = ⌘⌫, Delete Immediately = ⌥⌘⌫ (⇧⌘⌫ is
            // Finder's Empty Trash). ⌘-chords, so no double-fire vs the bare Del keys.
            item("delete", "Move to Trash", key: backspace),
            item("delete_permanent", "Delete Immediately…", key: backspace, mods: [.command, .option]),
        ]))

        main.addItem(submenu("Edit", [
            item("undo", "Undo", key: "z", enabled: false),
            sep(),
            item("copy", "Copy", key: "c"),
            item("copy_path", "Copy File Path", key: "C", mods: [.command, .shift]),
            // Owner request: also reachable here, not just the photo context menu
            // (copies filename/dimensions/codec/size + all non-blob EXIF as text).
            item("copy_image_details", "Copy Image Details"),
        ]))

        main.addItem(submenu("View", [
            item("scale_fit", "Fit"),
            item("scale_fill", "Crop to Fill"),
            item("scale_original", "Original 1:1"),
            sep(),
            item("zoom_in", "Zoom In"),
            item("zoom_out", "Zoom Out"),
            sep(),
            item("fullscreen", "Fullscreen"),
            // Native (Spaces) fullscreen — the ⌃⌘F / green-button behavior, a deliberate
            // alternative to the borderless speed mode above (winit-menu parity). AppKit
            // manages the Enter/Exit title itself for toggleFullScreen: items, and
            // providing one stops it auto-injecting a duplicate at the end of View.
            {
                let fs = system(
                    "Enter Full Screen", #selector(NSWindow.toggleFullScreen(_:)), key: "f"
                )
                fs.keyEquivalentModifierMask = [.command, .control]
                return fs
            }(),
            item("recursive", "Recursive (This Folder)"),
            item("slideshow", "Slideshow"),
            item("slideshow_faster", "Slideshow Faster"),
            item("slideshow_slower", "Slideshow Slower"),
            sep(),
            item("info", "Show Image Info"),
            item("full_exif", "Show All EXIF Info"),
            item("folder_tree", "Show Folder Tree"),
        ]))

        // Go — folder navigation (Finder's chords: ⌘↑ Enclosing Folder; ⌘←/⌘→ step
        // between sibling folders — PhotoBlaze has no back/forward history to shadow).
        main.addItem(submenu("Go", [
            {
                let up = item("open_parent", "Enclosing Folder", key: "\u{F700}")
                up.keyEquivalentModifierMask = [.command]
                return up
            }(),
            sep(),
            {
                let prev = item("prev_folder", "Previous Folder", key: "\u{F702}")
                prev.keyEquivalentModifierMask = [.command]
                return prev
            }(),
            {
                let next = item("next_folder", "Next Folder", key: "\u{F703}")
                next.keyEquivalentModifierMask = [.command]
                return next
            }(),
        ]))

        main.addItem(submenu("Image", [
            item("next", "Next"),
            item("prev", "Previous"),
            item("random", "Random"),
            item("random_prev", "Previous Random"),
            sep(),
            item("rotate_cw", "Rotate Right"),
            item("rotate_ccw", "Rotate Left"),
            sep(),
            // Flicker compare (task #43): bare keys (Y / ⇧Y) — badges, never
            // key-equivalents. SetMenuState drives enabled + the pin's checkmark.
            item("compare_pin", "Pin for Compare", enabled: false),
            item("compare_toggle", "Compare with Pinned", enabled: false),
            sep(),
            item("play_pause", "Play/Pause Animation"),
            item("frame_next", "Next Frame"),
            item("frame_prev", "Previous Frame"),
            sep(),
            item("mute_live_audio", "Mute Live Photo Audio"),
        ]))

        // The standard Window menu: native selectors give Minimize ⌘M / Zoom / Bring All
        // to Front for free; registering it as the windows menu appends the live window list.
        let window = submenu("Window", [
            system("Minimize", #selector(NSWindow.performMiniaturize(_:)), key: "m"),
            system("Zoom", #selector(NSWindow.performZoom(_:))),
            sep(),
            system("Bring All to Front", #selector(NSApplication.arrangeInFront(_:))),
        ])
        main.addItem(window)
        NSApp.windowsMenu = window.submenu

        let help = submenu("Help", [
            item("help", "Keyboard Shortcuts"),
        ])
        main.addItem(help)
        NSApp.helpMenu = help.submenu

        return main
    }
}
