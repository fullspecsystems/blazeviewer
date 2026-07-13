//! Delete the current photo (tasks.json #28): `Del` → Recycle Bin (recoverable, no
//! prompt), `Shift+Del` → permanent **with a confirmation**. This is the first op
//! that *removes* a user's file — like Save Rotation it only ever runs on an explicit
//! command, never as a byproduct of viewing (the "modify only on explicit command"
//! boundary in CLAUDE.md).
//!
//! The pure playlist-cursor math (`cursor_after_removal`) moved to `pb_app_core::engine`
//! (NS0 5.5 / Phase B) with `flush_pending_delete`; the file deletion ([`recycle`] /
//! [`delete_permanently`]) is the I/O shell that stays here. The permanent-delete confirmation
//! is a themed egui dialog (`dialog::DialogKind::Confirm`), and the playlist rebuild + advance
//! live in `AppCore` now.
//!
//! **Recycle-Bin awareness (task #86).** A drive can be configured to *bypass* the Recycle
//! Bin (Recycle Bin Properties → "Don't move files to the Recycle Bin"). On such a volume a
//! "recycle" silently permanently deletes — the shell honors the per-volume policy — with no
//! undo and (via the `trash` crate's `FOF_NO_UI`) no warning. Two seams close that hole:
//! [`will_recycle`] predicts it *before* the delete (so `Del` can route to the permanent-delete
//! confirmation), and [`recycle`] reports the actual outcome *after* (so the toast icon is honest
//! and an undo entry is only recorded when the file is genuinely restorable).
//!
//! **In-app Undo ([`restore`]) is now cross-platform.** On Windows/Linux the `trash` crate's
//! `os_limited` module lists the bin and restores from it. macOS has no such listing API, so we
//! delete through `NSFileManager trashItemAtURL:resultingItemURL:` ourselves (see [`macos`]),
//! keep the file's resulting location in the Trash, and Undo just moves it back — the Trash lives
//! on the deleted file's own volume, so the restore is a same-volume rename.

use std::path::Path;

/// Permanently delete `path` (irreversible). Reached only after the egui confirm
/// dialog answers Yes, mirroring Explorer's `Shift+Del`.
pub fn delete_permanently(path: &Path) -> Result<(), String> {
    std::fs::remove_file(path).map_err(|e| e.to_string())
}

/// Whether sending `path` to the Recycle Bin will actually *recycle* it (recoverable) rather
/// than silently permanently delete it. `false` only when we can positively determine the
/// path's volume bypasses the bin; **any uncertainty returns `true`** (the caller then does the
/// normal recoverable delete, and [`recycle`] catches a mispredict after the fact) so a normal
/// drive never gets a spurious permanent-delete confirmation.
///
/// Windows-only signal today (the per-volume `NukeOnDelete` policy). On macOS/Linux the Trash is
/// always used and there is no silent-nuke volume policy — a Trash that can't be written *fails
/// loudly* rather than deleting — so this is `true`, and the failure is handled by routing to the
/// permanent-delete confirmation instead (see `AppCore::do_delete`).
#[cfg(windows)]
pub fn will_recycle(path: &Path) -> bool {
    win::will_recycle(path).unwrap_or(true)
}

/// Non-Windows: the Trash is always used, so a recycle always recycles (a failure is a loud
/// error, never a silent permanent delete — see the Windows doc above).
#[cfg(not(windows))]
pub fn will_recycle(_path: &Path) -> bool {
    true
}

/// The pure Recycle-Bin decision given the two registry signals: the group-policy
/// `NoRecycleFiles` (`policy_nuke`) and the path's per-volume `NukeOnDelete` (`volume_nuke`),
/// each `Some(true|false)` when read or `None` when absent. Recycles unless a signal explicitly
/// forces permanent deletion — an absent per-volume value means "use the default", which is to
/// recycle. Split out so it's unit-testable without touching the registry.
#[cfg(windows)]
pub(crate) fn recycle_decision(policy_nuke: Option<bool>, volume_nuke: Option<bool>) -> bool {
    if policy_nuke == Some(true) {
        return false;
    }
    !matches!(volume_nuke, Some(true))
}

