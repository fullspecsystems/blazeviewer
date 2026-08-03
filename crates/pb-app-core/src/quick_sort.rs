//! **Quick Sort** (tasks.json #136) — file the displayed photo into a configured folder with
//! one keypress. [`SLOT_COUNT`] slots, each holding a destination folder, a label, and a
//! Move/Copy mode; the first [`DEFAULT_BOUND_SLOTS`] carry a digit chord out of the box.
//! Press the key, the photo is filed, the deck advances. The long tedious sort becomes one
//! key at a time.
//!
//! This module is the **first half** (`docs/where-code-goes.md`): the slot model, the pure
//! naming/sidecar rules, and the file I/O. The `impl AppCore` half — validating the slot,
//! retiring the item from the deck, dispatching off-thread — lives in
//! `app_core_impl/quick_sort.rs`.
//!
//! **Privacy (root `CLAUDE.md` #2), stated here so a future audit need not re-litigate it:**
//! both halves are in-bounds. The slot *folders* are user-chosen preferences set deliberately
//! in Settings — the same category as `settings::picker_dir` (ADR-018), never a viewing trace.
//! The *moves* are explicit user edits — the same allowed category as delete and save-rotation,
//! reachable only from a keypress, never as a byproduct of viewing. What we deliberately do
//! **not** keep: no MRU of destinations, no log of what was sorted where, no per-slot counter.
//!
//! **Why the I/O never runs on the event loop.** A quick-sort key is a nav key in disguise, and
//! it is meant to be *hammered*. [`perform`] is called from a worker: an `fs::rename` onto an
//! SMB share is tens of milliseconds and a cross-volume copy of a 40 MB RAW is hundreds — the
//! synchronous shape `delete::recycle` gets away with for an occasional `Del` would blow the
//! one-refresh budget on every press here.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How many quick-sort slots exist.
///
/// Sixteen, of which [`DEFAULT_BOUND_SLOTS`] carry a key out of the box. The count and the
/// binding are deliberately **different numbers**: how many destinations a sort needs is a
/// property of the user's corpus, while how many chords are free is a property of the keymap.
/// Tying them together would have capped the feature at whatever the keyboard could spare.
/// Slots past the bound bank are configured the same way and reached by binding a chord in
/// Settings ▸ Shortcuts.
pub const SLOT_COUNT: usize = 16;

/// How many slots ship with a default key: `1`–`7` for slots 1–7, `Shift+1`–`Shift+7` for
/// slots 8–14. `8`/`9`/`0` are already Fit / Fill / Toggle-1:1 (`keymap.rs`), which is what
/// bounds the first bank at seven; the shifted bank is free.
///
/// Chords are matched on the **physical** key plus modifier flags (`KeyChord`), so `Shift+1`
/// is a real chord on every layout — it never has to become `!`.
pub const DEFAULT_BOUND_SLOTS: usize = 14;

/// A slot that ships with a chord must exist. Checked at **compile** time rather than in a
/// test: raising `DEFAULT_BOUND_SLOTS` past `SLOT_COUNT` should fail the build, not wait for
/// someone to run the suite.
const _: () = assert!(DEFAULT_BOUND_SLOTS <= SLOT_COUNT);

/// What a slot does with the file it is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortMode {
    /// Move the file out of the deck's folder. The default, and the one that makes the source
    /// folder draining toward empty your progress bar.
    #[default]
    Move,
    /// Copy it, leaving the original in place. ⚠ A copy leaves the item **in the deck**, so
    /// the viewer does not advance — the photo you just filed is still the one on screen.
    Copy,
}

/// One configurable destination slot.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuickSortSlot {
    /// Where files go. `None` = the slot is unconfigured and its key does nothing.
    pub folder: Option<PathBuf>,
    /// What the pill says when you press the key ("→ Portraits"). Empty falls back to the
    /// folder's own name, so a configured slot always has something to show.
    pub label: String,
    pub mode: SortMode,
}

