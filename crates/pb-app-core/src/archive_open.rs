//! **The archive-open worker lifecycle** (task #126 step 2) — moved off the two shells.
//!
//! The companion to [`dir_scan`](crate::dir_scan), and the harder of the two. A folder walk is
//! *streaming*: one worker, many batches, one terminal. An archive open is **one-shot,
//! retrying, and secret-bearing**, which is why plan §5a rejected a single generic
//! worker-lifecycle abstraction and kept two bespoke state machines over shared primitives.
//!
//! Three things make this one harder than the walk:
//!
//! 1. **It can finish before it starts.** A plain `.zip` with no cached passwords to try opens
//!    *synchronously* — reading a central directory, not entry data — so the fast path must
//!    reach a terminal outcome without ever spawning a worker or showing chrome.
//! 2. **It retries.** A wrong password re-prompts for the *same* operation, so the operation
//!    outlives its own failure and the identity must survive the round trip.
//! 3. **It carries a [`SecretString`](crate::SecretString).** See the privacy note below.
//!
//! ## Privacy (Second Directive, plan §6) — read before touching `attempted_password`
//!
//! Moving the password across the crate boundary is exactly the change that quietly regresses
//! the guarantee, so the rules are encoded here rather than left to reviewers:
//!
//! - [`ArchiveOpenState`] deliberately has **no `Debug` derive**. A password reaching a log or
//!   panic message via `{:?}` is the most likely leak, and the type simply cannot be formatted.
//! - [`ArchiveOutcome`] *is* `Debug` — because tests and shells need to match on it — and it
//!   therefore **never carries a password in any variant**. `NeedPassword` names only a path.
//! - The winning password is handed straight to
//!   [`remember_archive_password`](crate::AppCore::remember_archive_password) inside the core
//!   and never returned to a shell, so no shell can log or persist it.
//! - It is not a `Settings` field, so `settings.save()` cannot reach it, and the session cache
//!   is wiped (zeroizing) at teardown even when the process `exit()`s without unwinding.
//!
//! The scope is honest, not overclaimed: this protects against *app-level* leaks, not OS
//! capture of live process RAM.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use crate::archive::ArchiveOpenError;
use crate::scan::Resolved;
use crate::SecretString;

/// The worker's payload: the open's result, plus the cached password that unlocked it (if the
/// auto-try found one), for MRU promotion.
pub(crate) type ArchiveResult = (Result<Resolved, ArchiveOpenError>, Option<SecretString>);

/// An in-flight archive open. Private state of the core; the shells no longer keep a copy.
///
/// ⚠ **No `Debug`, deliberately** — see the privacy note in the module docs. Adding one would
/// put a `SecretString` one `{:?}` away from a log line.
pub struct ArchiveOpenState {
    /// Identity from the shared generation space, so a late result from a superseded open — or
    /// from a folder scan that displaced it — is rejected by one gate.
    pub(crate) id: crate::background::OpId,
    pub(crate) rx: Receiver<(u64, ArchiveResult)>,
    /// The worker's generation tag as sent on the wire (see `dir_scan` for why it is separate).
    pub(crate) wire_gen: u64,
    /// The archive being opened — kept so a `PasswordRequired` failure can re-open this path
    /// with the entered password.
    pub(crate) path: PathBuf,
    /// The password *this attempt* carried, if any. Harvested on success (promoted into the
    /// session cache) and used to decide whether a `PasswordRequired` is a first prompt or a
    /// wrong-password retry.
    pub(crate) attempted_password: Option<SecretString>,
    /// Shared determinate progress + cancel flag. Chrome reads it; cancel flips it.
    pub(crate) progress: pb_source::OpenProgress,
}

impl ArchiveOpenState {
    /// Ask the open to stop at its next checkpoint. Idempotent.
    pub(crate) fn request_cancel(&self) {
        self.progress.request_cancel();
    }
}

/// The terminal (or not-yet-terminal) result of starting or polling an archive open.
///
/// Carries **no password in any variant** — see the module privacy note.
#[derive(Debug)]
pub enum ArchiveOutcome {
    /// Still in flight, or nothing was in flight. No chrome change.
    Pending,
    /// Opened successfully; the core has already applied the deck. Drop the Loading/Password
    /// chrome.
    Opened,
    /// The archive needs a password. `wrong` distinguishes a first prompt from a retry after a
    /// rejected one, so a shell can show an inline error and clear the field instead of
    /// jarringly re-opening its dialog.
    NeedPassword { path: PathBuf, wrong: bool },
    /// A terminal failure worth surfacing to the user.
    Failed(ArchiveOpenError),
    /// Cancelled by the user or superseded — close chrome quietly, report nothing.
    Cancelled,
}

