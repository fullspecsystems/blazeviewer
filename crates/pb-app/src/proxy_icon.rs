//! macOS: the title-bar **proxy icon** (the window's "represented file").
//!
//! Setting an `NSWindow`'s `representedURL` to the current photo's file makes AppKit
//! draw that file's Finder icon next to the title, enables ⌘-click → enclosing-folder
//! popup, and lets the user drag the icon into Finder / Mail / Messages to move or copy
//! the file. winit doesn't expose it, so — exactly as [`crate::hdr_surface`] reaches the
//! `CAMetalLayer` — we reach the `NSWindow` via the view handle and set the property.
//!
//! Only meaningful in windowed mode (a decorated, titled window); in the borderless
//! speed mode there's no title bar, so the caller clears it. It's a live, RAM-only
//! window property (like the title itself), never persisted — and we deliberately never
//! call `noteNewRecentDocumentURL:`, so the no-trace guarantee (privacy #2) is unaffected.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

/// The live NSView pointer for the window, or `None`.
fn ns_view_of(window: &Window) -> Option<*mut AnyObject> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(h) = handle.as_raw() else {
        return None;
    };
    Some(h.ns_view.as_ptr() as *mut AnyObject)
}

/// Set (or clear) the window's represented file — the draggable title-bar proxy icon.
/// `Some(path)` shows that file's icon; `None` clears it (an archive entry or the empty
/// state has no on-disk file). Idempotent; must run on the main (event-loop) thread.
pub fn set_represented_url(window: &Window, path: Option<&Path>) {
    let Some(ns_view) = ns_view_of(window) else {
        return;
    };
    // SAFETY: a live NSView from the window handle; `[NSView window]` is its NSWindow
    // (nil only when off-screen), and `setRepresentedURL:` takes an NSURL or nil. All on
    // the main thread. The NSString/NSURL are autoreleased — fine inside winit's
    // per-iteration autorelease pool (`setRepresentedURL:` retains what it keeps).
    unsafe {
        let win: *mut AnyObject = msg_send![ns_view, window];
        if win.is_null() {
            return;
        }
        let url: *mut AnyObject = match path {
            // A path that can't be made into a URL would only happen on an interior NUL
            // (impossible for a real filesystem path) — leave the proxy as-is rather
            // than wiping a valid one.
            Some(p) => match path_to_nsurl(p) {
                Some(u) => u,
                None => return,
            },
            None => std::ptr::null_mut(), // genuinely no file → clear the proxy
        };
        let _: () = msg_send![win, setRepresentedURL: url];
    }
}

/// Build an autoreleased file `NSURL` for `path`, or `None` if it can't be expressed as
/// a C string (an interior NUL — impossible for a real path).
///
/// SAFETY: must run on the main thread (creates autoreleased Foundation objects).
unsafe fn path_to_nsurl(path: &Path) -> Option<*mut AnyObject> {
    let cstr = CString::new(path.as_os_str().as_bytes()).ok()?;
    let ns_string: *mut AnyObject =
        msg_send![class!(NSString), stringWithUTF8String: cstr.as_ptr()];
    if ns_string.is_null() {
        return None;
    }
    let url: *mut AnyObject = msg_send![class!(NSURL), fileURLWithPath: ns_string];
    (!url.is_null()).then_some(url)
}