impl QuickSortSlot {
    /// The name to show for this slot — the user's label, else the destination folder's own
    /// name, else empty (an unconfigured slot).
    pub fn display_label(&self) -> String {
        if !self.label.trim().is_empty() {
            return self.label.trim().to_string();
        }
        self.folder
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Whether pressing this slot's key does anything.
    pub fn is_configured(&self) -> bool {
        self.folder.is_some()
    }
}

/// Pad/trim a persisted slot list to exactly [`SLOT_COUNT`], so the rest of the code can index
/// slots without bounds-checking a user-edited `settings.toml`.
pub fn normalize_slots(mut slots: Vec<QuickSortSlot>) -> Vec<QuickSortSlot> {
    slots.truncate(SLOT_COUNT);
    slots.resize_with(SLOT_COUNT, QuickSortSlot::default);
    slots
}

/// A fully cleared slot list — what **Settings ▸ Quick Sort ▸ Clear All** writes.
///
/// This exists as a named operation because it is the feature's **privacy escape hatch**, not
/// merely a convenience. Slot folders are legitimate preferences (ADR-018) and stay in
/// `settings.toml` until removed — but folder names are chosen by the user and can be telling
/// on their own (a bank of slots named after people says something even though no photo path
/// was ever recorded). One obvious control that empties the lot, rather than sixteen individual
/// Clear buttons, is what makes "get this off my disk" a single deliberate action.
///
/// It clears the *destinations*, not the key bindings: chords live in `keymap.toml` and name
/// only slot numbers, which reveal nothing.
pub fn cleared_slots() -> Vec<QuickSortSlot> {
    normalize_slots(Vec::new())
}

// ── Pure naming rules ─────────────────────────────────────────────────────────────────────

/// The compound suffixes where the extension is two dot-segments, not one. Deliberately a tiny
/// explicit list rather than a heuristic: a rule like "trailing short alphanumeric segments"
/// mangles `IMG_1234.2026.01.05.jpg`, and unpredictable renaming is worse than a missed case.
/// (Kept local rather than reaching for `pb_source::archive_kind` — that classifier answers
/// "is this an archive", a different question from "where does the extension start".)
const COMPOUND_EXTS: &[&str] = &[".tar.gz", ".tar.bz2", ".tar.zst", ".tar.xz"];

/// Split a file name into `(stem, extension)`, where the extension keeps its leading dot and is
/// `""` when there is none. Compound archive suffixes stay whole (`a.tar.gz` → `("a",
/// ".tar.gz")`), and a dotfile is all stem (`.gitignore` → `(".gitignore", "")`) — a leading dot
/// names the file, it does not introduce an extension.
pub fn split_name(name: &str) -> (&str, &str) {
    let lower = name.to_ascii_lowercase();
    for compound in COMPOUND_EXTS {
        if lower.ends_with(compound) && lower.len() > compound.len() {
            return name.split_at(name.len() - compound.len());
        }
    }
    match name.rfind('.') {
        // `> 0` skips the dotfile case: `.gitignore`'s dot is at 0.
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    }
}

/// A file name that does not collide in the destination: `name` itself when it is free, else
/// `stem-1.ext`, `stem-2.ext`, … until one is. `is_taken` probes the destination (the real
/// caller does a `try_exists`, so this costs one syscall per collision rather than a `read_dir`
/// of a folder that may hold 50k files).
///
/// The separator is a bare hyphen, not Windows' `" (1)"` or macOS' `" 2"`: a sorted-out corpus
/// is very often the input to a script, and spaces and parentheses in file names are a menace
/// there. Refusing on collision was the alternative and it is wrong — a stall defeats the
/// entire point of a one-key sort.
pub fn unique_name(name: &str, is_taken: impl Fn(&str) -> bool) -> String {
    if !is_taken(name) {
        return name.to_string();
    }
    let (stem, ext) = split_name(name);
    // Bounded only by the collision count; a folder that somehow defeats this is pathological
    // and the caller's I/O will report it honestly rather than us guessing.
    (1u32..)
        .map(|n| format!("{stem}-{n}{ext}"))
        .find(|candidate| !is_taken(candidate))
        .expect("the range is unbounded")
}

/// A destination name free for the image **and every one of its sidecars at once**.
///
/// ⚠ This is the fix for a real, deterministic data-loss bug (found by a Codex review,
/// 2026-08-03, and reproduced before fixing). Naming the image alone is not enough: with
/// `IMG_1.jpg` + `IMG_1.txt` incoming, and a destination that already holds `IMG_1.txt` but
/// **no** `IMG_1.jpg`, the image took the free `IMG_1.jpg` and the sidecar was then written
/// straight over the existing label — `fs::rename` replaces silently on POSIX, and the
/// `fs::copy` fallback does the same on Windows. A training set's label was destroyed with no
/// error and nothing to undo it, which is precisely the silent corruption the sidecar feature
/// exists to prevent.
///
/// Suffixing the group as a unit — rather than giving each file its own suffix — is what keeps
/// a sidecar attached to its image: `IMG_1-2.jpg` must be accompanied by `IMG_1-2.txt`, never
/// by an independently-numbered `IMG_1-5.txt`.
///
/// The original collision test missed this because it seeded *both* `IMG_1.jpg` and
/// `IMG_1.txt`, so the image was pushed to `IMG_1-1.jpg` and `IMG_1-1.txt` happened to be free.
/// The asymmetric case is the dangerous one.
fn unique_name_for_group(
    name: &str,
    sidecars: &[SidecarCandidate],
    is_taken: impl Fn(&str) -> bool,
) -> String {
    let group_free =
        |image: &str| !is_taken(image) && !sidecars.iter().any(|s| is_taken(&s.renamed_for(image)));
    if group_free(name) {
        return name.to_string();
    }
    let (stem, ext) = split_name(name);
    (1u32..)
        .map(|n| format!("{stem}-{n}{ext}"))
        .find(|candidate| group_free(candidate))
        .expect("the range is unbounded")
}

// ── Sidecars ──────────────────────────────────────────────────────────────────────────────

/// Extensions we treat as belonging to the image beside them. `xmp`/`pp3`/`dop`/`on1`/`arp` are
/// RAW-editor sidecars; `txt`/`json`/`yaml`/`yml` are the ML-annotation conventions (YOLO writes
/// `IMG_1234.txt`, COCO-style tooling `IMG_1234.json`); `aae` is Apple's edit record; `thm` is a
/// camera thumbnail.
///
/// This list is the reason quick sort moves sidecars at all: filing an image out of a training
/// set and orphaning its label file corrupts the set *silently*, which is the worst failure
/// mode a sort tool can have.
const SIDECAR_EXTS: &[&str] = &[
    "xmp", "txt", "json", "yaml", "yml", "aae", "thm", "pp3", "dop", "on1", "arp",
];

/// Which of the two real sidecar naming conventions a candidate follows. Both exist in the
/// wild and both must be matched — and they must be *renamed differently* when the image
/// collides in the destination, which is why the form is carried rather than inferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidecarForm {
    /// `IMG_1234.xmp` beside `IMG_1234.cr2` — replace the stem. Adobe's RAW convention, and
    /// YOLO's.
    Stem,
    /// `IMG_1234.jpg.xmp` beside `IMG_1234.jpg` — replace the whole name.
    FullName,
}

