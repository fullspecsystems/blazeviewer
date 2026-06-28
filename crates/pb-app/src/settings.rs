//! Persisted user preferences.
//!
//! **Privacy boundary (task #2):** this writes ONLY app preferences — currently
//! just the windowed/fullscreen choice — to the OS config dir. It never records
//! anything photo-*derived* (no viewed paths, no recent list, no thumbnails),
//! which is the actual privacy guarantee; app config is explicitly in-bounds
//! (ADR-018, and the no-trace test is scoped to photo data only).
//!
//! Format is a minimal `key = value` file (a forward-compatible subset of TOML,
//! so a future full config — tasks.json #8 — can absorb it). All I/O is
//! best-effort: a missing/unreadable file just means "use defaults," and a failed
//! write is silently ignored (a preference not sticking must never break viewing).

use std::path::PathBuf;

/// Per-user config directory for PhotoBlaze (created on demand), or `None` if the
/// platform's config location can't be determined.
fn config_dir() -> Option<PathBuf> {
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

fn settings_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("settings.toml"))
}

/// Parse the `fullscreen` preference from a settings file's contents. Pure, so
/// it's unit-testable without touching disk. `None` if the key is absent or its
/// value isn't a clear boolean.
fn parse_fullscreen(contents: &str) -> Option<bool> {
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "fullscreen" {
                return match value.trim() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                };
            }
        }
    }
    None
}

/// Load the saved fullscreen preference, or `None` if there's no (readable)
/// setting yet — the caller then picks its default.
pub fn load_fullscreen() -> Option<bool> {
    let path = settings_path()?;
    let contents = std::fs::read_to_string(path).ok()?;
    parse_fullscreen(&contents)
}

/// Persist the fullscreen preference (best-effort; errors are ignored).
pub fn save_fullscreen(fullscreen: bool) {
    let Some(path) = settings_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = format!(
        "# PhotoBlaze settings (auto-written; preferences only, never photo data)\nfullscreen = {fullscreen}\n"
    );
    let _ = std::fs::write(path, body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fullscreen_reads_true_false_and_ignores_noise() {
        assert_eq!(parse_fullscreen("fullscreen = true"), Some(true));
        assert_eq!(parse_fullscreen("fullscreen = false"), Some(false));
        assert_eq!(
            parse_fullscreen("# a comment\n\nfullscreen=true\n"),
            Some(true)
        );
        assert_eq!(parse_fullscreen("# fullscreen = true (commented)"), None);
        assert_eq!(parse_fullscreen("other = 1"), None);
        assert_eq!(parse_fullscreen("fullscreen = maybe"), None);
        assert_eq!(parse_fullscreen(""), None);
    }
}
