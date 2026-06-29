//! macOS: configure the wgpu-created `CAMetalLayer` for wide-gamut / EDR output, and
//! report the **window's** display headroom for the renderer's highlight roll-off.
//!
//! wgpu picks the surface *format* (`Rgba16Float` when the display is wide-gamut/HDR),
//! but it does not set the layer's color space or request EDR. Unlike DXGI on Windows
//! — where a float flip-model swapchain is scRGB for free and DWM tone-maps — macOS
//! needs both done explicitly, **per the screen the window is actually on** (not
//! `NSScreen.mainScreen`, which in a multi-display setup may be a different, SDR panel).
//! So we reach the window's `CAMetalLayer` and `NSScreen` via the NSView and set the
//! `colorspace` to **extended-linear-sRGB** (scRGB — what the scene pass writes) and
//! `wantsExtendedDynamicRangeContent` when the window's screen has EDR. We also return
//! that screen's EDR headroom so the present pass can roll highlights off toward it
//! (macOS EDR hard-clips above the headroom).

use objc2::encode::{Encoding, RefEncode};
use objc2::msg_send;
use objc2::runtime::{AnyObject, Bool};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

/// Opaque `CGColorSpace`, typed so `*const CGColorSpace` carries the Objective-C
/// encoding `^{CGColorSpace=}` that `-[CAMetalLayer setColorspace:]` expects (objc2
/// verifies this in debug builds; a bare `*const c_void` (`^v`) panics there).
#[repr(C)]
struct CGColorSpace {
    _opaque: [u8; 0],
}
unsafe impl RefEncode for CGColorSpace {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGColorSpace", &[]));
}

type CFStringRef = *const std::ffi::c_void;
type CGColorSpaceRef = *const CGColorSpace;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    static kCGColorSpaceExtendedLinearSRGB: CFStringRef;
    fn CGColorSpaceCreateWithName(name: CFStringRef) -> CGColorSpaceRef;
    fn CGColorSpaceRelease(space: CGColorSpaceRef);
}

/// The live NSView pointer for the window, or `None`.
fn ns_view_of(window: &Window) -> Option<*mut AnyObject> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(h) = handle.as_raw() else {
        return None;
    };
    Some(h.ns_view.as_ptr() as *mut AnyObject)
}

/// EDR headroom (potential max EDR multiplier; 1.0 if SDR) of the screen the view is
/// on — *not* `mainScreen`. `[window screen]` is nil for an off-screen window.
/// SAFETY: `ns_view` must be a live NSView; call on the main thread.
unsafe fn screen_max_edr(ns_view: *mut AnyObject) -> f32 {
    let win: *mut AnyObject = msg_send![ns_view, window];
    if win.is_null() {
        return 1.0;
    }
    let screen: *mut AnyObject = msg_send![win, screen];
    if screen.is_null() {
        return 1.0;
    }
    let v: f64 = msg_send![
        screen,
        maximumPotentialExtendedDynamicRangeColorComponentValue
    ];
    (v as f32).max(1.0)
}

/// Whether the window is currently in macOS **native (Spaces) fullscreen**, read from
/// the live `NSWindow.styleMask` (`NSWindowStyleMaskFullScreen`). This reflects the OS
/// truth however fullscreen was entered — our menu item, ⌃⌘F, the green title-bar
/// button, or a Mission Control gesture. We can't use winit's `Window::fullscreen()`
/// for this: in our borderless setup it tracks the *requested* mode and reads `None`
/// even while `toggleFullScreen:` has us fullscreen, so polling it never flips the
/// Enter/Exit label. `false` if the NSWindow can't be reached. Co-located here to reuse
/// [`ns_view_of`] + the objc2 plumbing; must run on the main thread.
pub fn window_is_fullscreen(window: &Window) -> bool {
    let Some(ns_view) = ns_view_of(window) else {
        return false;
    };
    // SAFETY: live NSView from the window handle; read NSWindow.styleMask, main thread.
    unsafe {
        let win: *mut AnyObject = msg_send![ns_view, window];
        if win.is_null() {
            return false;
        }
        let mask: usize = msg_send![win, styleMask];
        // NSWindowStyleMaskFullScreen = 1 << 14.
        mask & (1 << 14) != 0
    }
}

/// The window's current display EDR headroom — cheap (no layer poke). Used to notice
/// when the window has moved to a display with different HDR capability.
pub fn window_max_edr(window: &Window) -> f32 {
    let Some(ns_view) = ns_view_of(window) else {
        return 1.0;
    };
    // SAFETY: live NSView from the window handle, queried on the main thread.
    unsafe { screen_max_edr(ns_view) }
}

/// Configure the window's `CAMetalLayer` for the fp16 scRGB surface and return the
/// **window's screen** EDR headroom (potential max EDR multiplier; 1.0 if SDR) for
/// the present pass's highlight roll-off. Sets the layer colorspace to scRGB and
/// requests EDR only when the window's screen actually supports it. No-op (returns
/// 1.0) if the layer/screen can't be reached. Idempotent; must run on the main thread.
pub fn configure(window: &Window) -> f32 {
    let Some(ns_view) = ns_view_of(window) else {
        return 1.0;
    };
    // SAFETY: live NSView; we read its screen + set two layer properties, main thread.
    unsafe {
        let headroom = screen_max_edr(ns_view);
        let layer: *mut AnyObject = msg_send![ns_view, layer];
        if !layer.is_null() {
            let cs: CGColorSpaceRef = CGColorSpaceCreateWithName(kCGColorSpaceExtendedLinearSRGB);
            if !cs.is_null() {
                let _: () = msg_send![layer, setColorspace: cs];
                CGColorSpaceRelease(cs);
            }
            let wants_edr = headroom > 1.01;
            let _: () = msg_send![layer, setWantsExtendedDynamicRangeContent: Bool::new(wants_edr)];
        }
        headroom
    }
}