/// A file name that *would* be a sidecar of the image, if it exists. The caller probes; nothing
/// here touches the filesystem (the same pure-rules-over-names discipline as
/// [`crate::sidecar`], and it keeps the cost at a fixed ~22 `try_exists` probes instead of a
/// `read_dir` that scales with the folder).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarCandidate {
    pub name: String,
    pub form: SidecarForm,
}

impl SidecarCandidate {
    /// This sidecar's name once the image has been renamed to `new_image_name` — so a sidecar
    /// stays attached to its image even when a destination collision forced `IMG_1234.jpg` to
    /// land as `IMG_1234-1.jpg`. Getting this wrong is how you silently detach every label file
    /// in a colliding batch.
    pub fn renamed_for(&self, new_image_name: &str) -> String {
        let (_, sidecar_ext) = split_name(&self.name);
        match self.form {
            SidecarForm::Stem => {
                let (new_stem, _) = split_name(new_image_name);
                format!("{new_stem}{sidecar_ext}")
            }
            SidecarForm::FullName => format!("{new_image_name}{sidecar_ext}"),
        }
    }
}

/// Every name that would be a sidecar of `image_name`, in both conventions. Fixed length
/// (`2 × SIDECAR_EXTS`), deterministic order, no I/O.
pub fn sidecar_candidates(image_name: &str) -> Vec<SidecarCandidate> {
    let (stem, _) = split_name(image_name);
    let mut out = Vec::with_capacity(SIDECAR_EXTS.len() * 2);
    for ext in SIDECAR_EXTS {
        // Full-name first: `IMG_1234.jpg.xmp` is unambiguous, while the stem form of an
        // extension-less file would be the file itself (guarded below).
        out.push(SidecarCandidate {
            name: format!("{image_name}.{ext}"),
            form: SidecarForm::FullName,
        });
        let stem_form = format!("{stem}.{ext}");
        // A sidecar is never the image itself — matters for an extension-less image, where the
        // stem *is* the whole name.
        if !stem_form.eq_ignore_ascii_case(image_name) {
            out.push(SidecarCandidate {
                name: stem_form,
                form: SidecarForm::Stem,
            });
        }
    }
    out
}

// ── The I/O ───────────────────────────────────────────────────────────────────────────────

/// What a completed sort actually did — enough for the undo entry to put every piece back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SortOutcome {
    /// Where the image landed (its name may differ from the source's if the destination
    /// collided).
    pub dest: PathBuf,
    /// `(from, to)` for every sidecar that actually moved.
    pub sidecars: Vec<(PathBuf, PathBuf)>,
    /// Sidecars that matched but could not be moved. Reported, never fatal: the image is the
    /// operation, and failing the whole sort because a `.xmp` was locked would be worse.
    pub sidecar_failures: usize,
}

