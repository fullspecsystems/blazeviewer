//! "Set as default photo viewer" (task #14) — open PhotoBlaze's **own** page in
//! Windows Settings ▸ Default apps, not the generic list.
//!
//! Windows only gives an app its own Settings page when it's registered under
//! `RegisteredApplications` with a `Capabilities` key naming its file associations;
//! the `ms-settings:defaultapps?registeredApp{Machine,User}=<name>` URI then
//! deep-links straight to that page. The MSI writes the machine-wide (HKLM)
//! registration; installs that predate it (or a portable exe) get a **per-user
//! (HKCU) self-registration** written on demand when the button is clicked — an
//! explicit user command configuring their own machine, the same category as the
//! MSI's registry writes (privacy #2 is about viewing traces, not setup; ADR-018's
//! "never seize the default" still holds — the page only *offers* the switch).
//!
//! Best-effort throughout: any failure falls back to the generic Default-apps page,
//! which is where the old button always went.

/// The name PhotoBlaze registers under (`RegisteredApplications` value name and the
/// `ms-settings` URI parameter). Must match the MSI's `AppRegistration` component.
#[cfg(windows)]
const APP_NAME: &str = "PhotoBlaze";

/// Open Settings ▸ Default apps, on PhotoBlaze's own page when possible.
pub fn open_default_apps() {
    #[cfg(windows)]
    {
        let uri = if win::machine_registered() {
            format!("ms-settings:defaultapps?registeredAppMachine={APP_NAME}")
        } else if win::ensure_user_registration().is_ok() {
            format!("ms-settings:defaultapps?registeredAppUser={APP_NAME}")
        } else {
            "ms-settings:defaultapps".to_string()
        };
        // `explorer.exe` resolves the `ms-settings:` protocol; spawn-and-forget.
        let _ = std::process::Command::new("explorer.exe").arg(uri).spawn();
    }
}

/// Register PhotoBlaze's per-user (HKCU) shell integration — the `PhotoBlaze.Image` /
/// `PhotoBlaze.Archive` ProgIds (icon + open command → the running exe), the
/// per-extension `OpenWithProgids` candidacy, the folder right-click verb, and the
/// `Capabilities` + `RegisteredApplications` entries. This is the per-user twin of the
/// MSI's HKLM/HKCR registration in `wix/main.wxs`, for the Velopack per-user install: it
/// points the open command at [`std::env::current_exe`] (Velopack's stable
/// `…\current\photoblaze.exe`). Called from the Velopack `--veloapp-install`/`-updated`
/// hook. Best-effort — failures are logged, not fatal. Like the MSI it never *seizes* a
/// default (ADR-018).
#[cfg(windows)]
pub fn register_shell_integration() {
    if let Err(e) = win::register_shell_integration() {
        eprintln!("PhotoBlaze: file-association registration failed: {e:?}");
    }
}

/// Remove the per-user shell integration written by [`register_shell_integration`] —
/// the Velopack `--veloapp-uninstall` hook, so uninstall leaves no orphaned HKCU keys.
/// Best-effort: missing keys are ignored, and only *our* entries are pulled from shared
/// keys (each extension's `OpenWithProgids`, `RegisteredApplications`).
#[cfg(windows)]
pub fn unregister_shell_integration() {
    win::unregister_shell_integration();
}

#[cfg(windows)]
mod win {
    use windows::core::{w, PCWSTR};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    // Registry deletion is used only by the Velopack `--veloapp-uninstall` cleanup.
    use windows::Win32::System::Registry::{RegDeleteKeyValueW, RegDeleteTreeW};

    /// The image extensions the viewer handles, each mapped to the MSI's
    /// `PhotoBlaze.Image` ProgId on the Capabilities page. Mirrors the MSI's
    /// `Associations` component — keep the two lists in sync.
    const IMAGE_EXTS: &[&str] = &[
        ".jpg", ".jpeg", ".jpe", ".jfif", ".png", ".gif", ".bmp", ".tif", ".tiff", ".webp",
        ".heic", ".heif", ".avif", ".jxl",
    ];
    /// Browseable archives → the `PhotoBlaze.Archive` ProgId (never the default
    /// out of the box; the Settings page just offers the option).
    const ARCHIVE_EXTS: &[&str] = &[".zip", ".7z"];
    /// Video containers the library lists (task #79) → the `PhotoBlaze.Video` ProgId.
    /// Open-With candidacy only, never the default (ADR-018) — PhotoBlaze must not
    /// steal video files from the user's actual video player. Mirrors the one
    /// cross-platform recognition list (`pb_app_core::video::VideoContainer`).
    const VIDEO_EXTS: &[&str] = &[
        ".mp4", ".m4v", ".mov", ".qt", ".mkv", ".webm", ".avi", ".wmv", ".asf", ".mpg", ".mpeg",
        ".mts", ".m2ts", ".3gp", ".3g2",
    ];

