//! macOS: chromeless borderless fullscreen.
//!
//! The borderless "speed mode" (F / ⌥⏎) sizes a decoration-less window to the whole
//! display while staying in the **current Space** (no native-fullscreen swoosh). But the
//! system menu bar floats on top, eating the top strip and obscuring the photo's top edge.
//! Setting `NSApplicationPresentationOptions` to auto-hide the menu bar + Dock reclaims
//! that real estate — each slides back into view on hover at the screen edge, so the menu
//! stays reachable — without leaving the Space. Restored to the default when we return to
//! windowed mode. (Native ⌃⌘F fullscreen manages this itself; we only touch it for the
//! borderless mode.)

use objc2::runtime::AnyObject;
use objc2::{class, msg_send};

/// `NSApplicationPresentationAutoHideDock` (1<<0) | `NSApplicationPresentationAutoHideMenuBar`
/// (1<<2). The menu-bar bit is only valid combined with a Dock bit, which this satisfies.
const AUTO_HIDE: usize = 0b101;
/// `NSApplicationPresentationDefault` — the normal menu bar + Dock.
const DEFAULT: usize = 0;

/// Auto-hide the menu bar + Dock (`on`) for chromeless borderless fullscreen, or restore
/// them (`!on`). App-global (not per-window), so it survives moving the window between
/// displays. Must run on the main thread.
pub fn set_chromeless(on: bool) {
    // SAFETY: main-thread AppKit; `setPresentationOptions:` takes an NSUInteger bitmask,
    // and `AUTO_HIDE` is a documented-valid combination (auto-hide menu bar + dock).
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let options: usize = if on { AUTO_HIDE } else { DEFAULT };
        let _: () = msg_send![app, setPresentationOptions: options];
    }
}