/// The outcome of a recoverable ([`recycle`]) delete.
pub enum RecycleOutcome {
    /// The file is in the bin and can be restored (Edit ▸ Undo) via [`restore`].
    Recycled(RestoreHandle),
    /// The file is *not* in the bin — a bypass-the-bin volume permanently deleted it. No undo;
    /// the caller shows the permanent-delete icon rather than the misleading recycle one.
    /// (Windows/Linux only — macOS never silently nukes, so its [`recycle`] never returns this.)
    Permanent,
}

// ── Windows + FreeDesktop (Linux): restore via the `trash` crate's `os_limited` module ──

/// A restorable handle to a just-recycled file — the `trash` crate's [`trash::TrashItem`] on the
/// platforms that can list and restore the bin (Windows/Linux).
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
pub struct RestoreHandle(trash::TrashItem);

/// Send `path` to the OS Recycle Bin / Trash and report whether it actually recycled to a
/// restorable bin. No confirmation — the deletion is reversible, mirroring Explorer's `Del`.
/// `Err` when the OS refused the trash operation (the caller then offers permanent delete).
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
pub fn recycle(path: &Path) -> Result<RecycleOutcome, String> {
    trash::delete(path).map_err(|e| e.to_string())?;
    // Confirm the file actually reached a restorable bin: a bypass-the-bin volume that slipped
    // past `will_recycle` will have permanently deleted it, and there's nothing to restore.
    Ok(match find_trashed(path) {
        Some(item) => RecycleOutcome::Recycled(RestoreHandle(item)),
        None => RecycleOutcome::Permanent,
    })
}

/// Restore a recycled file to its original location (Edit ▸ Undo). Errors if something now
/// occupies that path (`RestoreCollision`) or the item can't be restored.
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
pub fn restore(handle: RestoreHandle) -> Result<(), String> {
    trash::os_limited::restore_all([handle.0]).map_err(|e| e.to_string())
}

/// The newest Recycle-Bin entry whose original location is `original`, if any. Scans the whole
/// bin (`os_limited::list`) and matches on the original path — case-insensitively on Windows,
/// where paths aren't case-sensitive. Picks the most recently deleted on the off chance of
/// duplicates.
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
fn find_trashed(original: &Path) -> Option<trash::TrashItem> {
    let items = trash::os_limited::list().ok()?;
    items
        .into_iter()
        .filter(|it| same_path(&it.original_path(), original))
        .max_by_key(|it| it.time_deleted)
}