    /// An owned `HKEY` that always closes (registry handles leak silently otherwise).
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    /// UTF-16 (NUL-terminated) bytes of `s`, as the `REG_SZ` byte payload.
    fn utf16_bytes(s: &str) -> Vec<u8> {
        s.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(|c| c.to_le_bytes())
            .collect()
    }

    fn set_string(key: HKEY, name: PCWSTR, value: &str) -> windows::core::Result<()> {
        unsafe { RegSetValueExW(key, name, None, REG_SZ, Some(&utf16_bytes(value))).ok() }
    }

    fn create_key(root: HKEY, path: PCWSTR) -> windows::core::Result<Key> {
        let mut key = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                root,
                path,
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
            .ok()?;
        }
        Ok(Key(key))
    }

    /// Whether the MSI's machine-wide registration is present (an HKLM
    /// `RegisteredApplications` value named "PhotoBlaze").
    pub fn machine_registered() -> bool {
        unsafe {
            let mut key = HKEY::default();
            if RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                w!("SOFTWARE\\RegisteredApplications"),
                None,
                KEY_QUERY_VALUE,
                &mut key,
            )
            .is_err()
            {
                return false;
            }
            let key = Key(key);
            RegQueryValueExW(key.0, w!("PhotoBlaze"), None, None, None, None).is_ok()
        }
    }

    /// Write the per-user (HKCU) registration: a `Capabilities` key naming the app +
    /// its file associations, and the `RegisteredApplications` pointer to it.
    /// Idempotent — re-running just rewrites the same values.
    pub fn ensure_user_registration() -> windows::core::Result<()> {
        let caps = create_key(HKEY_CURRENT_USER, w!("SOFTWARE\\PhotoBlaze\\Capabilities"))?;
        set_string(caps.0, w!("ApplicationName"), super::APP_NAME)?;
        set_string(caps.0, w!("ApplicationDescription"), pb_app_core::TAGLINE)?;

        let assoc = create_key(
            HKEY_CURRENT_USER,
            w!("SOFTWARE\\PhotoBlaze\\Capabilities\\FileAssociations"),
        )?;
        for (exts, progid) in [
            (IMAGE_EXTS, "PhotoBlaze.Image"),
            (ARCHIVE_EXTS, "PhotoBlaze.Archive"),
            (VIDEO_EXTS, "PhotoBlaze.Video"),
        ] {
            for ext in exts {
                let name: Vec<u16> = ext.encode_utf16().chain(std::iter::once(0)).collect();
                set_string(assoc.0, PCWSTR(name.as_ptr()), progid)?;
            }
        }

        let registered = create_key(HKEY_CURRENT_USER, w!("SOFTWARE\\RegisteredApplications"))?;
        set_string(
            registered.0,
            w!("PhotoBlaze"),
            "SOFTWARE\\PhotoBlaze\\Capabilities",
        )
    }

    /// UTF-16 (NUL-terminated) buffer for a runtime-built key path or value name.
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// `create_key` for a runtime-built (`format!`) path. The wide buffer only has to
    /// outlive the `RegCreateKeyExW` call (which copies the path), so binding it here is
    /// safe — the returned `Key` owns the opened handle.
    fn create_key_str(root: HKEY, path: &str) -> windows::core::Result<Key> {
        create_key(root, PCWSTR(wide(path).as_ptr()))
    }

    /// Set a key's **default** (unnamed) value — the ProgId label, icon, and command.
    fn set_default(key: HKEY, value: &str) -> windows::core::Result<()> {
        set_string(key, PCWSTR::null(), value)
    }

    /// Write the full per-user (HKCU) shell integration: ProgIds + open command + icon,
    /// per-extension `OpenWithProgids`, the folder verb, and the Default-apps
    /// registration. Mirrors `wix/main.wxs` under `HKCU\Software\Classes`, pointing the
    /// commands at the running exe (Velopack's stable `…\current\photoblaze.exe`).
    /// Idempotent — re-running just rewrites the same values.
    pub fn register_shell_integration() -> windows::core::Result<()> {
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let icon = format!("\"{exe}\",0");
        let open_cmd = format!("\"{exe}\" \"%1\"");
        let folder_cmd = format!("\"{exe}\" \"%V\"");

        // ── ProgIds: label + FriendlyTypeName + DefaultIcon + shell\open\command.
        for (progid, label) in [
            ("PhotoBlaze.Image", "PhotoBlaze Image"),
            ("PhotoBlaze.Archive", "PhotoBlaze Archive"),
            ("PhotoBlaze.Video", "PhotoBlaze Video"),
        ] {
            let base = format!("Software\\Classes\\{progid}");
            let k = create_key_str(HKEY_CURRENT_USER, &base)?;
            set_default(k.0, label)?;
            set_string(k.0, w!("FriendlyTypeName"), label)?;
            set_default(
                create_key_str(HKEY_CURRENT_USER, &format!("{base}\\DefaultIcon"))?.0,
                &icon,
            )?;
            set_default(
                create_key_str(HKEY_CURRENT_USER, &format!("{base}\\shell\\open\\command"))?.0,
                &open_cmd,
            )?;
        }

        // ── Candidate "Open with" associations (never the default; ADR-018): list the
        // ProgId under each extension's OpenWithProgids (value name = ProgId, empty data).
        for (exts, progid) in [
            (IMAGE_EXTS, "PhotoBlaze.Image"),
            (ARCHIVE_EXTS, "PhotoBlaze.Archive"),
            (VIDEO_EXTS, "PhotoBlaze.Video"),
        ] {
            for ext in exts {
                let k = create_key_str(
                    HKEY_CURRENT_USER,
                    &format!("Software\\Classes\\{ext}\\OpenWithProgids"),
                )?;
                let name = wide(progid);
                set_string(k.0, PCWSTR(name.as_ptr()), "")?;
            }
        }

        // ── Folder right-click verb ("Open with PhotoBlaze" → browse recursively). %V is
        // the folder path; also on the folder background (right-click empty space).
        for base in [
            "Software\\Classes\\Directory\\shell\\PhotoBlaze",
            "Software\\Classes\\Directory\\Background\\shell\\PhotoBlaze",
        ] {
            let k = create_key_str(HKEY_CURRENT_USER, base)?;
            set_default(k.0, "Open with PhotoBlaze")?;
            set_string(k.0, w!("Icon"), &icon)?;
            set_default(
                create_key_str(HKEY_CURRENT_USER, &format!("{base}\\command"))?.0,
                &folder_cmd,
            )?;
        }

        // ── Capabilities + RegisteredApplications → PhotoBlaze's own Default-apps page.
        ensure_user_registration()
    }

    /// Delete a key and all its subkeys (best-effort; a missing key is fine).
    fn delete_tree(root: HKEY, path: &str) {
        unsafe {
            let _ = RegDeleteTreeW(root, PCWSTR(wide(path).as_ptr()));
        }
    }

    /// Delete a single named value from a subkey (best-effort).
    fn delete_value(root: HKEY, subkey: &str, name: &str) {
        let subkey = wide(subkey);
        let name = wide(name);
        unsafe {
            let _ = RegDeleteKeyValueW(root, PCWSTR(subkey.as_ptr()), PCWSTR(name.as_ptr()));
        }
    }

    /// Reverse of [`register_shell_integration`]: drop the ProgIds, folder verb, and
    /// Capabilities tree, and pull our ProgId out of each extension's `OpenWithProgids`
    /// and out of `RegisteredApplications` — leaving other apps' entries in those shared
    /// keys untouched.
    pub fn unregister_shell_integration() {
        for progid in ["PhotoBlaze.Image", "PhotoBlaze.Archive", "PhotoBlaze.Video"] {
            delete_tree(HKEY_CURRENT_USER, &format!("Software\\Classes\\{progid}"));
        }
        for (exts, progid) in [
            (IMAGE_EXTS, "PhotoBlaze.Image"),
            (ARCHIVE_EXTS, "PhotoBlaze.Archive"),
            (VIDEO_EXTS, "PhotoBlaze.Video"),
        ] {
            for ext in exts {
                delete_value(
                    HKEY_CURRENT_USER,
                    &format!("Software\\Classes\\{ext}\\OpenWithProgids"),
                    progid,
                );
            }
        }
        delete_tree(
            HKEY_CURRENT_USER,
            "Software\\Classes\\Directory\\shell\\PhotoBlaze",
        );
        delete_tree(
            HKEY_CURRENT_USER,
            "Software\\Classes\\Directory\\Background\\shell\\PhotoBlaze",
        );
        delete_tree(HKEY_CURRENT_USER, "Software\\PhotoBlaze");
        delete_value(
            HKEY_CURRENT_USER,
            "Software\\RegisteredApplications",
            "PhotoBlaze",
        );
    }
}