/// File `src` into `dest_dir` per `mode`, taking its sidecars along.
///
/// Creates `dest_dir` if it is missing — the folder was configured deliberately, so recreating
/// it keeps the flow uninterrupted (owner decision, 2026-08-02); a failure to create is
/// reported rather than swallowed.
///
/// **Runs on a worker, never the event loop** (see the module doc).
pub fn perform(src: &Path, dest_dir: &Path, mode: SortMode) -> Result<SortOutcome, String> {
    let src_name = utf8_name(src)?;
    let src_dir = src
        .parent()
        .ok_or_else(|| format!("{} has no parent folder", src.display()))?;

    std::fs::create_dir_all(dest_dir)
        .map_err(|e| format!("Couldn't create {}: {e}", dest_dir.display()))?;

    // Which sidecars actually exist beside the image, probed once.
    let present: Vec<SidecarCandidate> = sidecar_candidates(src_name)
        .into_iter()
        .filter(|c| src_dir.join(&c.name).try_exists().unwrap_or(false))
        .collect();

    let dest_name = unique_name_for_group(src_name, &present, |candidate| {
        dest_dir.join(candidate).try_exists().unwrap_or(false)
    });
    let dest = dest_dir.join(&dest_name);

    transfer(src, &dest, mode)?;

    // Sidecars are best-effort from here: the image has already moved, and a stalled `.xmp`
    // must not turn a completed sort into a failure. Their destinations are already known
    // free — `unique_name_for_group` above cleared the whole set together.
    let mut outcome = SortOutcome {
        dest,
        ..Default::default()
    };
    for candidate in present {
        let from = src_dir.join(&candidate.name);
        let to = dest_dir.join(candidate.renamed_for(&dest_name));
        match transfer(&from, &to, mode) {
            Ok(()) => outcome.sidecars.push((from, to)),
            Err(e) => {
                eprintln!("quick sort: sidecar {} not moved: {e}", from.display());
                outcome.sidecar_failures += 1;
            }
        }
    }
    Ok(outcome)
}

/// Move or copy one file, per `mode`.
fn transfer(from: &Path, to: &Path, mode: SortMode) -> Result<(), String> {
    match mode {
        SortMode::Move => move_file(from, to),
        SortMode::Copy => std::fs::copy(from, to)
            .map(|_| ())
            .map_err(|e| e.to_string()),
    }
}

/// Move a file, falling back to copy-then-remove when the rename cannot cross the boundary
/// between `from` and `to` (`EXDEV` — a different volume, or an SMB/NFS mount).
pub fn move_file(from: &Path, to: &Path) -> Result<(), String> {
    // Wrapped in a closure rather than passed as `std::fs::rename`: the generic fn item binds
    // its lifetimes early, so it doesn't satisfy the `for<'a>` bound `move_file_with` needs.
    move_file_with(from, to, |a, b| std::fs::rename(a, b))
}

/// [`move_file`] with the rename injected, so a test can force the cross-volume path without
/// needing two real filesystems.
///
/// The fallback deliberately ignores *why* the rename failed rather than matching on
/// `ErrorKind::CrossesDevices` (still unstable, and platforms disagree on which errno they
/// raise). A copy that then also fails reports **its own** error — the rename's `EXDEV` would
/// only mislead. The source is never removed until the copy has succeeded.
pub fn move_file_with(
    from: &Path,
    to: &Path,
    rename: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), String> {
    if rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).map_err(|e| e.to_string())?;
    if let Err(e) = std::fs::remove_file(from) {
        // The copy landed, so the file *is* filed; leaving a stale original is the lesser
        // evil, and saying so is better than a silent duplicate.
        return Err(format!("copied, but the original couldn't be removed: {e}"));
    }
    Ok(())
}

// ── The worker ────────────────────────────────────────────────────────────────────────────

/// One queued sort, as handed to the worker.
#[derive(Clone, Debug)]
pub struct SortJob {
    /// The item's playlist index at press time, so a failure can put it back where it was.
    pub index: usize,
    pub src: PathBuf,
    pub dest_dir: PathBuf,
    pub mode: SortMode,
    /// The slot's display name, carried so the completion toast can name it without the core
    /// re-reading settings that may have changed since the press.
    pub slot_label: String,
}

/// A finished sort, reported back to the event loop.
#[derive(Debug)]
pub struct SortDone {
    pub job: SortJob,
    pub result: Result<SortOutcome, String>,
}

/// The quick-sort worker: **one** thread draining a queue, plus the channel results come back
/// on. Created lazily on the first sort, so a session that never uses the feature (and every
/// headless test) spawns nothing.
///
/// **Why exactly one thread, not a pool.** [`perform`] picks a free destination name and *then*
/// renames into it. Two sorts racing into the same folder could both probe `IMG_1.jpg`, both
/// find it free, and the second would clobber the first — a silent data-loss TOCTOU. Draining
/// the queue in order removes the race by construction, and it costs nothing: the bottleneck
/// is the filesystem, which does not go faster for being asked concurrently. It also makes
/// undo order equal press order, which is what the user expects from a stack.
pub struct SortQueue {
    jobs: std::sync::mpsc::Sender<SortJob>,
    done: std::sync::mpsc::Receiver<SortDone>,
}

impl SortQueue {
    /// Spawn the worker.
    pub fn new() -> SortQueue {
        let (jobs_tx, jobs_rx) = std::sync::mpsc::channel::<SortJob>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<SortDone>();
        std::thread::Builder::new()
            .name("pb-quick-sort".into())
            .spawn(move || {
                // Ends when the sender drops (app teardown) — nothing to join, nothing to
                // flush: every job either completed or never started, and neither state
                // needs persisting (privacy #2 — no on-disk queue).
                while let Ok(job) = jobs_rx.recv() {
                    let result = perform(&job.src, &job.dest_dir, job.mode);
                    if done_tx.send(SortDone { job, result }).is_err() {
                        break; // the core is gone
                    }
                }
            })
            .expect("spawn the quick-sort worker");
        SortQueue {
            jobs: jobs_tx,
            done: done_rx,
        }
    }

