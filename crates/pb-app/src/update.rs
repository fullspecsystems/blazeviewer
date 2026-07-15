//! Auto-update surface. Three platform implementations sit behind **one four-function API** that
//! `main()` calls uniformly ([`velopack_startup`], [`start_background_check`], [`newly_ready`],
//! [`apply_on_quit`]); only the current target's module does real work:
//!
//!   * **Windows** ([`win`]) — Velopack: a per-user install with built-in background auto-update.
//!   * **Linux** ([`linux`]) — a JSON-feed *self-replace* updater for the AppImage: poll
//!     `latest.json`, download the newer AppImage, verify its sha256, then swap `$APPIMAGE` on quit
//!     (the next launch is the new version). The Windows/macOS mental model, ported to AppImage.
//!   * **macOS** ([`stub`]) — ships a notarized DMG with native **Sparkle** auto-update, which lives
//!     in the Swift host, not here — so these four calls are no-ops.
//!
//! Gating the heavy deps per target also keeps `velopack` (and the OpenSSL its `native-tls` chain
//! drags in) out of the non-Windows build, and `ureq`/`sha2` out of the non-Linux one.

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(not(any(windows, target_os = "linux")))]
pub use stub::*;
#[cfg(windows)]
pub use win::*;

/// No-op auto-update surface for macOS (DMG + native Sparkle) and any other non-Windows/non-Linux
/// target. Keeps `main()` platform-agnostic while the updater deps stay off those builds.
#[cfg(not(any(windows, target_os = "linux")))]
mod stub {
    pub fn velopack_startup() {}
    pub fn start_background_check() {}
    pub fn newly_ready() -> bool {
        false
    }
    pub fn apply_on_quit() {}
}

/// Pure feed model + version/arch selection — no I/O. Compiled on Linux (where the [`linux`] module
/// drives it) **and** under `cargo test` on any host, so the selection logic is unit-tested even on
/// the macOS dev box where the surrounding download/swap code doesn't compile.
#[cfg(any(target_os = "linux", test))]
mod feed {
    use serde::Deserialize;
    use std::collections::BTreeMap;

    /// The `latest.json` the release upload script (`scripts/release-linux-upload.sh`) publishes:
    /// the newest version plus a per-arch asset (url + sha256 + size). Unknown fields are ignored.
    #[derive(Deserialize)]
    pub struct Manifest {
        pub version: String,
        #[serde(default)]
        pub assets: BTreeMap<String, Asset>,
    }

    /// One downloadable AppImage for a given arch key (`"x86_64"` / `"aarch64"`, matching
    /// [`std::env::consts::ARCH`]).
    #[derive(Deserialize, Clone)]
    pub struct Asset {
        #[allow(dead_code)] // informational; the URL is what we fetch.
        pub file: String,
        pub url: String,
        pub sha256: String,
        #[serde(default)]
        #[allow(dead_code)]
        // reserved (progress UI / free-space check); not required to update.
        pub size: u64,
    }

    /// Parse a dotted version (`"0.1.1"`, `"1.2.3-beta.4"`) into a `(major, minor, patch)` triple,
    /// dropping any `-pre`/`+build` suffix. Missing or non-numeric components read as 0 — never
    /// panics on malformed input from the network.
    fn semver_triple(v: &str) -> (u64, u64, u64) {
        let core = v.trim().split(['-', '+']).next().unwrap_or("");
        let mut it = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
        (
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
        )
    }

    /// True iff `candidate` is a strictly newer release than `current` by numeric
    /// major.minor.patch. A pre-release suffix is ignored, so `0.1.2-beta.1` counts as newer than
    /// `0.1.1`, and `0.1.1-rc.1` is *not* newer than `0.1.1` (same core).
    pub fn is_newer(current: &str, candidate: &str) -> bool {
        semver_triple(candidate) > semver_triple(current)
    }