/// Path equality for matching a trashed item to its origin: case-insensitive on Windows
/// (NTFS/`SIGDN` parsing names aren't case-sensitive), exact elsewhere.
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
fn same_path(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        a.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

// ── macOS: no `os_limited` in the `trash` crate, so we drive `NSFileManager` ourselves ──

/// A restorable handle on macOS: the file's original location plus where it now lives inside the
/// Trash (from `trashItemAtURL:resultingItemURL:`). Undo moves `trashed` back to `original`.
#[cfg(target_os = "macos")]
pub struct RestoreHandle {
    original: std::path::PathBuf,
    trashed: std::path::PathBuf,
}

/// Send `path` to the macOS Trash and return a restorable handle. macOS never silently
/// permanently deletes (a Trash it can't write *fails*), so a success is always `Recycled`.
/// `Err` when the OS refused (read-only / some network volumes) — the caller then offers a
/// permanent delete with confirmation.
#[cfg(target_os = "macos")]
pub fn recycle(path: &Path) -> Result<RecycleOutcome, String> {
    let trashed = macos::trash_item(path)?;
    Ok(RecycleOutcome::Recycled(RestoreHandle {
        original: path.to_path_buf(),
        trashed,
    }))
}

/// Restore a trashed file (Edit ▸ Undo) by moving it from the Trash back to its original path.
/// Errors on a collision (something now occupies the original path) — matching the Windows/Linux
/// `RestoreCollision` behavior — leaving the file safely in the Trash.
#[cfg(target_os = "macos")]
pub fn restore(handle: RestoreHandle) -> Result<(), String> {
    if handle.original.exists() {
        return Err(format!("{} already exists", handle.original.display()));
    }
    // The Trash lives on the deleted file's own volume, so this is a same-volume atomic rename.
    std::fs::rename(&handle.trashed, &handle.original).map_err(|e| e.to_string())
}

#[cfg(target_os = "macos")]
mod macos {
    //! The macOS Trash FFI, quarantined here (like the Windows `win` module). `NSFileManager`'s
    //! `trashItemAtURL:resultingItemURL:` moves a file to the Trash *and* hands back where it
    //! landed — the piece the `trash` crate throws away — which is exactly what an in-app Undo
    //! needs to move it back. Runs on the host main thread (the Delete command), like the rest of
    //! the delete op.

    use objc2::rc::Retained;
    use objc2_foundation::{NSFileManager, NSString, NSURL};
    use std::path::{Path, PathBuf};

    /// Move `path` to the Trash, returning its resulting location inside the Trash.
    pub fn trash_item(path: &Path) -> Result<PathBuf, String> {
        let src = NSString::from_str(&path.to_string_lossy());
        let url = NSURL::fileURLWithPath(&src);
        let fm = NSFileManager::defaultManager();
        let mut resulting: Option<Retained<NSURL>> = None;
        fm.trashItemAtURL_resultingItemURL_error(&url, Some(&mut resulting))
            .map_err(|e| e.localizedDescription().to_string())?;
        let resulting = resulting.ok_or("Trash returned no resulting URL")?;
        let path = resulting.path().ok_or("Trashed item URL has no path")?;
        Ok(PathBuf::from(path.to_string()))
    }
}

#[cfg(windows)]
mod win {
    //! Windows Recycle-Bin policy probe: map a path to its volume GUID and read the
    //! `NukeOnDelete` / `NoRecycleFiles` registry values that decide whether a delete recycles.
    //! All FFI is quarantined here; the decision itself is [`super::recycle_decision`] (pure,
    //! tested). Any failure returns `None` → the caller assumes recycling.

    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    };
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
        KEY_READ, REG_DWORD, REG_VALUE_TYPE,
    };

    const BITBUCKET_VOLUME: &str =
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\";
    const POLICIES_EXPLORER: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";

    /// `Some(true)` if `path`'s volume will recycle, `Some(false)` if it bypasses the bin, `None`
    /// if we couldn't determine it (caller assumes recycling).
    pub fn will_recycle(path: &Path) -> Option<bool> {
        // Group policy forces permanent deletion for every volume when set — check it first so a
        // volume-GUID failure can't mask it.
        let policy_nuke = read_policy_nuke();
        if policy_nuke == Some(true) {
            return Some(false);
        }
        let guid = volume_guid(path)?;
        let volume_nuke = read_dword(
            HKEY_CURRENT_USER,
            &format!("{BITBUCKET_VOLUME}{guid}"),
            "NukeOnDelete",
        )
        .map(|v| v != 0);
        Some(super::recycle_decision(policy_nuke, volume_nuke))
    }

    /// The group-policy `NoRecycleFiles` value (HKCU wins over HKLM), as a nuke-everything flag.
    fn read_policy_nuke() -> Option<bool> {
        read_dword(HKEY_CURRENT_USER, POLICIES_EXPLORER, "NoRecycleFiles")
            .or_else(|| read_dword(HKEY_LOCAL_MACHINE, POLICIES_EXPLORER, "NoRecycleFiles"))
            .map(|v| v != 0)
    }

    /// The `{GUID}` (braces included — the registry subkey name) of the volume that hosts `path`.
    /// `GetVolumePathNameW` resolves the mount point (handles mounted folders, not just drive
    /// letters); `GetVolumeNameForVolumeMountPointW` maps it to `\\?\Volume{GUID}\`.
    fn volume_guid(path: &Path) -> Option<String> {
        let wide = wide(path.as_os_str().to_string_lossy().as_ref());
        let mut mount = vec![0u16; 260];
        unsafe {
            GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut mount).ok()?;
        }
        // Ensure a trailing separator — GetVolumeNameForVolumeMountPointW requires it.
        let mut mount = trim_wide(&mount);
        if mount.last() != Some(&('\\' as u16)) {
            mount.push('\\' as u16);
        }
        mount.push(0);
        let mut name = vec![0u16; 64];
        unsafe {
            GetVolumeNameForVolumeMountPointW(PCWSTR(mount.as_ptr()), &mut name).ok()?;
        }
        let name = String::from_utf16_lossy(&trim_wide(&name));
        let start = name.find('{')?;
        let end = name.find('}')?;
        Some(name[start..=end].to_string())
    }

    /// Read a `REG_DWORD` value, or `None` if the key/value is missing or the wrong type.
    fn read_dword(root: HKEY, subkey: &str, value: &str) -> Option<u32> {
        unsafe {
            let mut hkey = HKEY::default();
            let sub = wide(subkey);
            if RegOpenKeyExW(root, PCWSTR(sub.as_ptr()), None, KEY_READ, &mut hkey).is_err() {
                return None;
            }
            let val = wide(value);
            let mut data: u32 = 0;
            let mut size: u32 = 4;
            let mut kind = REG_VALUE_TYPE::default();
            let r = RegQueryValueExW(
                hkey,
                PCWSTR(val.as_ptr()),
                None,
                Some(&mut kind),
                Some(&mut data as *mut u32 as *mut u8),
                Some(&mut size),
            );
            let _ = RegCloseKey(hkey);
            (r.is_ok() && kind == REG_DWORD).then_some(data)
        }
    }

    /// A NUL-terminated UTF-16 buffer for a Win32 wide-string argument.
    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// A UTF-16 buffer truncated at its first NUL (the Win32 out-buffers are over-allocated).
    fn trim_wide(buf: &[u16]) -> Vec<u16> {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        buf[..end].to_vec()
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::recycle_decision;

    /// End-to-end round trip against the **real** Recycle Bin / Trash: recycle a throwaway temp
    /// file, confirm [`recycle`] reports it restorable, [`restore`] puts it back byte-for-byte,
    /// and the original path holds the same content again. `#[ignore]`d because it touches the OS
    /// trash (side effects, needs a real desktop session) — run with `cargo test -- --ignored`.
    /// Covers every platform's restore path (Windows/Linux `os_limited`, macOS `NSFileManager`).
    #[test]
    #[ignore = "touches the real OS Recycle Bin / Trash"]
    fn recycle_then_restore_round_trips_a_temp_file() {
        use super::{recycle, restore, RecycleOutcome};
        use std::fs;
        let dir = std::env::temp_dir().join(format!("pb_undelete_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("victim.txt");
        fs::write(&file, b"restore me").expect("seed");

        let handle = match recycle(&file).expect("recycle") {
            RecycleOutcome::Recycled(h) => h,
            RecycleOutcome::Permanent => panic!("a temp-drive delete should recycle, not nuke"),
        };
        assert!(!file.exists(), "the file left its original location");

        restore(handle).expect("restore from the trash");
        assert_eq!(
            fs::read(&file).expect("restored file readable"),
            b"restore me",
            "content survives the round trip"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The pure decision: recycle unless a policy or the volume explicitly nukes.
    #[cfg(windows)]
    #[test]
    fn recycle_decision_covers_the_policy_matrix() {
        // No signals at all → recycle (the Windows default).
        assert!(recycle_decision(None, None));
        // Volume opts out of the bin → permanent.
        assert!(!recycle_decision(None, Some(true)));
        // Volume explicitly uses the bin → recycle.
        assert!(recycle_decision(None, Some(false)));
        // Group policy nukes everything, regardless of the per-volume value.
        assert!(!recycle_decision(Some(true), None));
        assert!(!recycle_decision(Some(true), Some(false)));
        // Policy present but not nuking → defer to the volume.
        assert!(recycle_decision(Some(false), None));
        assert!(!recycle_decision(Some(false), Some(true)));
    }
}
