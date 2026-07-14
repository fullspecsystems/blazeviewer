//! Windows single-instance election + launch forwarding (task #14).
//!
//! On Windows, opening a photo from Explorer (double-click via the file
//! association, or "Open") spawns a **fresh PhotoBlaze process every time** — each
//! with its own decode pool + resident VRAM ring, all racing to write
//! `settings.toml`. Multi-selecting N files launches N processes (Explorer caps
//! this at ~15). macOS already does the right thing through LaunchServices (one
//! instance, one batch open); this module gives Windows the same behavior.
//!
//! **How it works.** The first launch creates a named mutex and becomes the
//! *primary*: it runs a tiny message-only window on a background thread that
//! listens for `WM_COPYDATA`. Every later launch finds the mutex already held,
//! becomes a *secondary*, forwards its (absolutized) paths to the primary's window
//! via `WM_COPYDATA`, asks the OS to let the primary take the foreground, and
//! exits **before** it ever builds a GPU surface or decode pool. The primary opens
//! the forwarded paths exactly as if they'd been dropped on the window
//! (`classify_inputs` → `open_input`), so a single file scans its folder, several
//! files become an explicit playlist, a folder browses recursively, and an archive
//! opens — one code path for every entry point.
//!
//! Paired with `MultiSelectModel=Player` on the ProgIds (see `default_app.rs`),
//! Explorer collapses a multi-select "Open" into **one** invocation with all paths
//! in a deterministic order, so the whole selection arrives as a single, ordered
//! `WM_COPYDATA` instead of a storm of racing single-file processes.
//!
//! `--new-window` is the reserved escape hatch: it skips election entirely and runs
//! a standalone instance (no mutex, no forwarding, no IPC server).
//!
//! **Privacy (ADR-018).** This is IPC + process bookkeeping only. The mutex and
//! window names are static and carry no photo data; the forwarded paths live in RAM
//! for the duration of one `SendMessage` and are never persisted. Same category as
//! the association registry writes — setup/plumbing, not a viewing trace.
//!
//! Best-effort throughout: any failure (mutex, window, or send) degrades to the old
//! behavior — the launch just runs as its own instance rather than being lost.

use core::ffi::c_void;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT,
    WPARAM,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowExW,
    GetMessageW, GetWindowThreadProcessId, RegisterClassW, SendMessageW, TranslateMessage,
    HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_COPYDATA, WNDCLASSW,
};

/// The per-user, per-session mutex name. `Local\` scopes it to the login session
/// (a second RDP/console session for the same user gets its own primary — desired).
///
/// Built at runtime rather than `const` because it derives from
/// [`pb_app_core::APP_IDENT`]; both are called once at startup, so the allocation is
/// irrelevant. Renaming the app renames these, which means a build from before the
/// rename and one from after do not see each other — two primaries. Harmless, and
/// only during the changeover.
fn mutex_name() -> String {
    format!("Local\\{}.SingleInstance", pb_app_core::APP_IDENT)
}

/// The message-only window's class name. The secondary finds the primary by this
/// class under `HWND_MESSAGE`; the primary registers it on its IPC thread.
fn window_class() -> String {
    format!("{}.SingleInstance.Ipc", pb_app_core::APP_IDENT)
}

/// `COPYDATASTRUCT::dwData` tag ("PB14") so the primary only accepts a payload that
/// is actually one of ours, not an unrelated `WM_COPYDATA` from another app.
const MAGIC: usize = 0x5042_3134;

/// Paths received from secondary launches, drained by the app each tick.
static INBOX: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Wakes the winit event loop when a forward arrives (the loop is `ControlFlow::Wait`,
/// so an idle primary would otherwise never notice). Set once by [`serve`].
static WAKER: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// The outcome of the single-instance election.
pub enum Instance {
    /// We hold the mutex — run normally and (via [`serve`]) receive forwards.
    /// The guard must be kept alive for the whole process.
    Primary(Guard),
    /// Another primary already exists — forward to it and exit.
    Secondary,
}

/// Owns the mutex handle; closing it on drop releases primacy (moot at process exit,
/// but tidy). Held in `main` for the process lifetime.
pub struct Guard(HANDLE);

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// UTF-16, NUL-terminated — for a class / mutex name passed as `PCWSTR`.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Create-or-open the named mutex to decide whether we are the primary or a
/// secondary. A brand-new mutex → primary; an already-existing one → secondary.
/// A create failure degrades to primary (no single-instance, but the app still runs).
pub fn acquire() -> Instance {
    let name = wide(&mutex_name());
    unsafe {
        match CreateMutexW(None, false, PCWSTR(name.as_ptr())) {
            Ok(handle) => {
                // ERROR_ALREADY_EXISTS: the handle is valid but names the *existing*
                // mutex — someone else is primary. Close our reference and forward.
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = CloseHandle(handle);
                    Instance::Secondary
                } else {
                    Instance::Primary(Guard(handle))
                }
            }
            // Can't create it at all: don't block the launch — run standalone.
            Err(_) => Instance::Primary(Guard(HANDLE::default())),
        }
    }
}

/// Make each path absolute (Explorer passes absolute paths already; a CLI relative
/// path is resolved against *this* process's cwd before it's sent, since the
/// primary's cwd may differ). Uses `std::path::absolute` — it does not touch the
/// disk or resolve symlinks, so paths keep the plain form the primary's folder scan
/// produces, which its cursor match relies on (unlike `canonicalize`'s `\\?\`).
pub fn absolutize(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|p| std::path::absolute(p).unwrap_or_else(|_| p.clone()))
        .collect()
}

