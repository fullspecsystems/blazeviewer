//! macOS: receive files opened from Finder (double-click), the Dock (drag-onto-icon), or
//! `open -a` — delivered to the app as `application:openURLs:`, which winit drops.
//!
//! winit's `WinitApplicationDelegate` doesn't implement `application:openURLs:`, and its
//! event loop runs off CFRunLoop observers (not the delegate) — so rather than replace the
//! delegate (which would break winit's lifecycle), we just **add** that one missing method
//! to winit's delegate class at runtime (`class_addMethod`). The added method copies the
//! file paths into a queue that the event loop drains each tick (`App::about_to_wait` →
//! `open_input`). Installed once in `main()` right after `EventLoop::new()` — winit sets
//! its delegate *there*, and the run loop (which dispatches a cold double-click's launch
//! `openURLs` right after `applicationDidFinishLaunching`) hasn't started yet, so even the
//! launch file is caught. Pairs with the `CFBundleDocumentTypes` in Info.plist.

use std::ffi::{c_char, CStr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};

use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{class, msg_send, sel};

/// Paths handed to us by `application:openURLs:`, awaiting the next event-loop drain.
static OPENED: LazyLock<Mutex<Vec<PathBuf>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// The `application:openURLs:` implementation grafted onto winit's app delegate. AppKit
/// calls it `(self, _cmd, NSApplication*, NSArray<NSURL>*)`; we copy out the file-URL paths
/// and queue them. Runs on the main thread.
extern "C" fn open_urls(
    _self: *mut AnyObject,
    _cmd: Sel,
    _app: *mut AnyObject,
    urls: *mut AnyObject,
) {
    if urls.is_null() {
        return;
    }
    let mut out = Vec::new();
    // SAFETY: `urls` is a live `NSArray<NSURL>` from AppKit; we send only standard,
    // type-correct messages and copy out UTF-8 path bytes (no ownership taken).
    unsafe {
        let count: usize = msg_send![urls, count];
        for i in 0..count {
            let url: *mut AnyObject = msg_send![urls, objectAtIndex: i];
            if url.is_null() {
                continue;
            }
            let path: *mut AnyObject = msg_send![url, path]; // NSString*, nil for non-file URLs
            if path.is_null() {
                continue;
            }
            let c: *const c_char = msg_send![path, UTF8String];
            if c.is_null() {
                continue;
            }
            out.push(PathBuf::from(
                CStr::from_ptr(c).to_string_lossy().into_owned(),
            ));
        }
    }
    if !out.is_empty() {
        if let Ok(mut q) = OPENED.lock() {
            q.extend(out);
        }
    }
}

/// Add `application:openURLs:` to winit's app-delegate class, once. No-op if the delegate
/// isn't set yet or already implements it (`class_addMethod` returns false and doesn't
/// clobber). Call from `main()` after `EventLoop::new()`, on the main thread.
pub fn install() {
    if INSTALLED.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: main-thread ObjC. We read NSApp's delegate (winit's singleton, which lacks
    // `openURLs`) and add one method to its class; the IMP has the matching ObjC ABI and
    // type encoding "v@:@@" (void; self, _cmd, NSApplication*, NSArray*).
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let delegate: *mut AnyObject = msg_send![app, delegate];
        if delegate.is_null() {
            return; // delegate not installed yet — retry on a later tick
        }
        let cls: *mut AnyClass = msg_send![delegate, class];
        let imp: objc2::runtime::Imp = std::mem::transmute(
            open_urls as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject),
        );
        objc2::ffi::class_addMethod(cls, sel!(application:openURLs:), imp, c"v@:@@".as_ptr());
        INSTALLED.store(true, Ordering::Relaxed);
    }
}

/// Drain and return any files opened via `application:openURLs:` since the last call.
pub fn take_opened() -> Vec<PathBuf> {
    OPENED
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default()
}