    /// The update asset for `arch`, but only if the manifest advertises a newer version than
    /// `current`. `None` when already up to date or the manifest lacks this arch.
    pub fn pick_update<'a>(m: &'a Manifest, arch: &str, current: &str) -> Option<&'a Asset> {
        if !is_newer(current, &m.version) {
            return None;
        }
        m.assets.get(arch)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn newer_detection() {
            assert!(is_newer("0.1.1", "0.1.2"));
            assert!(is_newer("0.1.1", "0.2.0"));
            assert!(is_newer("0.9.9", "1.0.0"));
            assert!(is_newer("0.1.9", "0.1.10")); // numeric, not lexical
            assert!(!is_newer("0.1.1", "0.1.1"));
            assert!(!is_newer("0.1.2", "0.1.1"));
            // A pre-release of a higher patch is still newer; a suffix on the same core is not.
            assert!(is_newer("0.1.1", "0.1.2-beta.1"));
            assert!(!is_newer("0.1.1", "0.1.1-beta.1"));
            // Garbage never panics and never falsely triggers an update.
            assert!(!is_newer("0.1.1", "not-a-version"));
        }

        fn manifest(json: &str) -> Manifest {
            serde_json::from_str(json).expect("manifest parses")
        }

        #[test]
        fn parses_real_manifest_shape() {
            // Exactly the schema scripts/release-linux-upload.sh writes.
            let m = manifest(
                r#"{
                  "product": "Blaze Viewer",
                  "version": "0.1.2",
                  "platform": "linux",
                  "assets": {
                    "x86_64":  { "file": "BlazeViewer-0.1.2-x86_64.AppImage",  "url": "https://x/a", "sha256": "aa", "size": 1 },
                    "aarch64": { "file": "BlazeViewer-0.1.2-aarch64.AppImage", "url": "https://x/b", "sha256": "bb", "size": 2 }
                  }
                }"#,
            );
            assert_eq!(m.version, "0.1.2");
            assert_eq!(m.assets["x86_64"].url, "https://x/a");
            assert_eq!(m.assets["aarch64"].sha256, "bb");
        }

        #[test]
        fn pick_requires_newer_and_matching_arch() {
            let m = manifest(
                r#"{ "version": "0.1.2", "assets": {
                    "x86_64": { "file": "f", "url": "u", "sha256": "s", "size": 1 } } }"#,
            );
            // Newer + arch present → picked.
            assert_eq!(
                pick_update(&m, "x86_64", "0.1.1").map(|a| a.url.as_str()),
                Some("u")
            );
            // Newer but arch absent → none.
            assert!(pick_update(&m, "aarch64", "0.1.1").is_none());
            // Same version → none even though the arch is present.
            assert!(pick_update(&m, "x86_64", "0.1.2").is_none());
            // Older feed than us → none.
            assert!(pick_update(&m, "x86_64", "0.2.0").is_none());
        }
    }
}

/// Linux AppImage self-replace updater. Polls the JSON feed, downloads + verifies the newer
/// AppImage in the background, and swaps `$APPIMAGE` on quit (mirroring the Windows "installs when
/// you quit" flow).
#[cfg(target_os = "linux")]
mod linux {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use sha2::{Digest, Sha256};

    use super::feed::{pick_update, Manifest};

    /// The public Linux release feed: the flat directory holding `latest.json` + the versioned
    /// AppImages, published by `scripts/release-linux-upload.sh`. `PB_UPDATE_FEED` overrides the
    /// base URL for testing an update loop against a local HTTP server.
    const FEED_BASE: &str = "https://downloads.blazeviewer.app/linux";

    /// A verified, executable AppImage downloaded this session, waiting to replace `$APPIMAGE` on
    /// quit. `None` until a background download completes and passes its checksum.
    struct Staged {
        tmp: PathBuf,
        target: PathBuf,
    }
    static STAGED: OnceLock<Mutex<Option<Staged>>> = OnceLock::new();
    /// Set once the "update ready" toast has fired, so [`newly_ready`] returns true exactly once.
    static TOASTED: AtomicBool = AtomicBool::new(false);