/// Secondary path: find the primary's message-only window and hand it `paths` via
/// `WM_COPYDATA`, then let the primary take the foreground. Returns whether the
/// paths were delivered — `false` means the caller should fall back to launching
/// standalone (better than dropping the open).
///
/// Retries briefly: the primary can hold the mutex a moment before its IPC window
/// exists (a cold start racing this launch), so we poll for it rather than give up.
pub fn forward(paths: &[PathBuf]) -> bool {
    let class = wide(&window_class());
    // ~5 s of headroom for a primary that's still coming up; each miss sleeps 100 ms.
    for _ in 0..50 {
        let hwnd = unsafe {
            FindWindowExW(
                Some(HWND_MESSAGE),
                None,
                PCWSTR(class.as_ptr()),
                PCWSTR::null(),
            )
        };
        if let Ok(hwnd) = hwnd {
            if !hwnd.0.is_null() {
                return unsafe { send_paths(hwnd, paths) };
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Send `paths` to the primary window as a single `WM_COPYDATA` (newline-joined
/// UTF-8), first granting the primary's process the right to take the foreground so
/// its `SetForegroundWindow` (via winit's `focus_window`) actually raises the window.
///
/// `SendMessageW` (never `PostMessage`) is mandatory for `WM_COPYDATA`: the call is
/// synchronous, so the payload buffer stays valid until the primary has copied it.
unsafe fn send_paths(hwnd: HWND, paths: &[PathBuf]) -> bool {
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid != 0 {
        let _ = AllowSetForegroundWindow(pid);
    }

    // Windows paths cannot contain '\n', so it is a safe separator.
    let joined = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    let bytes = joined.as_bytes();
    let cds = COPYDATASTRUCT {
        dwData: MAGIC,
        cbData: bytes.len() as u32,
        lpData: bytes.as_ptr() as *mut c_void,
    };
    let r = SendMessageW(
        hwnd,
        WM_COPYDATA,
        Some(WPARAM(0)),
        Some(LPARAM(&cds as *const COPYDATASTRUCT as isize)),
    );
    // Our wndproc returns 1 for an accepted payload.
    r.0 != 0
}

/// Primary path: start the IPC server (a message-only window on its own thread) and
/// register the loop `waker`. Called once, after the event loop exists, only when
/// this process won the election. Failure to create the window is non-fatal — the
/// primary just won't receive forwards (secondaries then run standalone).
pub fn serve<F: Fn() + Send + Sync + 'static>(waker: F) {
    let _ = WAKER.set(Box::new(waker));
    let _ = std::thread::Builder::new()
        .name("pb-single-instance".into())
        .spawn(|| unsafe { run_ipc_window() });
}

/// Drain the paths forwarded by secondary launches since the last call. Cheap when
/// empty; called by the app each tick after a wake.
pub fn take_forwarded() -> Vec<PathBuf> {
    INBOX
        .lock()
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default()
}

/// Register the class, create the `HWND_MESSAGE` window, and pump its messages
/// forever (until the process exits). Runs on the dedicated IPC thread.
unsafe fn run_ipc_window() {
    let Ok(hmodule) = GetModuleHandleW(None) else {
        return;
    };
    let hinstance = HINSTANCE(hmodule.0);
    let class = wide(&window_class());

    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance,
        lpszClassName: PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    // Returns 0 on failure (incl. already-registered); either way we try to create
    // the window next and bail only if *that* fails.
    RegisterClassW(&wc);

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(class.as_ptr()),
        PCWSTR::null(),
        WINDOW_STYLE(0),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE),
        None,
        Some(hinstance),
        None,
    );
    if hwnd.is_err() {
        return;
    }

    let mut msg = MSG::default();
    loop {
        let r = GetMessageW(&mut msg, None, 0, 0);
        // 0 = WM_QUIT (never sent here), -1 = error — either ends the pump.
        if r.0 <= 0 {
            break;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// The message-only window's proc: accept our tagged `WM_COPYDATA`, split the
/// newline-joined UTF-8 payload into paths, push them to the [`INBOX`], and wake the
/// event loop. Everything else falls through to the default proc.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_COPYDATA {
        let cds = lparam.0 as *const COPYDATASTRUCT;
        if !cds.is_null() {
            let cds = &*cds;
            if cds.dwData == MAGIC && !cds.lpData.is_null() && cds.cbData > 0 {
                let bytes =
                    std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize);
                if let Ok(text) = std::str::from_utf8(bytes) {
                    let paths: Vec<PathBuf> = text
                        .split('\n')
                        .filter(|s| !s.is_empty())
                        .map(PathBuf::from)
                        .collect();
                    if !paths.is_empty() {
                        if let Ok(mut inbox) = INBOX.lock() {
                            inbox.extend(paths);
                        }
                        if let Some(waker) = WAKER.get() {
                            waker();
                        }
                    }
                }
            }
        }
        // Report handled so the sender's SendMessage returns non-zero.
        return LRESULT(1);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolutize_makes_relative_paths_absolute() {
        let rel = PathBuf::from("some_photo.jpg");
        let out = absolutize(std::slice::from_ref(&rel));
        assert_eq!(out.len(), 1);
        assert!(out[0].is_absolute(), "relative path should be absolutized");
        assert!(out[0].ends_with("some_photo.jpg"));
    }

    #[test]
    fn absolutize_leaves_absolute_paths_intact() {
        let abs = PathBuf::from("C:\\photos\\a.jpg");
        let out = absolutize(std::slice::from_ref(&abs));
        assert_eq!(out, vec![abs]);
    }

    #[test]
    fn take_forwarded_is_empty_by_default() {
        // Nothing has been pushed in this unit-test process.
        assert!(take_forwarded().is_empty());
    }
}
