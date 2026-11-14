//! The native **save** dialog for image export (task #89, `CoreEffect::ShowSavePanel`), behind
//! one helper seam per platform (export-plan.md § 3.6).
//!
//! - **Windows:** our own `IFileSaveDialog` wrapper, because `rfd` 0.14.1 can't set
//!   `FOS_DONTADDTORECENT` — and the owner's call (2026-11-13) is that exports never land in
//!   the OS's Recent Items. `SetFolder` (not `SetDefaultFolder`) forces the start folder the
//!   core chose, so the dialog's own folder memory never decides where it opens.
//! - **Linux:** `rfd` (GTK). Whether GTK records the file in `recently-used.xbel` is an open
//!   verification item (plan § 3.6); if it does and can't be prevented, that's an owner call.
//!
//! Both are modal on the main thread, exactly like the existing Open dialogs.

use std::path::{Path, PathBuf};

/// Run the save dialog. `None` = cancelled (or the dialog couldn't be shown).
#[cfg(windows)]
pub fn save_png(owner: Option<isize>, start_dir: &Path, suggested_name: &str) -> Option<PathBuf> {
    use windows::core::{w, HSTRING};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{
        FileSaveDialog, IFileSaveDialog, IShellItem, SHCreateItemFromParsingName,
        FOS_DONTADDTORECENT, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, FOS_OVERWRITEPROMPT,
        FOS_STRICTFILETYPES, SIGDN_FILESYSPATH,
    };

    // SAFETY: standard COM usage on the UI thread; every interface is released by its
    // wrapper's Drop, the returned PWSTR is freed with CoTaskMemFree, and COM is balanced.
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let result = (|| -> Option<PathBuf> {
            let dialog: IFileSaveDialog =
                CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            let options = dialog.GetOptions().ok()?;
            dialog
                .SetOptions(
                    options
                        | FOS_OVERWRITEPROMPT
                        | FOS_STRICTFILETYPES
                        | FOS_FORCEFILESYSTEM
                        | FOS_NOCHANGEDIR
                        | FOS_DONTADDTORECENT,
                )
                .ok()?;
            let filters = [COMDLG_FILTERSPEC {
                pszName: w!("PNG image"),
                pszSpec: w!("*.png"),
            }];
            dialog.SetFileTypes(&filters).ok()?;
            dialog.SetDefaultExtension(w!("png")).ok()?;
            let _ = dialog.SetTitle(w!("Export as PNG"));
            let _ = dialog.SetOkButtonLabel(w!("Export"));
            let _ = dialog.SetFileName(&HSTRING::from(suggested_name));
            if let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(
                &HSTRING::from(start_dir.as_os_str()),
                None,
            ) {
                let _ = dialog.SetFolder(&folder);
            }
            let owner = owner.map(|h| HWND(h as *mut core::ffi::c_void));
            dialog.Show(owner).ok()?; // Err = cancelled
            let item = dialog.GetResult().ok()?;
            let raw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let path = raw.to_string().ok();
            CoTaskMemFree(Some(raw.0 as *const core::ffi::c_void));
            path.map(PathBuf::from)
        })();
        if init.is_ok() {
            CoUninitialize();
        }
        result
    }
}

/// Run the save dialog. `None` = cancelled.
#[cfg(not(windows))]
pub fn save_png(_owner: Option<isize>, start_dir: &Path, suggested_name: &str) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Export as PNG")
        .set_directory(start_dir)
        .set_file_name(suggested_name)
        .add_filter("PNG image", &["png"])
        .save_file()
}
