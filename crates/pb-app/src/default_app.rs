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

#[cfg(windows)]
mod win {
    use windows::core::{w, PCWSTR};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };

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
        set_string(
            caps.0,
            w!("ApplicationDescription"),
            "A fast, keyboard-driven photo viewer.",
        )?;

        let assoc = create_key(
            HKEY_CURRENT_USER,
            w!("SOFTWARE\\PhotoBlaze\\Capabilities\\FileAssociations"),
        )?;
        for (exts, progid) in [
            (IMAGE_EXTS, "PhotoBlaze.Image"),
            (ARCHIVE_EXTS, "PhotoBlaze.Archive"),
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
}
