//! The native "About PhotoBlaze" dialog (Windows) — the app's first native dialog.
//!
//! A Win32 **TaskDialog** showing the app icon, name, version, tagline, copyright,
//! and a clickable GitHub link. Replaces the old custom centered overlay About.
//! Modal: it runs its own message loop and blocks until dismissed (fine — the app
//! isn't flying through photos with a dialog up, same as the file picker). macOS
//! would mirror this with an NSAlert/about panel behind the same `show` call.

use windows::core::{w, HRESULT, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Controls::{
    TaskDialogIndirect, TASKDIALOGCONFIG, TASKDIALOG_NOTIFICATIONS, TDCBF_OK_BUTTON,
    TDF_ALLOW_DIALOG_CANCELLATION, TDF_ENABLE_HYPERLINKS, TDF_USE_HICON_MAIN,
    TDN_HYPERLINK_CLICKED,
};
use windows::Win32::UI::Shell::{ExtractIconExW, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::{HICON, SW_SHOWNORMAL};

const URL: &str = "https://github.com/jdlien/photoblaze";

/// Encode a string as a NUL-terminated UTF-16 buffer for the Win32 W APIs.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The exe's large icon (for the dialog's main icon), or an invalid handle if it
/// can't be extracted (then the dialog simply shows no icon).
fn exe_icon() -> HICON {
    let Ok(exe) = std::env::current_exe() else {
        return HICON::default();
    };
    let path = wide(&exe.to_string_lossy());
    let mut big = HICON::default();
    // SAFETY: valid NUL-terminated path; we request one large icon into `big`.
    let n = unsafe { ExtractIconExW(PCWSTR(path.as_ptr()), 0, Some(&mut big), None, 1) };
    if n == 0 {
        HICON::default()
    } else {
        big
    }
}

/// TaskDialog callback: open the clicked hyperlink in the default browser.
unsafe extern "system" fn callback(
    _hwnd: HWND,
    msg: TASKDIALOG_NOTIFICATIONS,
    _wparam: WPARAM,
    lparam: LPARAM,
    _data: isize,
) -> HRESULT {
    if msg == TDN_HYPERLINK_CLICKED {
        // For TDN_HYPERLINK_CLICKED, lParam is the clicked href (a wide string).
        let url = PCWSTR(lparam.0 as *const u16);
        unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                url,
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    }
    HRESULT(0) // S_OK
}

/// Show the modal "About PhotoBlaze" dialog, owned by `parent` (the main window's
/// HWND as an `isize`, if available).
pub fn show(parent: Option<isize>) {
    let title = wide("About PhotoBlaze");
    let instruction = wide("PhotoBlaze");
    let content = wide(&format!(
        "Version {}\n\nAn ultra-fast photo viewer\n\n\u{00a9} JD Lien 2026\n\n<a href=\"{URL}\">github.com/jdlien/photoblaze</a>",
        env!("CARGO_PKG_VERSION"),
    ));
    let icon = exe_icon();

    let mut flags = TDF_ALLOW_DIALOG_CANCELLATION | TDF_ENABLE_HYPERLINKS;
    if !icon.is_invalid() {
        flags |= TDF_USE_HICON_MAIN;
    }

    let mut config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        dwFlags: flags,
        dwCommonButtons: TDCBF_OK_BUTTON,
        pszWindowTitle: PCWSTR(title.as_ptr()),
        pszMainInstruction: PCWSTR(instruction.as_ptr()),
        pszContent: PCWSTR(content.as_ptr()),
        pfCallback: Some(callback),
        ..Default::default()
    };
    // Union field: the main icon (used because TDF_USE_HICON_MAIN is set above).
    config.Anonymous1.hMainIcon = icon;
    if let Some(p) = parent {
        config.hwndParent = HWND(p as *mut core::ffi::c_void);
    }

    // SAFETY: `config` and the wide-string buffers outlive the (modal) call.
    unsafe {
        let _ = TaskDialogIndirect(&config, None, None, None);
    }
}
