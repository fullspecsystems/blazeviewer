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
use velopack::{UpdateManager, VelopackApp, UpdateInfo};

/// The release feed: a flat HTTP directory holding `releases.win.json` + the `.nupkg` packages.
/// `HttpSource` fetches `<FEED_URL>/releases.win.json` and the referenced `.nupkg` from here.
/// Hosted on a Caddy static server (DigitalOcean, `downloads.fullspec.ca`); product-namespaced so
/// the same host can serve other apps / a future macOS channel (`…/photoblaze/osx`). Until the
/// first release is uploaded, an update check simply finds nothing (a failed lookup is silent).
const FEED_URL: &str = "https://downloads.fullspec.ca/photoblaze/win";

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
    #[cfg(windows)]
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
                eprintln!("PhotoBlaze: update downloaded; it installs when you quit.");
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
        eprintln!("PhotoBlaze: applying the downloaded update failed: {e}");
    }
}

fn apply<S: UpdateSource>(source: S, update: &UpdateInfo) -> Result<(), String> {
    let um = UpdateManager::new(source, None).map_err(|e| e.to_string())?;
    um.apply_updates_and_exit(update).map_err(|e| e.to_string())
}