    /// Queue a sort. Returns `false` only if the worker died, which the caller reports rather
    /// than silently dropping the user's keypress.
    pub fn submit(&self, job: SortJob) -> bool {
        self.jobs.send(job).is_ok()
    }

    /// Every sort that has finished since the last call. Never blocks — called from `tick`.
    pub fn drain(&self) -> Vec<SortDone> {
        self.done.try_iter().collect()
    }
}

impl Default for SortQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SortQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SortQueue")
    }
}

/// Reverse a completed sort — the I/O half of [`crate::undo::UndoAction::Sorted`].
///
/// A [`SortMode::Move`] renames every piece back; a [`SortMode::Copy`] deletes the copies it
/// made (the originals never moved, so there is nothing to restore). Refuses a Move whose
/// original path is occupied again rather than overwriting whatever now lives there — the same
/// collision stance as `delete::restore`, and the file stays safely at its sorted location.
pub fn undo_sort(
    from: &Path,
    to: &Path,
    sidecars: &[(PathBuf, PathBuf)],
    mode: SortMode,
) -> Result<(), String> {
    if mode == SortMode::Copy {
        std::fs::remove_file(to).map_err(|e| e.to_string())?;
        for (_, copied) in sidecars {
            let _ = std::fs::remove_file(copied); // best-effort, as on the way out
        }
        return Ok(());
    }
    if from.try_exists().unwrap_or(false) {
        return Err(format!("{} already exists", from.display()));
    }
    move_file(to, from)?;
    // Sidecars are best-effort on the way back too: the image is the operation, and a stuck
    // `.xmp` must not leave the undo half-done and unreportable.
    for (orig, moved) in sidecars {
        // ⚠ The same no-clobber rule the image gets above. Undo used to move a sidecar back
        // unconditionally, so an editor that wrote a fresh `a.xmp` after the sort had it
        // silently replaced by the stale one we were returning (Codex review, 2026-08-03).
        // Leaving ours at the destination is recoverable; overwriting the user's newer file
        // is not.
        if orig.try_exists().unwrap_or(false) {
            eprintln!(
                "quick sort undo: {} reappeared — leaving the sorted copy alone",
                orig.display()
            );
            continue;
        }
        if let Err(e) = move_file(moved, orig) {
            eprintln!(
                "quick sort undo: sidecar {} not restored: {e}",
                moved.display()
            );
        }
    }
    Ok(())
}

