//! **The directory-scan worker lifecycle** (task #126 step 1) — moved off the two shells.
//!
//! Both shells hand-maintained a byte-similar copy of this: spawn a walk, pump its channel,
//! supersede an older walk by generation, cancel it, and reveal a progress dialog only once
//! the walk is slow enough to notice. The *logic* was always shell-neutral — the winit and
//! macOS `struct DirScan`s are field-for-field identical, and every type involved
//! ([`ScanProgress`], [`Receiver`], [`Instant`], [`PathBuf`]) is platform-neutral. Only the
//! **dialog realisation** is genuinely per-platform, so only that stays in the shell.
//!
//! ## The testability seam is the channel, not a runtime trait
//!
//! Codex round 1 asked for an injectable worker runtime so "cancel mid-walk" and "teardown
//! in flight" get deterministic completion points. They already have one: the worker
//! communicates *only* over an `mpsc` channel, so [`DirScanState::armed`] installs a
//! caller-supplied receiver and a test simply drives the sender itself — no thread, no
//! sleeps, no new abstraction. [`super::AppCore::begin_dir_scan`] is the thin production
//! wrapper that makes the channel and spawns the walk.
//!
//! ## What this module does NOT own
//!
//! Cancelling a superseded **archive open** is the shell's job *for now*, because that
//! worker still lives there (step 2 moves it). The policy is already here though:
//! [`BackgroundOps`](crate::background::BackgroundOps) decides *that* something was
//! superseded and hands it back, so the shell only performs the mechanism. That is the
//! interim split Codex's phase-0 note calls for — the invariant has one owner even while
//! the two workers do not.

use std::sync::mpsc::Receiver;
use std::time::Duration;

use crate::scan::{ScanProgress, ScanUpdate};

/// How long a walk must run before its progress dialog is worth showing. A normal folder
/// resolves in milliseconds and must never flash one.
///
/// Previously declared **independently in both shells** (`pb-app/src/main.rs` and
/// `pb-mac-ffi/src/lib.rs:44`), each at 250 ms — same value, two sources of truth. This is
/// now the only one.
pub const SCAN_DIALOG_DELAY: Duration = Duration::from_millis(250);

/// The scanned folder's display name: the first root's folder name.
///
/// Both shells derived this independently and **disagreed on the fallback** — winit's
/// `scan_display_name` returned the literal `"folder"` for a root with no file name (`/`, or a
/// bare drive letter), while the macOS copy fell through to `root.display()`, printing a raw
/// path into a headline. winit's is adopted: a headline reading *Scanning "folder"…* is worse
/// than a name but better than *Scanning "C:\"…*, and neither shell should be choosing.
pub fn scan_display_name(roots: &[std::path::PathBuf]) -> String {
    roots
        .first()
        .and_then(|r| r.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "folder".to_string())
}

/// An in-flight directory walk. Private state of the core; the shells no longer keep a copy.
pub struct DirScanState {
    /// Identity from the shared generation space, so a late batch from a superseded walk —
    /// or from an archive open — is rejected by one gate.
    pub(crate) id: crate::background::OpId,
    pub(crate) rx: Receiver<(u64, ScanUpdate)>,
    /// The worker's generation tag, as sent on the wire. Kept separate from [`Self::id`]
    /// because it is what `scan::stream_scan` stamps on each update.
    pub(crate) wire_gen: u64,
    /// Shared found-count / current-folder / cancel flag. The dialog reads it; cancel flips it.
    pub(crate) progress: ScanProgress,
    /// Folder name for the dialog headline ("Scanning "name"…").
    pub(crate) name: String,
}

// NOTE: there is deliberately no `started` here. Both shells kept one, but the
// reveal-after-delay clock now belongs to `BackgroundOps` — which also latches the reveal so
// it fires once. Keeping a second copy would be a third source of truth for the same fact.

impl DirScanState {
    /// Install a walk that is already running (or, in tests, one that never will). The
    /// caller owns spawning; this owns the bookkeeping.
    pub(crate) fn armed(
        id: crate::background::OpId,
        wire_gen: u64,
        rx: Receiver<(u64, ScanUpdate)>,
        progress: ScanProgress,
        name: String,
    ) -> Self {
        Self {
            id,
            rx,
            wire_gen,
            progress,
            name,
        }
    }