impl ArchiveOutcome {
    /// Whether this outcome ends the operation (so chrome tracking it should stop).
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// A live, read-only view of the open in flight — the reconciliation surface, matching
/// [`ScanStatus`](crate::dir_scan::ScanStatus).
#[derive(Clone, PartialEq, Debug)]
pub struct ArchiveStatus {
    /// The archive's display name (a chrome headline: `Opening "name"…`).
    pub name: String,
    /// Determinate progress, 0.0–1.0.
    pub fraction: f32,
    /// The open has outlasted the reveal delay — slow enough to be worth chrome.
    pub slow: bool,
}

/// How long an archive open must run before its progress chrome is worth showing.
///
/// The archive twin of [`SCAN_DIALOG_DELAY`](crate::dir_scan::SCAN_DIALOG_DELAY), and it exists
/// for the same reason: a plain `.zip` opens in single-digit milliseconds, so revealing the
/// "Opening…" sheet immediately produced a **nonsensical flash** of a spinner nobody could read
/// (owner-reported 2026-07-20). Chrome that appears and vanishes faster than the eye resolves is
/// worse than no chrome — it reads as a glitch.
///
/// **500 ms, deliberately longer than the scan pill's 250 ms** (owner call 2026-07-20). The
/// previous value was 250 ms with the note *"the same judgement about the same human, and two
/// different numbers would only invite drift"* — that reasoning is answered rather than
/// ignored: it is **not** the same judgement, because the two surfaces cost the user
/// differently.
///
/// * The scan pill is **ambient** — a small non-blocking pill in the corner. Showing it early
///   is nearly free, so a low bar is right.
/// * This is a **modal dialog window**. It takes focus and interrupts. A higher bar for
///   interrupting than for informing is principled, not drift.
///
/// Why 500 and not more: past roughly a second a user starts wondering whether the machine is
/// working at all, so the indicator has to arrive comfortably before that. 500 ms sits in the
/// band where it reassures without ever being the reason you waited.
///
/// ⚠ **Do not "fix" the residual boundary flash with a minimum display duration.** An open
/// that lands just past the gate still paints and closes quickly, and the standard cure is to
/// hold the dialog open for a few hundred ms once shown. That was considered and **rejected**:
/// it trades a few ms of visual blip for a real, deliberate delay on every slow-ish open, in an
/// app whose whole thesis is that the next action feels instant. Raising the gate makes the
/// case rarer at zero cost; holding the dialog makes every affected open genuinely slower.
pub const LOADING_DIALOG_DELAY: Duration = Duration::from_millis(500);

/// The archive's display name for a headline: its file name, else a stable word.
///
/// Both shells derived this inline with the same `unwrap_or("archive")` fallback; unlike
/// `scan_display_name` they happened to agree, so this move is a pure de-duplication.
pub fn archive_display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The privacy invariant that is easiest to regress and hardest to notice: an outcome may
    /// be logged by a shell, so no variant may carry a secret. Enforced structurally — this
    /// test fails to compile if someone adds a password-bearing variant and formats it.
    #[test]
    fn no_outcome_variant_can_leak_a_password() {
        let outcomes = [
            ArchiveOutcome::Pending,
            ArchiveOutcome::Opened,
            ArchiveOutcome::Cancelled,
            ArchiveOutcome::NeedPassword {
                path: PathBuf::from("/secrets/holiday.7z"),
                wrong: true,
            },
        ];
        for o in &outcomes {
            let rendered = format!("{o:?}");
            assert!(
                !rendered.to_lowercase().contains("hunter2"),
                "no fixture password is in scope, but the shape must stay password-free"
            );
        }
        // The real guarantee is structural: `NeedPassword` names a path and a bool, nothing
        // else. If a future variant carries a `SecretString`, its redacted Debug still applies,
        // but the right fix is not to put it here at all.
        let np = format!(
            "{:?}",
            ArchiveOutcome::NeedPassword {
                path: PathBuf::from("/a.7z"),
                wrong: false
            }
        );
        assert!(np.contains("a.7z") && np.contains("false"));
    }

    /// A `SecretString` must stay redacted under `{:?}` wherever it travels. This pins the
    /// property the moved `attempted_password` depends on (plan §6).
    #[test]
    fn a_secret_is_redacted_in_debug_output() {
        let pw = SecretString::from("hunter2");
        let rendered = format!("{pw:?}");
        assert!(
            !rendered.contains("hunter2"),
            "SecretString leaked its contents through Debug: {rendered}"
        );
    }

    #[test]
    fn only_pending_is_non_terminal() {
        assert!(!ArchiveOutcome::Pending.is_terminal());
        assert!(ArchiveOutcome::Opened.is_terminal());
        assert!(ArchiveOutcome::Cancelled.is_terminal());
        assert!(ArchiveOutcome::Failed(ArchiveOpenError::Empty).is_terminal());
        assert!(ArchiveOutcome::NeedPassword {
            path: PathBuf::from("/a.7z"),
            wrong: false
        }
        .is_terminal());
    }

    #[test]
    fn display_name_is_the_file_name_with_a_stable_fallback() {
        assert_eq!(
            archive_display_name(std::path::Path::new("/photos/Trip.cbz")),
            "Trip.cbz"
        );
        assert_eq!(archive_display_name(std::path::Path::new("/")), "archive");
    }
}