/// A path's file name as UTF-8. Refuses rather than going lossy: a lossy conversion would have
/// us create a *differently named* file and then delete the user's original.
fn utf8_name(path: &Path) -> Result<&str, String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{} has no usable file name", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ── split_name ────────────────────────────────────────────────────────────────────────

    #[test]
    fn split_name_handles_the_ordinary_case() {
        assert_eq!(split_name("IMG_1234.jpg"), ("IMG_1234", ".jpg"));
        assert_eq!(split_name("photo.jpeg"), ("photo", ".jpeg"));
    }

    #[test]
    fn split_name_keeps_compound_archive_suffixes_whole() {
        assert_eq!(split_name("trip.tar.gz"), ("trip", ".tar.gz"));
        assert_eq!(split_name("trip.TAR.GZ"), ("trip", ".TAR.GZ"));
        assert_eq!(split_name("backup.tar.zst"), ("backup", ".tar.zst"));
        // Not a compound: the stem doesn't end in `.tar`.
        assert_eq!(split_name("photo.jpg.gz"), ("photo.jpg", ".gz"));
    }

    #[test]
    fn split_name_treats_a_dotfile_as_all_stem() {
        assert_eq!(split_name(".gitignore"), (".gitignore", ""));
        assert_eq!(split_name("README"), ("README", ""));
        // A dotfile that also has an extension still splits at the last dot.
        assert_eq!(split_name(".config.toml"), (".config", ".toml"));
    }

    #[test]
    fn split_name_survives_non_ascii_stems() {
        assert_eq!(split_name("naïve-café.jpg"), ("naïve-café", ".jpg"));
        assert_eq!(split_name("写真.png"), ("写真", ".png"));
    }

    #[test]
    fn split_name_does_not_mistake_a_bare_compound_suffix_for_one() {
        // A file literally named "tar.gz" is stem "tar", ext ".gz" — there is no name left
        // in front of the compound, so treating it as one would give an empty stem.
        assert_eq!(split_name("tar.gz"), ("tar", ".gz"));
    }

    // ── unique_name ───────────────────────────────────────────────────────────────────────

    #[test]
    fn unique_name_passes_a_free_name_through() {
        assert_eq!(unique_name("IMG_1234.jpg", |_| false), "IMG_1234.jpg");
    }

    #[test]
    fn unique_name_suffixes_before_the_extension() {
        let taken: HashSet<&str> = ["IMG_1234.jpg"].into_iter().collect();
        assert_eq!(
            unique_name("IMG_1234.jpg", |n| taken.contains(n)),
            "IMG_1234-1.jpg"
        );
    }

    #[test]
    fn unique_name_counts_up_past_a_run_of_collisions() {
        let taken: HashSet<&str> = ["a.jpg", "a-1.jpg", "a-2.jpg"].into_iter().collect();
        assert_eq!(unique_name("a.jpg", |n| taken.contains(n)), "a-3.jpg");
    }

    #[test]
    fn unique_name_respects_compound_and_missing_extensions() {
        let taken: HashSet<&str> = ["trip.tar.gz", "README"].into_iter().collect();
        assert_eq!(
            unique_name("trip.tar.gz", |n| taken.contains(n)),
            "trip-1.tar.gz"
        );
        assert_eq!(unique_name("README", |n| taken.contains(n)), "README-1");
    }

    #[test]
    fn unique_name_uses_a_hyphen_not_a_space_or_parens() {
        let taken: HashSet<&str> = ["a.jpg"].into_iter().collect();
        let out = unique_name("a.jpg", |n| taken.contains(n));
        assert!(
            !out.contains(' '),
            "no spaces in a script-hostile name: {out}"
        );
        assert!(!out.contains('('), "no parentheses: {out}");
    }

    // ── sidecars ──────────────────────────────────────────────────────────────────────────

    fn names(image: &str) -> Vec<String> {
        sidecar_candidates(image)
            .into_iter()
            .map(|c| c.name)
            .collect()
    }

    #[test]
    fn sidecar_candidates_cover_both_conventions() {
        let out = names("IMG_1234.jpg");
        assert!(out.contains(&"IMG_1234.xmp".to_string()), "stem form");
        assert!(
            out.contains(&"IMG_1234.jpg.xmp".to_string()),
            "full-name form"
        );
        assert!(out.contains(&"IMG_1234.txt".to_string()), "YOLO labels");
        assert!(out.contains(&"IMG_1234.json".to_string()), "COCO-style");
    }

    #[test]
    fn sidecar_candidates_never_include_the_image_itself() {
        // An extension-less image: its stem IS its whole name, so the stem form of `.txt`
        // would be fine, but for a `.txt` "image" the stem form would name the file itself.
        let out = names("notes.txt");
        assert!(
            !out.contains(&"notes.txt".to_string()),
            "a file is not its own sidecar: {out:?}"
        );
        assert!(
            out.contains(&"notes.txt.txt".to_string()),
            "the full-name form is still a distinct file"
        );
    }

    #[test]
    fn a_near_miss_stem_is_not_a_sidecar() {
        let out = names("IMG_1234.jpg");
        assert!(
            !out.contains(&"IMG_12345.txt".to_string()),
            "a longer stem is a different file"
        );
        assert!(!out.contains(&"IMG_123.txt".to_string()));
    }

    #[test]
    fn an_unrelated_extension_is_not_a_sidecar() {
        let out = names("IMG_1234.jpg");
        assert!(!out.contains(&"IMG_1234.mp4".to_string()));
        assert!(!out.contains(&"IMG_1234.png".to_string()));
    }

    #[test]
    fn a_sidecar_follows_the_image_through_a_collision_rename() {
        let stem = SidecarCandidate {
            name: "IMG_1234.txt".into(),
            form: SidecarForm::Stem,
        };
        let full = SidecarCandidate {
            name: "IMG_1234.jpg.xmp".into(),
            form: SidecarForm::FullName,
        };
        // The image landed as IMG_1234-1.jpg; both sidecars must land attached to it.
        assert_eq!(stem.renamed_for("IMG_1234-1.jpg"), "IMG_1234-1.txt");
        assert_eq!(full.renamed_for("IMG_1234-1.jpg"), "IMG_1234-1.jpg.xmp");
    }

    #[test]
    fn a_sidecar_keeps_its_name_when_the_image_did_not_collide() {
        let stem = SidecarCandidate {
            name: "IMG_1234.txt".into(),
            form: SidecarForm::Stem,
        };
        assert_eq!(stem.renamed_for("IMG_1234.jpg"), "IMG_1234.txt");
    }

    // ── slots ─────────────────────────────────────────────────────────────────────────────

    #[test]
    fn a_slot_labels_itself_from_its_folder_when_unnamed() {
        let slot = QuickSortSlot {
            folder: Some(PathBuf::from("/data/faces/Portraits")),
            label: String::new(),
            mode: SortMode::Move,
        };
        assert_eq!(slot.display_label(), "Portraits");
        assert!(slot.is_configured());
    }

    #[test]
    fn an_explicit_label_wins_over_the_folder_name() {
        let slot = QuickSortSlot {
            folder: Some(PathBuf::from("/data/faces/cls_003")),
            label: "  Ada  ".into(),
            mode: SortMode::Move,
        };
        assert_eq!(slot.display_label(), "Ada", "trimmed");
    }

    #[test]
    fn an_unconfigured_slot_has_no_label_and_does_nothing() {
        let slot = QuickSortSlot::default();
        assert!(!slot.is_configured());
        assert_eq!(slot.display_label(), "");
        assert_eq!(slot.mode, SortMode::Move, "Move is the default");
    }

    #[test]
    fn normalize_pads_and_trims_to_the_slot_count() {
        assert_eq!(normalize_slots(vec![]).len(), SLOT_COUNT);
        let too_many = vec![QuickSortSlot::default(); SLOT_COUNT + 4];
        assert_eq!(normalize_slots(too_many).len(), SLOT_COUNT);
        // Padding preserves what was there.
        let one = vec![QuickSortSlot {
            folder: Some(PathBuf::from("/x")),
            label: "keep".into(),
            mode: SortMode::Copy,
        }];
        let out = normalize_slots(one);
        assert_eq!(out[0].label, "keep");
        assert_eq!(out[0].mode, SortMode::Copy);
        assert!(!out[1].is_configured());
    }

    #[test]
    fn clear_all_leaves_no_configured_slot_and_no_label_behind() {
        let out = cleared_slots();
        assert_eq!(out.len(), SLOT_COUNT, "the slots remain, emptied");
        assert!(
            out.iter().all(|s| !s.is_configured()),
            "no destination survives a Clear All"
        );
        assert!(
            out.iter().all(|s| s.display_label().is_empty()),
            "and no user-chosen name survives either"
        );
    }

    // ── I/O ───────────────────────────────────────────────────────────────────────────────

    /// A throwaway directory tree for the I/O tests, removed on drop.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pb_quick_sort_{tag}_{}_{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("mkdir");
            TempTree(dir)
        }
        fn write(&self, rel: &str, body: &str) -> PathBuf {
            let p = self.0.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).expect("mkdir parent");
            }
            std::fs::write(&p, body).expect("write");
            p
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn perform_moves_the_image_and_creates_a_missing_destination() {
        let t = TempTree::new("move");
        let src = t.write("src/IMG_1.jpg", "pixels");
        let dest_dir = t.path("out/Portraits"); // deliberately does not exist

        let outcome = perform(&src, &dest_dir, SortMode::Move).expect("sort");

        assert_eq!(outcome.dest, dest_dir.join("IMG_1.jpg"));
        assert!(!src.exists(), "the original left the source folder");
        assert_eq!(
            std::fs::read_to_string(&outcome.dest).expect("read"),
            "pixels"
        );
    }

    #[test]
    fn perform_copies_without_removing_the_original() {
        let t = TempTree::new("copy");
        let src = t.write("src/IMG_1.jpg", "pixels");
        let dest_dir = t.path("out");

        let outcome = perform(&src, &dest_dir, SortMode::Copy).expect("sort");

        assert!(src.exists(), "a copy leaves the original in place");
        assert_eq!(
            std::fs::read_to_string(&outcome.dest).expect("read"),
            "pixels"
        );
    }

    #[test]
    fn perform_takes_sidecars_along_in_both_conventions() {
        let t = TempTree::new("sidecars");
        let src = t.write("src/IMG_1.jpg", "pixels");
        t.write("src/IMG_1.txt", "0 0.5 0.5 0.2 0.2"); // YOLO label, stem form
        t.write("src/IMG_1.jpg.xmp", "<xmp/>"); // full-name form
        t.write("src/IMG_1.mp4", "unrelated"); // NOT a sidecar
        let dest_dir = t.path("out");

        let outcome = perform(&src, &dest_dir, SortMode::Move).expect("sort");

        assert_eq!(outcome.sidecars.len(), 2, "both conventions moved");
        assert_eq!(outcome.sidecar_failures, 0);
        assert!(
            dest_dir.join("IMG_1.txt").exists(),
            "the YOLO label followed"
        );
        assert!(dest_dir.join("IMG_1.jpg.xmp").exists());
        assert!(
            t.path("src/IMG_1.mp4").exists(),
            "an unrelated sibling stayed put"
        );
    }

    #[test]
    fn a_destination_collision_suffixes_the_image_and_keeps_sidecars_attached() {
        let t = TempTree::new("collide");
        let src = t.write("src/IMG_1.jpg", "second");
        t.write("src/IMG_1.txt", "second label");
        let dest_dir = t.path("out");
        t.write("out/IMG_1.jpg", "first"); // already filed an IMG_1.jpg earlier
        t.write("out/IMG_1.txt", "first label");

        let outcome = perform(&src, &dest_dir, SortMode::Move).expect("sort");

        assert_eq!(outcome.dest, dest_dir.join("IMG_1-1.jpg"));
        assert_eq!(
            std::fs::read_to_string(dest_dir.join("IMG_1.jpg")).expect("read"),
            "first",
            "the earlier file is untouched"
        );
        // The whole point: the label must follow its own image, not overwrite the other one.
        assert_eq!(
            std::fs::read_to_string(dest_dir.join("IMG_1-1.txt")).expect("read"),
            "second label"
        );
        assert_eq!(
            std::fs::read_to_string(dest_dir.join("IMG_1.txt")).expect("read"),
            "first label",
            "the earlier label is untouched"
        );
    }

    /// ⚠ REGRESSION (Codex review, 2026-08-03 — reproduced as real data loss before fixing).
    /// The destination holds `IMG_1.txt` but **no** `IMG_1.jpg`. The image alone would have
    /// found `IMG_1.jpg` free and its sidecar would then have been written straight over the
    /// existing label. The whole group must move aside together.
    #[test]
    fn an_incoming_sidecar_never_overwrites_one_already_at_the_destination() {
        let t = TempTree::new("sidecar_clobber");
        let src = t.write("src/IMG_1.jpg", "new pixels");
        t.write("src/IMG_1.txt", "NEW label");
        let dest_dir = t.path("out");
        t.write("out/IMG_1.txt", "OLD label"); // present; the IMAGE name is free

        let outcome = perform(&src, &dest_dir, SortMode::Move).expect("sort");

        assert_eq!(
            std::fs::read_to_string(dest_dir.join("IMG_1.txt")).expect("read"),
            "OLD label",
            "the label already at the destination survives"
        );
        assert_eq!(
            outcome.dest,
            dest_dir.join("IMG_1-1.jpg"),
            "the image moves aside even though its own name was free, so its label can follow"
        );
        assert_eq!(
            std::fs::read_to_string(dest_dir.join("IMG_1-1.txt")).expect("read"),
            "NEW label",
            "and the incoming label lands beside its own image"
        );
    }

    /// The mirror case: the IMAGE name is taken but the sidecar's is free. The group still
    /// moves as a unit, so the pair stays attached.
    #[test]
    fn the_group_moves_as_a_unit_when_only_the_image_name_collides() {
        let t = TempTree::new("image_clobber");
        let src = t.write("src/IMG_1.jpg", "new pixels");
        t.write("src/IMG_1.txt", "NEW label");
        let dest_dir = t.path("out");
        t.write("out/IMG_1.jpg", "old pixels");

        let outcome = perform(&src, &dest_dir, SortMode::Move).expect("sort");

        assert_eq!(outcome.dest, dest_dir.join("IMG_1-1.jpg"));
        assert_eq!(
            std::fs::read_to_string(dest_dir.join("IMG_1-1.txt")).expect("read"),
            "NEW label"
        );
    }

    /// Undo must not clobber a sidecar that reappeared at the source while the photo was away.
    #[test]
    fn undo_leaves_a_sidecar_that_came_back_while_the_photo_was_sorted() {
        let t = TempTree::new("undo_clobber");
        let src = t.write("src/a.jpg", "pixels");
        t.write("src/a.txt", "original label");
        let dest_dir = t.path("out");

        let outcome = perform(&src, &dest_dir, SortMode::Move).expect("sort");
        // An editor writes a NEW label at the source before the user undoes.
        t.write("src/a.txt", "freshly written label");

        undo_sort(&src, &outcome.dest, &outcome.sidecars, SortMode::Move).expect("undo");

        assert_eq!(
            std::fs::read_to_string(t.path("src/a.txt")).expect("read"),
            "freshly written label",
            "the newer file wins; undo must not restore the stale one over it"
        );
        assert!(src.exists(), "the image itself still came home");
    }

    #[test]
    fn move_file_falls_back_to_copy_when_the_rename_cannot_cross() {
        let t = TempTree::new("exdev");
        let src = t.write("src/a.jpg", "bytes");
        let dst = t.path("out/a.jpg");
        std::fs::create_dir_all(dst.parent().expect("parent")).expect("mkdir");

        // Force the cross-volume path without needing a second filesystem.
        let out = move_file_with(&src, &dst, |_, _| {
            Err(std::io::Error::other("simulated EXDEV"))
        });

        assert!(out.is_ok(), "the copy fallback carried it: {out:?}");
        assert!(!src.exists(), "the source was removed after a good copy");
        assert_eq!(std::fs::read_to_string(&dst).expect("read"), "bytes");
    }

    #[test]
    fn move_file_never_removes_the_source_when_the_copy_fails() {
        let t = TempTree::new("nocopy");
        let src = t.write("src/a.jpg", "bytes");
        // A destination directory that does not exist → the copy fails.
        let dst = t.path("nope/deeper/a.jpg");

        let out = move_file_with(&src, &dst, |_, _| {
            Err(std::io::Error::other("simulated EXDEV"))
        });

        assert!(out.is_err(), "reports the copy's own error");
        assert!(src.exists(), "the user's file is still there");
    }

    #[test]
    fn perform_reports_a_destination_it_cannot_create() {
        let t = TempTree::new("baddest");
        let src = t.write("src/a.jpg", "bytes");
        // A *file* stands where the destination folder would go.
        let blocker = t.write("blocked", "not a folder");

        let out = perform(&src, &blocker, SortMode::Move);

        assert!(out.is_err(), "a create_dir_all failure is reported");
        assert!(src.exists(), "and nothing was moved");
    }
}
