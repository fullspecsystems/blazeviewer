//! Per-user config location.
//!
//! Shared by the keymap loader ([`crate::keymap`]) and the settings model (in the
//! shell). Read-only path computation here; the callers do the actual I/O.

use std::path::PathBuf;

/// Per-user config directory (created on demand by callers), or `None` if the
/// platform's config location can't be determined. Holds `keymap.toml` and
/// `settings.toml`.
///
/// The directory is named from [`crate::APP_IDENT`] on Windows/macOS (whose
/// conventions favour the readable product name) and [`crate::APP_SLUG`] on Linux
/// (which favours a lowercase dotfile). Renaming either constant **moves this
/// directory**, orphaning an existing install's config — see task #101, where that
/// is the accepted, deliberate outcome (no migration; the keymap is re-created).
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join(crate::APP_IDENT))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join("Library/Application Support")
                .join(crate::APP_IDENT)
        })
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|c| c.join(crate::APP_SLUG))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The directory must end in the app's own name — not a hardcoded "PhotoBlaze",
    /// which is exactly what task #101 is unpicking. Guards against a rename that
    /// updates the constant but leaves a stale literal behind.
    #[test]
    fn config_dir_is_named_from_the_app_constants() {
        let Some(dir) = config_dir() else { return };
        let leaf = dir.file_name().unwrap().to_string_lossy().into_owned();
        let expected = if cfg!(any(windows, target_os = "macos")) {
            crate::APP_IDENT
        } else {
            crate::APP_SLUG
        };
        assert_eq!(
            leaf, expected,
            "config dir leaf must come from the constants"
        );
    }

    /// macOS must sit under `Application Support`, not the Linux/XDG shape. The
    /// previous code built this as one joined literal, so a rename could plausibly
    /// have mangled the parent path too.
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_config_dir_lives_under_application_support() {
        let dir = config_dir().expect("HOME is set under test");
        assert!(
            dir.ends_with(format!("Library/Application Support/{}", crate::APP_IDENT)),
            "unexpected macOS config path: {}",
            dir.display()
        );
    }
}
