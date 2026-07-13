//! Accent-color source resolution (brand vs. OS-accent vs. custom).
//!
//! The chrome accent (`pb_ui`'s `Palette::accent`) defaults to the PhotoBlaze brand orange but
//! can follow the OS accent or a custom pick ([`AccentSource`]). This module reads the platform
//! accent and resolves the chosen source to one concrete, contrast-guarded color that the shell
//! pushes into [`pb_ui::set_accent`] (process-wide — every panel/dialog then tracks it).
//!
//! The OS read is Windows-only for now; Linux/macOS report no system accent, so `System`
//! degrades to the brand there (the owner's call — Linux stays orange).

use egui::Color32;
use pb_app_core::settings::{AccentSource, Settings};

/// The OS accent color as RGB, or `None` when the platform has no accent to offer (non-Windows
/// today, or the WinRT query failed). On Windows this is the user's *Settings ▸ Colors* accent.
#[cfg(windows)]
pub fn system_accent() -> Option<[u8; 3]> {
    use windows::UI::ViewManagement::{UIColorType, UISettings};
    let settings = UISettings::new().ok()?;
    let c = settings.GetColorValue(UIColorType::Accent).ok()?;
    Some([c.R, c.G, c.B])
}

#[cfg(not(windows))]
pub fn system_accent() -> Option<[u8; 3]> {
    None
}

/// Resolve the chosen accent source to a concrete color for the current theme, applying the
/// legibility guard (a too-pale / near-invisible pick falls back to the brand). `dark` is the
/// resolved light/dark state so the guard weighs contrast against the right page background.
pub fn resolve(settings: &Settings, dark: bool) -> Color32 {
    let candidate = match settings.accent_source {
        AccentSource::Brand => None,
        AccentSource::Custom => Some(settings.accent_custom),
        AccentSource::System => system_accent(),
    };
    match candidate {
        Some([r, g, b]) => pb_ui::ensure_legible(Color32::from_rgb(r, g, b), dark),
        None => pb_ui::BRAND_ACCENT,
    }
}

/// Push the resolved accent into `pb_ui` (process-wide) so all chrome — the overlay panels and
/// the dialogs — picks it up on its next build. Call at startup and on a settings/theme change.
pub fn apply(settings: &Settings, dark: bool) {
    pb_ui::set_accent(Some(resolve(settings, dark)));
}

/// Set when the OS accent/theme colors change under us (Windows `ColorValuesChanged`), so the
/// event loop re-resolves the accent live. Always `false` off Windows.
#[cfg(windows)]
static ACCENT_DIRTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Take + clear the "OS accent changed" flag. `true` means the shell should invalidate its
/// accent change-guard and re-resolve this turn (the System source re-reads the OS accent).
pub fn take_os_accent_changed() -> bool {
    #[cfg(windows)]
    {
        ACCENT_DIRTY.swap(false, std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// A live subscription to the OS accent-color change signal. Held by the shell so the callback
/// stays registered — the handler lives as long as this `UISettings` instance (COM refcount), so
/// there's no token to keep; dropping the watcher releases it. Empty off Windows.
#[cfg(windows)]
pub struct AccentWatcher {
    _settings: windows::UI::ViewManagement::UISettings,
}
#[cfg(not(windows))]
pub struct AccentWatcher;

/// Subscribe to Windows accent/theme color changes. On a change it flags
/// [`take_os_accent_changed`] and pokes `window` so the event loop wakes and re-resolves the
/// accent with no restart. Returns the watcher to keep alive (`None` if the WinRT settings
/// object can't be created). No-op (returns `None`) off Windows.
#[cfg(windows)]
pub fn watch_system_accent(window: std::sync::Arc<winit::window::Window>) -> Option<AccentWatcher> {
    use std::sync::atomic::Ordering;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::ViewManagement::UISettings;
    let settings = UISettings::new().ok()?;
    // The delegate runs on a WinRT thread-pool thread — flag the change, then request a redraw
    // (thread-safe on Windows: it posts a message) so the loop wakes and polls the flag.
    let handler = TypedEventHandler::new(move |_sender, _args| {
        ACCENT_DIRTY.store(true, Ordering::Relaxed);
        window.request_redraw();
        Ok(())
    });
    settings.ColorValuesChanged(&handler).ok()?;
    Some(AccentWatcher {
        _settings: settings,
    })
}

#[cfg(not(windows))]
pub fn watch_system_accent(
    _window: std::sync::Arc<winit::window::Window>,
) -> Option<AccentWatcher> {
    None
}
