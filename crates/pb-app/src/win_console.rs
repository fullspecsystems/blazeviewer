//! Windows console-attach shim (task #78).
//!
//! Release Windows builds are GUI-subsystem (`#![windows_subsystem = "windows"]`), so a
//! process launched from a terminal has **no console of its own** and clap's
//! `--help` / `--version` / error output would go to a void. [`attach_parent_console`]
//! attaches to the launching terminal's console (best-effort) and rebinds any invalid
//! standard handle to it, then reports whether a console is now available — which the
//! caller uses to choose between printing to stderr and popping a dialog.
//!
//! Everywhere else (Linux, and the macOS FFI host) a normal process already has working
//! stdout/stderr, so this is a no-op that returns `true`.
//!
//! Known cosmetic wart (accepted): the launching shell has already printed its next
//! prompt by the time we attach, so our output interleaves under that prompt.

/// Attach to the parent process's console if there is one, rebinding invalid std handles
/// so `println!` / `eprintln!` reach it. Returns whether a console is available for output
/// (drives the parse-error "stderr vs. dialog" choice). No-op on non-Windows (returns
/// `true`: stdout already works).
#[cfg(windows)]
pub fn attach_parent_console() -> bool {
    use std::fs::OpenOptions;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    // A std handle we can actually write to — a real console, a pipe, or a `> out.txt` redirect.
    let usable = |h: HANDLE| !h.is_invalid() && !h.0.is_null();

    unsafe {
        // Best-effort: attach to the launching terminal's console. Fails harmlessly when there is
        // no parent console (double-click / association) or we already have one (a debug build).
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);

        // Rebind any *invalid* std handle to the console (CONOUT$). Opening CONOUT$ fails
        // harmlessly when there is no console (e.g. output is a pipe), leaving the handle as-is; a
        // handle that is already usable (a real console, or a pipe / file redirect) is left
        // untouched so the redirection is honored.
        for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let invalid = GetStdHandle(which).map(|h| !usable(h)).unwrap_or(true);
            if invalid {
                if let Ok(f) = OpenOptions::new().read(true).write(true).open("CONOUT$") {
                    let _ = SetStdHandle(which, HANDLE(f.as_raw_handle() as _));
                    // Keep the handle alive for the process; the OS reclaims it at exit.
                    std::mem::forget(f);
                }
            }
        }

        // Output is available iff stderr is now a usable handle. Only a genuine GUI launch with no
        // console *and* no redirection leaves it invalid → the caller uses a dialog for real errors.
        GetStdHandle(STD_ERROR_HANDLE).map(usable).unwrap_or(false)
    }
}

/// No-op on non-Windows: a normal process already has working stdout/stderr.
#[cfg(not(windows))]
pub fn attach_parent_console() -> bool {
    true
}