    /// Ask the walk to stop at its next entry. Idempotent.
    pub(crate) fn request_cancel(&self) {
        self.progress.request_cancel();
    }
}

/// What the shell should do with its **Scanning dialog** after a poll.
///
/// A return value rather than a `CoreEffect` on purpose (task #126 §0): the macOS shell
/// constructs and consumes the dialog effects in ~50 places and cannot be compiled from the
/// Windows dev box, so this step stays strictly additive to the contract. Folding these into
/// identity-stamped dialog effects is the phase-0 P0 item, to be done on a Mac where both
/// shells can be migrated together.
/// `PartialEq`/`Debug` are hand-written: [`ScanProgress`] is a shared `Arc` handle with
/// neither, and comparing worker handles would be meaningless anyway — two requests are the
/// same request when they name the same folder.
#[derive(Clone)]
pub enum ScanDialogRequest {
    /// Leave the dialog stack alone.
    None,
    /// The walk is slow and nothing is on screen yet — reveal the progress dialog.
    ///
    /// The shell still decides whether it *may* (it must not steal a Settings window the
    /// user opened, nor overwrite a Password request queued earlier in the same tick). That
    /// gate is genuine shell knowledge about its own dialog stack, so it stays there.
    Reveal {
        name: String,
        progress: ScanProgress,
    },
    /// Drop the progress dialog: a photo is on screen, the walk finished, or the worker died.
    Close,
}

/// A live, read-only view of the walk in flight — everything a shell needs to draw whatever
/// scan chrome it prefers, queryable every frame.
///
/// This is the **reconciliation** half of the design Codex asked for in plan §5a.2 ("shells
/// should reconcile toward a desired dialog state rather than execute unqualified imperative
/// commands"), and it is what lets one core serve two genuinely different presentations: the
/// core owns the *facts* (is a walk running, is it slow, is a photo up yet, how far along),
/// and each shell owns the *policy* of what chrome those facts warrant.
///
/// It also removes the stale-close hazard by construction for this flow. There is no late
/// imperative `Close` that can arrive after a newer walk started, because the core never
/// issues one — it only ever describes the state of whatever operation is current *now*.
#[derive(Clone, PartialEq, Debug)]
pub struct ScanStatus {
    /// The scanned folder's display name (a chrome headline).
    pub name: String,
    /// Images found so far.
    pub found: usize,
    /// The sub-folder currently being walked, blank while it is still the root (which would
    /// only repeat [`Self::name`]). Blanked here rather than in each shell because both want
    /// it and neither wants to re-derive the comparison.
    pub current_dir: String,
    /// The walk has outlasted [`SCAN_DIALOG_DELAY`] — slow enough to be worth telling the
    /// user about. A fast folder never reports `true`, so chrome never flashes.
    pub slow: bool,
    /// A photo is on screen: the first non-empty batch has bootstrapped the view. Chrome that
    /// blocks input (winit's modal dialog) must hide once this is true; ambient chrome (the
    /// macOS pill) deliberately stays up for the rest of the walk.
    pub bootstrapped: bool,
}

/// The outcome of one [`super::AppCore::poll_dir_scan`] tick.
#[derive(Clone, PartialEq, Debug)]
pub struct ScanPoll {
    /// What to do with the Scanning dialog.
    pub dialog: ScanDialogRequest,
    /// The walk finished and found **no images**; carries the folder name so the shell can
    /// toast it. `None` unless this tick ended the walk empty-handed.
    pub found_no_photos: Option<String>,
}

impl ScanPoll {
    pub(crate) fn idle() -> Self {
        Self {
            dialog: ScanDialogRequest::None,
            found_no_photos: None,
        }
    }
    pub(crate) fn dialog(d: ScanDialogRequest) -> Self {
        Self {
            dialog: d,
            found_no_photos: None,
        }
    }
}

impl PartialEq for ScanDialogRequest {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::None, Self::None) | (Self::Close, Self::Close) => true,
            (Self::Reveal { name: a, .. }, Self::Reveal { name: b, .. }) => a == b,
            _ => false,
        }
    }
}

impl std::fmt::Debug for ScanDialogRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "None"),
            Self::Close => write!(f, "Close"),
            Self::Reveal { name, .. } => write!(f, "Reveal({name:?})"),
        }
    }
}