    fn staged() -> &'static Mutex<Option<Staged>> {
        STAGED.get_or_init(|| Mutex::new(None))
    }

    /// No install/uninstall lifecycle hooks on Linux (that's Velopack's job on Windows); present so
    /// `main()` can call the same four functions on every platform.
    pub fn velopack_startup() {}

    /// Spawn the background check + download. Cheap, non-blocking, safe on every launch: a raw
    /// `cargo run` (no `$APPIMAGE`), being offline, or already being up to date all just exit the
    /// thread quietly. Updates must never interrupt viewing, so every failure stays silent in
    /// release (a stderr breadcrumb helps when launched from a terminal).
    pub fn start_background_check() {
        let _ = std::thread::Builder::new()
            .name("pb-update-check".into())
            .spawn(|| {
                if let Err(e) = check_and_stage() {
                    eprintln!("{}: update check skipped: {e}", pb_app_core::APP_NAME);
                }
            });
    }

    /// Fetch the manifest and, if it advertises a newer build for this arch, download it next to
    /// `$APPIMAGE`, verify its sha256, mark it executable, and stage it for the on-quit swap.
    fn check_and_stage() -> Result<(), String> {
        // Self-gate: only a real AppImage launch sets `$APPIMAGE` (to the .AppImage path). Without
        // it there's nothing to replace — a dev build / extracted run — so do nothing (the Linux
        // analogue of Velopack failing to construct on a non-installed Windows build).
        let target = std::env::var_os("APPIMAGE")
            .map(PathBuf::from)
            .ok_or("not running as an AppImage ($APPIMAGE unset)")?;

        let base = std::env::var("PB_UPDATE_FEED").unwrap_or_else(|_| FEED_BASE.to_string());
        let manifest_url = format!("{}/latest.json", base.trim_end_matches('/'));

        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_connect(Some(Duration::from_secs(10)))
                .timeout_global(Some(Duration::from_secs(600)))
                .build(),
        );

        let mut resp = agent.get(&manifest_url).call().map_err(|e| e.to_string())?;
        let manifest: Manifest = resp.body_mut().read_json().map_err(|e| e.to_string())?;

        let current = env!("CARGO_PKG_VERSION");
        let arch = std::env::consts::ARCH; // "x86_64" | "aarch64" — matches the manifest keys.
        let Some(asset) = pick_update(&manifest, arch, current) else {
            return Ok(()); // up to date, or no build for this arch
        };

        // We self-replace by renaming a sibling temp over `$APPIMAGE` (atomic — same directory, so
        // same filesystem). That needs the AppImage's directory to be writable; if it isn't (e.g.
        // installed to /opt as root), we simply don't stage — the user can re-download from the
        // feed. (A toast-based "update available" notify for the read-only case is a future add.)
        let dir = target.parent().ok_or("$APPIMAGE has no parent directory")?;
        let tmp = dir.join(format!(
            ".photoblaze-update-{}.AppImage.part",
            manifest.version
        ));

        let staged_result = (|| -> Result<(), String> {
            download_to(&agent, &asset.url, &tmp)?;
            let got = sha256_hex(&tmp)?;
            if !got.eq_ignore_ascii_case(&asset.sha256) {
                return Err(format!(
                    "sha256 mismatch (expected {}, got {got}) — discarding",
                    asset.sha256
                ));
            }
            set_executable(&tmp)?;
            Ok(())
        })();

        if let Err(e) = staged_result {
            let _ = std::fs::remove_file(&tmp); // never leave a partial/corrupt file behind
            return Err(e);
        }

        *staged().lock().unwrap() = Some(Staged { tmp, target });
        eprintln!(
            "{}: update {} downloaded; it installs when you quit.",
            pb_app_core::APP_NAME,
            manifest.version
        );
        Ok(())
    }

    /// Stream a URL to a file (the AppImage is ~75 MB, so don't buffer it in RAM).
    fn download_to(agent: &ureq::Agent, url: &str, dest: &Path) -> Result<(), String> {
        let mut resp = agent.get(url).call().map_err(|e| e.to_string())?;
        let mut reader = resp.body_mut().as_reader();
        let mut file =
            std::fs::File::create(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
        std::io::copy(&mut reader, &mut file).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Hex-encoded SHA-256 of a file, streamed in 64 KiB chunks.
    fn sha256_hex(path: &Path) -> Result<String, String> {
        let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        for b in digest {
            use std::fmt::Write;
            let _ = write!(hex, "{b:02x}");
        }
        Ok(hex)
    }

    /// Mark the downloaded file 0755 so the swapped-in AppImage is runnable.
    fn set_executable(path: &Path) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(path)
            .map_err(|e| e.to_string())?
            .permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(path, perm).map_err(|e| e.to_string())
    }

    /// Main-thread poll: `true` exactly once, when a background download has finished staging. The
    /// shell shows the "Update ready. It installs when you quit." toast in response.
    pub fn newly_ready() -> bool {
        if TOASTED.load(Ordering::Relaxed) {
            return false;
        }
        if staged().lock().unwrap().is_some() {
            TOASTED.store(true, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// If an update was staged this session, swap it in now (called from the quit path). The rename
    /// is atomic (same directory). The running process keeps its own open fd / FUSE mount, so
    /// replacing the file's path is safe; the *next* launch of the AppImage is the new version. On
    /// failure the old AppImage is left untouched.
    pub fn apply_on_quit() {
        let Some(s) = staged().lock().unwrap().take() else {
            return;
        };
        if let Err(e) = std::fs::rename(&s.tmp, &s.target) {
            eprintln!(
                "{}: applying the downloaded update failed: {e}",
                pb_app_core::APP_NAME
            );
            let _ = std::fs::remove_file(&s.tmp);
        }
    }
}

#[cfg(windows)]
mod win {
    //! Velopack: per-user installer lifecycle hooks + background auto-update.
    //!
    //! PhotoBlaze ships on Windows as a per-user Velopack install (replacing the old WiX/MSI):
    //! a ~5 s, no-UAC install with built-in auto-update. This module is the app-side glue.
    //!
    //!   * [`velopack_startup`] runs **first thing in `main()`**. Velopack invokes the app with a
    //!     lifecycle arg (`--veloapp-install` / `-updated` / `-uninstall` / `-obsolete`) during
    //!     install / update / uninstall; the Rust binding's `VelopackApp::run()` only *exits* on
    //!     these, so we intercept them to (un)register PhotoBlaze's per-user (HKCU) file
    //!     associations (see [`crate::default_app::register_shell_integration`]). A no-op for a
    //!     normal launch, so `cargo run` and a not-yet-installed build behave as before.
    //!   * [`start_background_check`] spawns a low-priority thread that checks the release feed and,
    //!     if a newer version exists, downloads it in the background. Self-gating: on a dev build /
    //!     non-Velopack install `UpdateManager::new` fails locally (no network) and the thread exits.
    //!   * [`newly_ready`] is a main-thread poll: true exactly once, when a download has finished
    //!     staging → the shell shows a toast.
    //!   * [`apply_on_quit`] is called from `begin_exit`: if an update is staged it installs it (the
    //!     next launch is the new version) and exits; otherwise it returns and quit proceeds.
    //!
    //! **Update source.** The Rust binding has no `GithubSource` (only `HttpSource` / `FileSource`),
    //! so the app reads the feed over plain HTTP from a **flat directory** ([`FEED_URL`]) holding
    //! `releases.win.json` (the index) + the `.nupkg` packages — the native `HttpSource` layout. We
    //! host that directory on our own web space (a DigitalOcean droplet / Space) rather than GitHub
    //! Releases, whose per-release asset layout `HttpSource` can't walk (a flat dir also keeps delta
    //! updates working). `PB_UPDATE_FEED` overrides the source with a local `FileSource` directory,
    //! for testing an update loop offline (see scripts/spike-velopack.ps1).

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};

    use velopack::sources::{FileSource, HttpSource, UpdateSource};
    use velopack::{UpdateInfo, UpdateManager, VelopackApp};

    /// The release feed: a flat HTTP directory holding `releases.win.json` + the `.nupkg` packages.
    /// `HttpSource` fetches `<FEED_URL>/releases.win.json` and the referenced `.nupkg` from here.
    ///
    /// Hosted on a Caddy static server (DigitalOcean) behind a domain **we own**, which is the
    /// whole point: this URL is compiled into every installed binary and can never move without
    /// orphaning that install, so moving the *bytes* elsewhere later (e.g. GitHub Releases for
    /// free bandwidth) must stay a server-side config change. No product namespace — the domain
    /// is the product (task #101; it was `downloads.fullspec.ca/photoblaze/win`).
    ///
    /// Until the first release is uploaded, an update check simply finds nothing (a failed
    /// lookup is silent).
    const FEED_URL: &str = "https://downloads.blazeviewer.app/win";

    /// A downloaded, staged update waiting to be applied on quit (`None` until a download completes).
    static STAGED: OnceLock<Mutex<Option<UpdateInfo>>> = OnceLock::new();
    /// Set once the "update ready" toast has been shown, so [`newly_ready`] fires exactly once.
    static TOASTED: AtomicBool = AtomicBool::new(false);

    fn staged() -> &'static Mutex<Option<UpdateInfo>> {
        STAGED.get_or_init(|| Mutex::new(None))
    }

    /// Run Velopack's startup logic. **Must be the first thing in `main()`.** On an install / update /
    /// uninstall hook invocation it (un)registers the file associations and exits; on a normal launch
    /// it's a no-op.
    pub fn velopack_startup() {
        // Velopack runs the app with a lifecycle arg during install / update / uninstall, expecting
        // per-app setup then an exit. We intercept them to (un)register the per-user associations —
        // real install AND uninstall hooks out of the minimal binding.
        for arg in std::env::args() {
            match arg.to_ascii_lowercase().as_str() {
                // Fresh install or in-place update: (re)write associations → the current exe.
                "--veloapp-install" | "--veloapp-updated" => {
                    crate::default_app::register_shell_integration();
                    std::process::exit(0);
                }
                // Uninstall: remove the HKCU keys we added (leave other apps' entries intact).
                "--veloapp-uninstall" => {
                    crate::default_app::unregister_shell_integration();
                    std::process::exit(0);
                }
                // A superseded version being cleaned up: nothing app-specific to do.
                "--veloapp-obsolete" => std::process::exit(0),
                _ => {}
            }
        }

        VelopackApp::build().run();
    }

    /// Spawn the background update check + download. Cheap and non-blocking; returns immediately.
    /// Safe to call on every launch — a dev build / non-Velopack install fails `UpdateManager::new`
    /// locally (before any network) and the thread exits.
    pub fn start_background_check() {
        let _ = std::thread::Builder::new()
            .name("pb-update-check".into())
            .spawn(|| {
                // `PB_UPDATE_FEED` (a local folder) overrides the GitHub feed for offline testing.
                let checked = match std::env::var("PB_UPDATE_FEED") {
                    Ok(dir) => check_and_download(FileSource::new(dir)),
                    Err(_) => check_and_download(HttpSource::new(FEED_URL)),
                };
                if let Ok(Some(update)) = checked {
                    *staged().lock().unwrap() = Some(update);
                    eprintln!(
                        "{}: update downloaded; it installs when you quit.",
                        pb_app_core::APP_NAME
                    );
                }
                // No update, or any error (offline, private feed, not a Velopack install): stay quiet.
                // Updates must never interrupt viewing.
            });
    }

    /// Check the feed and, if a newer version is available, download it. Returns the staged asset.
    fn check_and_download<S: UpdateSource>(source: S) -> Result<Option<UpdateInfo>, String> {
        let um = UpdateManager::new(source, None).map_err(|e| e.to_string())?;
        match um.check_for_updates().map_err(|e| e.to_string())? {
            Some(info) => {
                um.download_updates(&info, |_| {})
                    .map_err(|e| e.to_string())?;
                Ok(Some(info))
            }
            None => Ok(None),
        }
    }

    /// Main-thread poll: `true` exactly once, when a background download has finished staging an
    /// update. The shell shows a toast in response.
    pub fn newly_ready() -> bool {
        if TOASTED.load(Ordering::Relaxed) {
            return false;
        }
        if staged().lock().unwrap().is_some() {
            TOASTED.store(true, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// If an update was downloaded this session, install it now (the next launch is the new version)
    /// and exit the process. A no-op if nothing is staged. Called from the quit path (`begin_exit`).
    pub fn apply_on_quit() {
        let Some(update) = staged().lock().unwrap().take() else {
            return;
        };
        // Recreate a manager (local — the package is already downloaded, so no network) and apply.
        // On success this exits the process; the update lands and the next launch is the new version.
        let result = match std::env::var("PB_UPDATE_FEED") {
            Ok(dir) => apply(FileSource::new(dir), &update),
            Err(_) => apply(HttpSource::new(FEED_URL), &update),
        };
        if let Err(e) = result {
            eprintln!(
                "{}: applying the downloaded update failed: {e}",
                pb_app_core::APP_NAME
            );
        }
    }

    fn apply<S: UpdateSource>(source: S, update: &UpdateInfo) -> Result<(), String> {
        let um = UpdateManager::new(source, None).map_err(|e| e.to_string())?;
        um.apply_updates_and_exit(update).map_err(|e| e.to_string())
    }
}
