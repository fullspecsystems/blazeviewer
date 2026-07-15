import Foundation
import Sparkle

/// The Mac auto-updater (task #65) — a thin wrapper over Sparkle's `SPUStandardUpdaterController`.
///
/// Sparkle rides the existing notarized DMG on downloads.blazeviewer.app via an EdDSA-signed
/// `appcast.xml` (see the `SU*` keys in Info-swift-host.plist). This is the macOS half of the
/// same background-download / install-on-quit UX Windows already gets from Velopack
/// (`crates/pb-app/src/update.rs`); the two are independent (different tooling, different feed).
///
/// **Isolation.** Everything Sparkle touches lives in this one file plus the four Info.plist
/// keys — the rest of the host (the FFI, the core config) never sees it. In particular the
/// "Automatically check for updates" / "…download and install" preferences are owned by
/// Sparkle (it persists them in the app's `UserDefaults`), NOT by pb-app-core's `settings.toml`:
/// this is a Mac-only concern with its own storage, so routing it through the cross-platform
/// config would just create a second, conflicting source of truth.
///
/// **Gating.** The updater only *starts* (and the "Check for Updates…" menu item only appears)
/// when the running bundle actually carries a feed URL — i.e. a real assembled `.app`. A bare
/// `swift run` dev launch has no Info.plist feed, so it stays inert instead of logging feed
/// errors or offering a check that can't work.
@MainActor
final class Updater {
    static let shared = Updater()

    /// Whether this bundle is Sparkle-configured (a feed URL is present) — the assembled
    /// `.app`, not a bare-binary dev run. Gates both `startUpdater()` and the menu item.
    let isEnabled: Bool

    private let controller: SPUStandardUpdaterController

    private init() {
        isEnabled = Bundle.main.object(forInfoDictionaryKey: "SUFeedURL") != nil
        // `startingUpdater:` auto-starts the background scheduler when a feed exists. The
        // standard controller also wires `checkForUpdates(_:)` validation for the menu item.
        controller = SPUStandardUpdaterController(
            startingUpdater: isEnabled,
            updaterDelegate: nil,
            userDriverDelegate: nil
        )
    }

    /// Touch the singleton so the scheduler starts at launch (no-op call; the work is in `init`).
    func startIfEnabled() { _ = isEnabled }

    /// The target/action pair for a "Check for Updates…" menu item. Sparkle's controller
    /// implements `validateMenuItem(_:)`, so the item auto-disables while a check is running.
    var checkForUpdatesTarget: AnyObject { controller }
    var checkForUpdatesAction: Selector { #selector(SPUStandardUpdaterController.checkForUpdates(_:)) }

    // MARK: - Settings bindings (Sparkle owns the persistence)

    /// "Automatically check for updates" (Settings ▸ General ▸ Startup). Backed by Sparkle's
    /// `SUEnableAutomaticChecks` user default; setting it reschedules the background check.
    var automaticallyChecksForUpdates: Bool {
        get { controller.updater.automaticallyChecksForUpdates }
        set { controller.updater.automaticallyChecksForUpdates = newValue }
    }

    /// "Download and install automatically" — when off, Sparkle prompts before installing.
    /// Only meaningful while `automaticallyChecksForUpdates` is on.
    var automaticallyDownloadsUpdates: Bool {
        get { controller.updater.automaticallyDownloadsUpdates }
        set { controller.updater.automaticallyDownloadsUpdates = newValue }
    }
}
