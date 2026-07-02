//! Reveal a file in the OS file manager — the shell side of
//! [`CoreEffect::RevealPath`](pb_app_core::contract::CoreEffect::RevealPath) (task #40).
//!
//! "Reveal" = open the file's **containing folder** with the file **selected**, the
//! platform idiom behind macOS *Show in Finder* / Windows *Show in File Explorer*. The
//! core validates there's a real on-disk file before emitting the effect; this just
//! launches the native file manager on that path — a read-only, privacy-safe user
//! command (privacy #2, same category as Copy File Path), never the view path.
//!
//! Best-effort: a spawn failure is logged (the release exe is GUI-subsystem, so this is
//! only visible under a dev build / log file) and swallowed — a file manager that won't
//! launch shouldn't take the viewer down.

use std::path::Path;
use std::process::Command;

/// Reveal `path` in the platform file manager, selecting the file where the OS supports
/// it (macOS + Windows do; Linux `xdg-open` can only open the containing folder).
pub fn in_file_manager(path: &Path) {
    if let Err(e) = launch(path) {
        eprintln!(
            "reveal: failed to open file manager for {}: {e}",
            path.display()
        );
    }
}

/// macOS: `open -R <path>` asks Finder to reveal (containing folder + select the file) —
/// the CLI form of `NSWorkspace.activateFileViewerSelectingURLs`, no extra deps.
#[cfg(target_os = "macos")]
fn launch(path: &Path) -> std::io::Result<()> {
    Command::new("open").arg("-R").arg(path).spawn().map(|_| ())
}

/// Windows: `explorer /select,"<path>"` opens the containing folder and selects the file.
/// Explorer parses its own command line, splitting on space/comma/`=` outside quotes — so
/// the quotes must wrap **only the path**, not the whole `/select,…` token. `raw_arg`
/// (not `arg`) is required: `arg` would quote the entire argument when the path contains
/// a space, which makes Explorer stop recognizing the `/select` switch and fall back to
/// opening Documents. `"` is illegal in Windows filenames, so no escaping is needed.
/// Explorer needs backslash separators — real filesystem paths from the directory scan
/// already use them. Explorer often returns a non-zero exit code even on success, so we
/// only `spawn` and never check the status.
#[cfg(windows)]
fn launch(path: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn()
        .map(|_| ())
}

/// Linux / other: best-effort `xdg-open` on the **containing folder** — there's no
/// portable per-file "select" via `xdg-open`, so we open the directory (task #40).
#[cfg(not(any(target_os = "macos", windows)))]
fn launch(path: &Path) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(path);
    Command::new("xdg-open").arg(dir).spawn().map(|_| ())
}
