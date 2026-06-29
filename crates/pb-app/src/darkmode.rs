//! Windows dark-mode glue for the native menu bar.
//!
//! muda custom-draws the menu *bar* dark and lets the OS theme the popup
//! dropdowns — but only once the app opts into dark mode through uxtheme's
//! **undocumented** exports, which winit's (documented, DWM-based) dark title bar
//! does not do. Without this, `MenuTheme::Auto`'s `IsDarkModeAllowedForWindow`
//! gate is false and the menu renders light even on a dark desktop.
//!
//! So we run the same win32-darkmode init the Notepad++/Tauri menus use:
//! `SetPreferredAppMode(AllowDark)` (the app follows the system theme) +
//! `FlushMenuThemes` (dark popups), and `AllowDarkModeForWindow(hwnd, true)`
//! per window (so `Auto`'s gate passes). All three are resolved by ordinal from
//! `uxtheme.dll`; a missing export just leaves the menu light (best-effort, never
//! fatal). Once enabled, muda's `Auto` tracks light↔dark at runtime on its own.

use windows::core::PCSTR;
use windows::Win32::Foundation::{FARPROC, HMODULE, HWND};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

// uxtheme.dll ordinals (stable across Win10 1903+ and Win11).
const ORD_ALLOW_DARK_MODE_FOR_WINDOW: usize = 133;
const ORD_SET_PREFERRED_APP_MODE: usize = 135;
const ORD_FLUSH_MENU_THEMES: usize = 136;

/// `PreferredAppMode::AllowDark` — "use dark mode where the system asks for it"
/// (i.e. follow the OS), as opposed to forcing dark/light.
const APPMODE_ALLOW_DARK: i32 = 1;

/// Resolve an export from `uxtheme.dll` by ordinal. The library is loaded once and
/// intentionally never freed (it lives for the process). `None` if it can't be
/// loaded or the ordinal is absent.
fn uxtheme_proc(ordinal: usize) -> FARPROC {
    use std::sync::OnceLock;
    static UXTHEME: OnceLock<usize> = OnceLock::new();
    let handle = *UXTHEME.get_or_init(|| unsafe {
        LoadLibraryA(PCSTR(c"uxtheme.dll".as_ptr() as *const u8))
            .map(|m| m.0 as usize)
            .unwrap_or(0)
    });
    if handle == 0 {
        return None;
    }
    // SAFETY: ordinal lookup — the ordinal goes in the low word of the name
    // pointer (MAKEINTRESOURCE); a valid module handle is required, which we have.
    unsafe {
        GetProcAddress(
            HMODULE(handle as *mut core::ffi::c_void),
            PCSTR(ordinal as *const u8),
        )
    }
}

/// Opt the whole app into following the system dark/light setting for menus, and
/// flush cached menu themes so popup dropdowns pick it up. Idempotent — safe to
/// call once at startup (and again on a theme change).
pub fn init_app() {
    if let Some(p) = uxtheme_proc(ORD_SET_PREFERRED_APP_MODE) {
        let set_preferred_app_mode: unsafe extern "system" fn(i32) -> i32 =
            unsafe { std::mem::transmute(p) };
        unsafe { set_preferred_app_mode(APPMODE_ALLOW_DARK) };
    }
    flush_menu_themes();
}

/// Allow dark mode for one window so muda's `MenuTheme::Auto` gate
/// (`IsDarkModeAllowedForWindow`) passes and the menu bar can render dark.
pub fn allow_for_window(hwnd: isize) {
    if let Some(p) = uxtheme_proc(ORD_ALLOW_DARK_MODE_FOR_WINDOW) {
        let allow_dark_mode_for_window: unsafe extern "system" fn(HWND, bool) -> bool =
            unsafe { std::mem::transmute(p) };
        unsafe { allow_dark_mode_for_window(HWND(hwnd as *mut core::ffi::c_void), true) };
    }
}

/// Re-flush cached menu themes (call on a runtime theme change so popups update to
/// the new system theme the next time they open).
pub fn flush_menu_themes() {
    if let Some(p) = uxtheme_proc(ORD_FLUSH_MENU_THEMES) {
        let flush: unsafe extern "system" fn() = unsafe { std::mem::transmute(p) };
        unsafe { flush() };
    }
}
