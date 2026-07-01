//! Per-user config location for PhotoBlaze.
//!
//! Shared by the keymap loader ([`crate::keymap`]) and the settings model (in the
//! shell). Read-only path computation here; the callers do the actual I/O.

use std::path::PathBuf;

/// Per-user config directory for PhotoBlaze (created on demand by callers), or
/// `None` if the platform's config location can't be determined. Holds
/// `keymap.toml` and `settings.toml`.
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("PhotoBlaze"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support/PhotoBlaze"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|c| c.join("photoblaze"))
    }
}
