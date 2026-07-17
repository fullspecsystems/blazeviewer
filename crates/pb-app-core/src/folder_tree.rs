//! Folder-tree derivation for the `Shift+F` overlay — phase 2: the rows plus what
//! **clicking** each row opens and (where free) per-folder photo counts.
//!
//! Builds the [`FolderTreeModel`] the HUD rasterizes and the click handler
//! consumes. For disk decks the display anchors one level **above** the opened
//! root: the parent is the heading and the root sits expanded among its
//! siblings — so clicking a sibling (which re-roots the deck) still leaves the
//! new root's neighbours one click away — then **every folder at every level
//! along the path** down to the current photo's folder, with the on-path folder
//! expanding in place, the current folder highlighted, and its children one
//! level deeper. The "up" affordance row exits one more level. A path deeper
//! than [`MAX_ANCESTORS`] + 1 levels folds its shallow levels into one dim "…"
//! marker row.
//!
//! Two derivations, one shape:
//! - **Disk** ([`rows_from_disk`]): the path levels come from the path
//!   components between the root and the current folder (no I/O); each visible
//!   level's folder list costs one `read_dir` (≤ [`MAX_ANCESTORS`] + 2 of
//!   them), directories only. Read-only, and only ever run on the explicit
//!   `Shift+F` toggle or when the current folder changes while the overlay is
//!   open — never on the view/decode path (privacy #2). Counts come from the
//!   caller's optional [`disk_counts`] map (recursive decks — free from the
//!   in-RAM playlist; plain decks pass `None`).
//! - **Entry names** ([`rows_from_names`]): archive sources (`.zip`/`.7z`)
//!   carry each entry's forward-slashed relative path in `name(i)`, so the
//!   whole tree — including every count — groups out of the already-in-RAM
//!   list with no I/O at all. Every row is clickable: folder rows carry the
//!   [`TreeTarget::Scope`] prefix the deck re-scopes to (the root row `""`,
//!   back to the whole archive), and the up row the caller prepends opens the
//!   folder on disk containing the archive.
//!
//! Pure data in/out (the disk half isolated in one function), so the grouping
//! logic is unit-tested without a filesystem.

use pb_hud::hud::TreeRow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Cap on ancestor rows shown between the root and the current folder. A deeper
/// chain collapses its *middle* into one dim "…" marker row, keeping the root
/// and the nearest ancestors — the ends are what orient you; the middle of a
/// pathological nesting is noise.
const MAX_ANCESTORS: usize = 4;

/// What clicking a tree row opens: a folder on disk (full Open Folder
/// semantics — re-roots the deck at it), or an internal archive folder as a
/// forward-slashed entry-name prefix the deck **re-scopes** to (`""` = back to
/// the whole archive). The prefix stays a plain string because it is not a
/// filesystem path — nothing is ever extracted to open it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TreeTarget {
    Dir(PathBuf),
    Scope(String),
}

/// The derived tree: display rows plus, index-aligned, what clicking each row
/// opens (`None` = not clickable: the "…" collapse markers).
pub struct FolderTreeModel {
    pub rows: Vec<TreeRow>,
    pub targets: Vec<Option<TreeTarget>>,
}

impl FolderTreeModel {
    /// Prepend the "up to the parent" affordance row — the parent's real name
    /// with the folder-up glyph. `target` is the directory clicking it opens
    /// (the grandparent for a disk deck; the folder containing the archive for
    /// an archive deck — stepping back *inside* a scoped archive is the root
    /// row's job, which re-scopes to `""`).
    pub fn push_up(&mut self, name: &str, target: PathBuf) {
        self.rows.insert(
            0,
            TreeRow {
                depth: 0,
                name: name.to_string(),
                open: false,
                current: false,
                marker: false,
                up: true,
                count: None,
            },
        );
        self.targets.insert(0, Some(TreeTarget::Dir(target)));
    }
}

/// The containing folder of a forward-slashed relative path: everything before
/// the last `/`, or `""` when the entry sits at the root level.
pub fn folder_of(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

/// The first path segment of `dir` strictly below `base` (`""` = the root), or
/// `None` when `dir` is not under `base` (or *is* `base`). This is what makes an
/// entry at `a/b/c/x.jpg` contribute the folder `b` to `a`'s children.
fn child_segment<'a>(dir: &'a str, base: &str) -> Option<&'a str> {
    let rest = if base.is_empty() {
        dir
    } else {
        dir.strip_prefix(base)?.strip_prefix('/')?
    };
    if rest.is_empty() {
        return None;
    }
    rest.split('/').next()
}

/// Case-insensitive sort for display (folder names read alphabetically
/// regardless of case; ties break case-sensitively so the order is total).
fn sort_names(mut v: Vec<String>) -> Vec<String> {
    v.sort_unstable_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    v
}

fn folder(depth: u32, name: &str, open: bool, current: bool) -> TreeRow {
    TreeRow {
        depth,
        name: name.to_string(),
        open,
        current,
        marker: false,
        up: false,
        count: None,
    }
}

/// The structural role of each assembled row — what lets the disk deriver map
/// rows to click-target paths (and both derivers attach counts) **by
/// construction** instead of re-guessing the layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Root,
    Marker,
    /// `lists[level][index]` — a folder at `level` (0 = directly under the root).
    At {
        level: usize,
        index: usize,
    },
}

/// Where the level walk starts: with a path longer than this allows, the
/// shallow levels (and their sibling lists) fold into the dim "…" marker row,
/// bounding both the row count and the indent of a pathological nesting. `k` =
/// the path length root → current; the marker appears once more than
/// [`MAX_ANCESTORS`] named levels would show.
fn collapse_start(k: usize) -> usize {
    if k > MAX_ANCESTORS + 1 {
        k - MAX_ANCESTORS
    } else {
        0
    }
}

/// Assemble the display rows from the level structure: `lists[i]` holds the
/// sorted folder names at level `i` (directly under the path prefix of length
/// `i`; `lists.len() == chain.len() + 1`, the last being the current folder's
/// children) and `chain[i]` names the on-path folder within `lists[i]`. **All**
/// folders at every visible level show, with the "you are here" path expanding
/// in place — the owner's ~/Pictures catch (2026-07-03): a recursive open lands
/// on a deep first photo, and with only the current level listed, the root's
/// other folders were unreachable. Levels shallower than [`collapse_start`]
/// fold into the "…" marker. An empty chain = the current folder IS the root.
fn assemble(
    root_label: &str,
    lists: &[Vec<String>],
    chain: &[String],
) -> (Vec<TreeRow>, Vec<Role>) {
    let mut rows = vec![folder(0, root_label, true, chain.is_empty())];
    let mut roles = vec![Role::Root];
    let start = collapse_start(chain.len());
    let mut depth = 1u32;
    if start > 0 {
        rows.push(TreeRow {
            depth,
            name: "\u{2026}".to_string(),
            open: false,
            current: false,
            marker: true,
            up: false,
            count: None,
        });
        roles.push(Role::Marker);
        depth += 1;
    }
    emit(lists, chain, start, depth, &mut rows, &mut roles);
    (rows, roles)
}

/// Recursive level walk: emit every folder at `level`; the on-path one expands
/// into the next level immediately after its own row (the tree-view order).
fn emit(
    lists: &[Vec<String>],
    chain: &[String],
    level: usize,
    depth: u32,
    rows: &mut Vec<TreeRow>,
    roles: &mut Vec<Role>,
) {
    let Some(names_at) = lists.get(level) else {
        return;
    };
    for (index, name) in names_at.iter().enumerate() {
        let on_path = chain.get(level).map(String::as_str) == Some(name.as_str());
        let current = on_path && level + 1 == chain.len();
        rows.push(folder(depth, name, on_path, current));
        roles.push(Role::At { level, index });
        if on_path {
            emit(lists, chain, level + 1, depth + 1, rows, roles);
        }
    }
}

/// Build the tree from a flat list of forward-slashed relative entry paths (an
/// archive's `name(i)` list, already in RAM — no I/O, and every row gets its
/// under-prefix photo count for free). `current` is the current photo's
/// containing folder (`""` = the archive root); `root_label` is the display
/// name for the root level (the archive's file name). Every folder row is
/// clickable: its target is the [`TreeTarget::Scope`] prefix the deck re-scopes
/// to — the root row carries `""`, the way back to the whole archive.
pub fn rows_from_names<'a>(
    names: impl Iterator<Item = &'a str>,
    current: &str,
    root_label: &str,
) -> FolderTreeModel {
    let chain: Vec<String> = if current.is_empty() {
        Vec::new()
    } else {
        current.split('/').map(str::to_string).collect()
    };
    let k = chain.len();
    let start = collapse_start(k);
    // One path prefix per visible level (`""` for level 0, `"a"`, `"a/b"`, …);
    // a single pass over the entries fills every level's folder set + counts.
    let prefixes: Vec<String> = (start..=k).map(|i| chain[..i].join("/")).collect();
    let mut total = 0u64;
    let mut level_counts: Vec<HashMap<String, u64>> = vec![HashMap::new(); k + 1];
    for n in names {
        total += 1;
        let dir = folder_of(n);
        for (j, p) in prefixes.iter().enumerate() {
            if let Some(seg) = child_segment(dir, p) {
                *level_counts[start + j].entry(seg.to_string()).or_insert(0) += 1;
            }
        }
    }
    let lists: Vec<Vec<String>> = level_counts
        .iter()
        .map(|m| sort_names(m.keys().cloned().collect()))
        .collect();
    let (mut rows, roles) = assemble(root_label, &lists, &chain);
    for (row, role) in rows.iter_mut().zip(&roles) {
        row.count = match role {
            Role::Root => Some(total),
            Role::Marker => None,
            Role::At { level, index } => level_counts[*level].get(&lists[*level][*index]).copied(),
        };
    }
    let targets = roles
        .iter()
        .map(|role| match role {
            Role::Root => Some(TreeTarget::Scope(String::new())),
            Role::Marker => None,
            Role::At { level, index } => {
                // The row's entry-name prefix: the on-path chain down to its
                // level, then its own name (`chain[..level]` is always intact —
                // only sibling *lists* collapse into the "…" marker).
                let mut p = chain[..*level].join("/");
                if !p.is_empty() {
                    p.push('/');
                }
                p.push_str(&lists[*level][*index]);
                Some(TreeTarget::Scope(p))
            }
        })
        .collect();
    FolderTreeModel { rows, targets }
}

/// The adjacent sibling folder-prefix of `prefix` (`step` = ±1) within an
/// archive's entry names — the Go ▸ previous/next analog of
/// [`sibling_with_photos`], over the same sorted sibling row the tree shows.
/// Pure and in-RAM (the names are already resident), so unlike the disk
/// version it needs no off-thread worker — and no photo probe either: archive
/// folders derive from image entry names, so a photo-less one can't exist.
/// `None` at the ends of the row, or for the archive root (`""` has no
/// siblings inside the archive).
pub fn sibling_scope<'a>(
    names: impl Iterator<Item = &'a str>,
    prefix: &str,
    step: i32,
) -> Option<String> {
    if prefix.is_empty() {
        return None;
    }
    let parent = folder_of(prefix);
    let mut sibs: Vec<String> = Vec::new();
    for n in names {
        if let Some(seg) = child_segment(folder_of(n), parent) {
            if !sibs.iter().any(|s| s == seg) {
                sibs.push(seg.to_string());
            }
        }
    }
    let sibs = sort_names(sibs);
    let own = prefix.rsplit('/').next()?;
    let pos = sibs.iter().position(|s| s == own)?;
    let next = pos as i64 + step as i64;
    if next < 0 || next as usize >= sibs.len() {
        return None; // at the end of the row of folders — nothing to step to
    }
    let name = &sibs[next as usize];
    Some(if parent.is_empty() {
        name.clone()
    } else {
        format!("{parent}/{name}")
    })
}

/// The non-hidden subdirectory names of `dir`, display-sorted. Read-only; a
/// missing/unreadable directory just yields an empty list. `pub(crate)` for the
/// Go sibling commands, which step through the same listing the tree shows.
pub(crate) fn subdirs(dir: &Path) -> Vec<String> {
    let v = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| {
                let e = e.ok()?;
                if !e.file_type().ok()?.is_dir() {
                    return None;
                }
                if entry_hidden(&e) {
                    return None;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                (!name.starts_with('.')).then_some(name)
            })
            .collect()
        })
        .unwrap_or_default();
    sort_names(v)
}

/// Windows hides folders by *attribute*, not by name — `$RECYCLE.BIN`, `System
/// Volume Information`, `found.000` all carry FILE_ATTRIBUTE_HIDDEN/SYSTEM and
/// never appear in Explorer, so they mustn't appear in the tree either (the
/// dot-prefix filter above only covers the Unix convention).
#[cfg(windows)]
fn entry_hidden(e: &std::fs::DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN: u32 = 0x2; // FILE_ATTRIBUTE_HIDDEN
    const SYSTEM: u32 = 0x4; // FILE_ATTRIBUTE_SYSTEM
    e.metadata()
        .map(|m| m.file_attributes() & (HIDDEN | SYSTEM) != 0)
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn entry_hidden(_e: &std::fs::DirEntry) -> bool {
    false
}

/// The rebuild identity of a photo's containing folder against the deck root:
/// root-relative and forward-slashed when the photo lives under the root, the
/// **absolute** parent path otherwise. The fallback matters on explicit
/// multi-folder decks (two folders dropped at once — the root is only the first
/// one): collapsing every out-of-root photo to `""` made the ⇧F tree's
/// signature blind to folder changes, so it kept showing the wrong hierarchy.
pub fn folder_identity(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) => folder_of(&rel.to_string_lossy().replace('\\', "/")).to_string(),
        Err(_) => path
            .parent()
            .map(|d| d.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default(),
    }
}

/// A path component's display name (falls back to the full path for a
/// filesystem root like `/`).
pub fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Aggregate under-prefix photo counts for a disk deck: each photo path counts
/// toward its containing folder and every ancestor up to (and including)
/// `root`. Free for a recursive deck — it's one pass over the in-RAM playlist;
/// plain decks skip counts instead of paying a `read_dir` per row.
pub fn disk_counts<'a>(
    paths: impl Iterator<Item = &'a Path>,
    root: &Path,
) -> HashMap<PathBuf, u64> {
    let mut map: HashMap<PathBuf, u64> = HashMap::new();
    for p in paths {
        let mut d = p.parent();
        while let Some(dir) = d {
            if !dir.starts_with(root) {
                break;
            }
            *map.entry(dir.to_path_buf()).or_insert(0) += 1;
            if dir == root {
                break;
            }
            d = dir.parent();
        }
    }
    map
}

/// Build the tree **without touching the disk**: sibling/child folder names come
/// from the keys of a [`disk_counts`] map (every photo-bearing folder in the
/// deck) instead of `read_dir`. The hold-to-blaze fast path — the tree keeps
/// tracking the current folder mid-flight at pure in-RAM cost; photo-less
/// folders (which only `read_dir` can see) appear when flight settles and the
/// full derivation re-runs.
pub fn rows_from_paths(root: &Path, dir: &Path, counts: &HashMap<PathBuf, u64>) -> FolderTreeModel {
    let names_under = |base: &Path| -> Vec<String> {
        sort_names(
            counts
                .keys()
                .filter(|k| k.parent() == Some(base))
                .map(|k| name_of(k))
                .collect(),
        )
    };
    rows_from_listings(root, dir, names_under, Some(counts))
}

/// Build the tree for a real on-disk photo whose containing folder is `dir`:
/// the display is anchored one level **above** the opened `root` — the parent
/// is the heading and the root sits expanded among its siblings, so clicking a
/// sibling (which re-roots the deck at it) still leaves the *new* root's
/// neighbours one click away. Below the root, every level down to `dir` lists
/// all its folders with the on-path one expanding in place. Levels come from
/// one `read_dir` each; the "up" row exits one more level (the grandparent).
/// Every row is clickable (`targets` = the folder's absolute path); `counts`
/// decorates rows where the caller has them (see [`disk_counts`]) — the
/// parent-level rows sit outside the deck, so they carry none.
pub fn rows_from_disk(
    root: &Path,
    dir: &Path,
    counts: Option<&HashMap<PathBuf, u64>>,
) -> FolderTreeModel {
    rows_from_listings(root, dir, subdirs, counts)
}

/// The shared disk-shaped assembly over a pluggable folder-listing source —
/// `read_dir` for the settled tree, the deck's counts-map keys for the no-I/O
/// flight variant.
fn rows_from_listings(
    root: &Path,
    dir: &Path,
    list: impl Fn(&Path) -> Vec<String>,
    counts: Option<&HashMap<PathBuf, u64>>,
) -> FolderTreeModel {
    let chain: Vec<String> = match dir.strip_prefix(root) {
        Ok(rel) => rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    // Anchor: `dir` when it isn't under the root (explicit-list oddity), else `root`.
    let anchor = if chain.is_empty() && dir != root {
        dir
    } else {
        root
    };
    // Hoist the display one level above the deck root (owner catch, 2026-07-03):
    // clicking a sibling re-roots the deck at it, and without the parent's level
    // the NEW root's siblings vanished — adjacent hops needed a round trip
    // through "up" (which re-opens the parent and lands deep again). With the
    // parent as the heading and the root expanded among its siblings, adjacent
    // folders stay one click away after every hop. Deck semantics are untouched;
    // sibling rows are outside the deck, so they simply carry no counts. The up
    // row then exits to the grandparent.
    let (head, chain): (PathBuf, Vec<String>) =
        match anchor.parent().filter(|p| !p.as_os_str().is_empty()) {
            Some(par) => (
                par.to_path_buf(),
                std::iter::once(name_of(anchor)).chain(chain).collect(),
            ),
            None => (anchor.to_path_buf(), chain),
        };
    let anchor = head.as_path();
    let k = chain.len();
    // The directory each level lists (level i = folders under the prefix of length i).
    let level_dirs: Vec<PathBuf> = (0..=k)
        .map(|i| {
            chain[..i]
                .iter()
                .fold(anchor.to_path_buf(), |p, s| p.join(s))
        })
        .collect();
    let mut lists: Vec<Vec<String>> = vec![Vec::new(); k + 1];
    for i in collapse_start(k)..=k {
        let mut l = list(&level_dirs[i]);
        // A hidden or unreadable listing must still show where we are.
        if let Some(on_path) = chain.get(i) {
            if !l.contains(on_path) {
                l.push(on_path.clone());
                l = sort_names(l);
            }
        }
        lists[i] = l;
    }
    let (rows, roles) = assemble(&name_of(anchor), &lists, &chain);
    let targets: Vec<Option<TreeTarget>> = roles
        .iter()
        .map(|role| match role {
            Role::Root => Some(TreeTarget::Dir(anchor.to_path_buf())),
            Role::Marker => None,
            Role::At { level, index } => Some(TreeTarget::Dir(
                level_dirs[*level].join(&lists[*level][*index]),
            )),
        })
        .collect();
    let mut model = FolderTreeModel { rows, targets };
    if let Some(map) = counts {
        for (row, target) in model.rows.iter_mut().zip(&model.targets) {
            row.count = match target {
                Some(TreeTarget::Dir(d)) => map.get(d).copied(),
                _ => None,
            };
        }
    }
    if let Some(par) = anchor.parent().filter(|p| !p.as_os_str().is_empty()) {
        model.push_up(&name_of(par), par.to_path_buf());
    }
    model
}

// ── Off-thread disk derivation ───────────────────────────────────────────────
//
// `rows_from_disk` / `subdirs` are the only disk I/O the tree does, and they
// used to run inline on the event-loop thread — a spun-down drive or a dead
// network share stalled the whole app for seconds on ⇧F ("never block the
// event loop"). The callers now paint the no-I/O lite view immediately and hand
// the `read_dir` work to a short-lived worker; `AppCore::tick` polls the
// receiver (`work_pending` keeps the loop ticking) and installs the result when
// it lands. A superseded job's receiver is simply dropped — the worker's send
// fails and the thread exits.

/// What a tree-io worker delivers back to `tick`.
pub enum TreeIoResult {
    /// The settled `read_dir` view for the folder whose rebuild signature is `sig`.
    FullTree { sig: String, model: FolderTreeModel },
    /// The Go ▸ previous/next resolution: the deck root the search started from
    /// (so a result that outlives its deck is dropped, not opened — the probe
    /// can run for seconds on a slow share) and the first sibling **with
    /// photos** in that direction (`None` = nothing that way → the host
    /// toasts).
    Sibling {
        from_root: PathBuf,
        target: Option<PathBuf>,
    },
}

/// An in-flight tree-io job: the receiver `tick` polls, plus the full-tree
/// signature (`None` for a sibling job) so the settle-upgrade tick doesn't
/// re-kick a derivation that's already running. Dropping it (a superseding
/// job, teardown) flips the cancel flag the sibling skip-search checks per
/// walk entry — a rapid re-press *aborts* the old probe instead of orphaning
/// a potentially deep walk. (`pub` only because it rides a
/// struct-literal-constructed `AppCore` field.)
pub struct TreeIo {
    pub full_sig: Option<String>,
    pub rx: std::sync::mpsc::Receiver<TreeIoResult>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for TreeIo {
    fn drop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Test seam: a [`TreeIo`] over a hand-fed channel, so core tests can inject
/// worker results without threads or a filesystem.
#[cfg(test)]
pub(crate) fn tree_io_for_tests(rx: std::sync::mpsc::Receiver<TreeIoResult>) -> TreeIo {
    TreeIo {
        full_sig: None,
        rx,
        cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }
}

/// Derive the full `read_dir` tree for (`root`, `dir`) off-thread.
pub(crate) fn spawn_full_tree(
    root: PathBuf,
    dir: PathBuf,
    counts: Option<std::sync::Arc<HashMap<PathBuf, u64>>>,
    sig: String,
) -> TreeIo {
    let (tx, rx) = std::sync::mpsc::channel();
    let full_sig = Some(sig.clone());
    std::thread::spawn(move || {
        let model = rows_from_disk(&root, &dir, counts.as_deref());
        let _ = tx.send(TreeIoResult::FullTree { sig, model });
    });
    TreeIo {
        full_sig,
        rx,
        cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }
}

/// How many photo-less sibling candidates one ⌘←/→ press will probe before
/// giving up (a folder with thousands of empty siblings must not turn a
/// keypress into an unbounded search)…
const SIBLING_HOP_CAP: usize = 64;
/// …and the wall-clock ceiling across the whole search. The cap alone doesn't
/// bound the expensive case — ONE huge photo-less subtree must be enumerated
/// to conclude "empty" — so the deadline cuts that off too. Exhaustion of
/// either reports "nothing that way" (the toast), never a wrong answer.
const SIBLING_BUDGET: Duration = Duration::from_secs(3);

/// Resolve the Go ▸ previous/next (`step` = ∓1) target of `root` off-thread —
/// the listing (and even the `is_dir` stat) can stall on a dead share, and the
/// per-candidate photo probes (#49) can walk entire subtrees.
pub(crate) fn spawn_sibling(root: PathBuf, step: i32, show_archives: bool) -> TreeIo {
    let (tx, rx) = std::sync::mpsc::channel();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = std::sync::Arc::clone(&cancel);
    std::thread::spawn(move || {
        let deadline = Instant::now() + SIBLING_BUDGET;
        let target =
            sibling_with_photos(&root, step, show_archives, &seen, SIBLING_HOP_CAP, deadline);
        let _ = tx.send(TreeIoResult::Sibling {
            from_root: root,
            target,
        });
    });
    TreeIo {
        full_sig: None,
        rx,
        cancel,
    }
}

/// The nearest sibling directory of `root` **containing at least one photo**
/// (anywhere under it — matching what the recursive open would find), stepping
/// through the parent's sorted listing in `step`'s direction and skipping
/// photo-less candidates (#49: an empty folder must not dead-end ⌘←/→ behind
/// a modal). `None` at the row's end, for an unlisted root, or when the hop
/// cap / deadline / `cancel` cuts the search short — never a folder without
/// photos, so the caller's open can't hit "No supported images".
fn sibling_with_photos(
    root: &Path,
    step: i32,
    show_archives: bool,
    cancel: &std::sync::atomic::AtomicBool,
    cap: usize,
    deadline: Instant,
) -> Option<PathBuf> {
    if !root.is_dir() {
        return None;
    }
    let parent = root.parent().filter(|p| !p.as_os_str().is_empty())?;
    let sibs = subdirs(parent);
    let name = name_of(root);
    let pos = sibs.iter().position(|s| *s == name)?;
    let mut i = pos as i64 + step as i64;
    let mut hops = 0usize;
    while i >= 0 && (i as usize) < sibs.len() && hops < cap {
        let candidate = parent.join(&sibs[i as usize]);
        match crate::scan::dir_has_image(&candidate, show_archives, cancel, deadline) {
            crate::scan::Probe::Found => return Some(candidate),
            crate::scan::Probe::Empty => {} // skip it — keep stepping
            crate::scan::Probe::Aborted => return None,
        }
        i += step as i64;
        hops += 1;
    }
    None // the row's end (or the cap) — nothing with photos that way
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(rows: &[TreeRow]) -> Vec<(u32, &str, bool, bool)> {
        rows.iter()
            .map(|r| (r.depth, r.name.as_str(), r.open, r.current))
            .collect()
    }

    /// The disk path a row opens, if it is a disk target.
    fn dir_of(t: &Option<TreeTarget>) -> Option<&Path> {
        match t {
            Some(TreeTarget::Dir(d)) => Some(d.as_path()),
            _ => None,
        }
    }

    #[test]
    fn folder_of_splits_on_the_last_slash() {
        assert_eq!(folder_of("a/b/c.jpg"), "a/b");
        assert_eq!(folder_of("c.jpg"), "");
        assert_eq!(folder_of(""), "");
    }

    #[test]
    fn archive_names_show_the_chain_with_counts() {
        let entries = [
            "a/one.jpg",
            "a/b/two.JPG",
            "a/b/c/three.jpg",
            "a/b/x/four.jpg",
            "a/d/five.jpg",
            "top.jpg",
        ];
        // Current = a/b: root, ancestor chain (a), siblings of b under a (b, d),
        // children of b (c, x) under the highlighted row.
        let m = rows_from_names(entries.iter().copied(), "a/b", "trip.zip");
        assert_eq!(
            names(&m.rows),
            vec![
                (0, "trip.zip", true, false),
                (1, "a", true, false),
                (2, "b", true, true),
                (3, "c", false, false),
                (3, "x", false, false),
                (2, "d", false, false),
            ]
        );
        // Under-prefix counts: root=6 total, a=5, b=3 (two.JPG + c + x), c=1, x=1, d=1.
        let counts: Vec<Option<u64>> = m.rows.iter().map(|r| r.count).collect();
        assert_eq!(
            counts,
            vec![Some(6), Some(5), Some(3), Some(1), Some(1), Some(1)]
        );
        // Every row re-scopes the deck to its entry-name prefix; the root row
        // is the way back to the whole archive.
        let scopes: Vec<Option<&str>> = m
            .targets
            .iter()
            .map(|t| match t {
                Some(TreeTarget::Scope(p)) => Some(p.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            scopes,
            vec![
                Some(""),
                Some("a"),
                Some("a/b"),
                Some("a/b/c"),
                Some("a/b/x"),
                Some("a/d"),
            ]
        );
    }

    #[test]
    fn current_at_archive_root_lists_top_level_folders() {
        let entries = ["top.jpg", "a/one.jpg", "B/two.jpg", "a/deep/x.jpg"];
        let m = rows_from_names(entries.iter().copied(), "", "trip.zip");
        assert_eq!(
            names(&m.rows),
            vec![
                (0, "trip.zip", true, true),
                (1, "a", false, false),
                (1, "B", false, false),
            ]
        );
        let counts: Vec<Option<u64>> = m.rows.iter().map(|r| r.count).collect();
        assert_eq!(counts, vec![Some(4), Some(2), Some(1)]);
    }

    #[test]
    fn deep_chains_collapse_their_middle_into_a_marker() {
        let entry = "l1/l2/l3/l4/l5/l6/deep/img.jpg";
        let m = rows_from_names([entry].into_iter(), "l1/l2/l3/l4/l5/l6/deep", "x.zip");
        // Root, "…" marker, then the nearest MAX_ANCESTORS-1 ancestors, then current.
        assert_eq!(
            names(&m.rows),
            vec![
                (0, "x.zip", true, false),
                (1, "\u{2026}", false, false),
                (2, "l4", true, false),
                (3, "l5", true, false),
                (4, "l6", true, false),
                (5, "deep", true, true),
            ]
        );
        assert!(m.rows[1].marker, "the collapse row is a marker");
        assert_eq!(m.rows.iter().filter(|r| r.marker).count(), 1);
        // Scope prefixes stay full paths even below the collapsed levels — the
        // chain is intact, only the sibling lists folded into the marker.
        let scopes: Vec<Option<&str>> = m
            .targets
            .iter()
            .map(|t| match t {
                Some(TreeTarget::Scope(p)) => Some(p.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            scopes,
            vec![
                Some(""),
                None, // the "…" marker stays unclickable
                Some("l1/l2/l3/l4"),
                Some("l1/l2/l3/l4/l5"),
                Some("l1/l2/l3/l4/l5/l6"),
                Some("l1/l2/l3/l4/l5/l6/deep"),
            ]
        );
    }

    #[test]
    fn sibling_scope_steps_through_the_sorted_folder_row() {
        let entries = [
            "trip/Alpha/1.jpg",
            "trip/beta/2.jpg",
            "trip/Gamma/3.jpg",
            "other/x.jpg",
        ];
        let sib = |p: &str, step| sibling_scope(entries.iter().copied(), p, step);
        // Case-insensitive display order: Alpha, beta, Gamma.
        assert_eq!(sib("trip/beta", 1), Some("trip/Gamma".to_string()));
        assert_eq!(sib("trip/beta", -1), Some("trip/Alpha".to_string()));
        assert_eq!(sib("trip/Gamma", 1), None, "at the end of the row");
        assert_eq!(sib("trip/Alpha", -1), None);
        // Top-level scopes step among the root's folders (parent = "").
        assert_eq!(sib("other", 1), Some("trip".to_string()));
        assert_eq!(sib("trip", -1), Some("other".to_string()));
        // The archive root has no siblings inside the archive.
        assert_eq!(sib("", 1), None);
    }

    #[test]
    fn sorting_is_case_insensitive() {
        assert_eq!(
            sort_names(vec!["Zed".into(), "apple".into(), "Beta".into()]),
            vec!["apple".to_string(), "Beta".into(), "Zed".into()]
        );
    }

    #[test]
    fn disk_rows_chain_from_the_opened_root_with_targets() {
        // A throwaway tree under the OS temp dir; no dev-dep needed.
        let base = std::env::temp_dir().join(format!("pb-ftree-{}", std::process::id()));
        let root = base.join("Photos");
        let cur = root.join("parent").join("current");
        for d in ["sib-a", "current/kid", "sib-b"] {
            std::fs::create_dir_all(root.join("parent").join(d)).unwrap();
        }
        let m = rows_from_disk(&root, &cur, None);
        std::fs::remove_dir_all(&base).unwrap();
        // Display hoisted one level: heading = the root's PARENT, with the root
        // expanded among its siblings (adjacent hops stay one click away after a
        // re-root); the up row exits to the grandparent.
        assert!(m.rows[0].up, "the up affordance leads");
        assert_eq!(dir_of(&m.targets[0]), base.parent());
        assert!(
            m.rows[1].name.starts_with("pb-ftree-"),
            "heading = the root's parent"
        );
        assert_eq!(dir_of(&m.targets[1]), Some(base.as_path()));
        assert_eq!(
            names(&m.rows)[2..],
            vec![
                (1, "Photos", true, false),
                (2, "parent", true, false),
                (3, "current", true, true),
                (4, "kid", false, false),
                (3, "sib-a", false, false),
                (3, "sib-b", false, false),
            ]
        );
        // Every folder row opens its absolute path.
        let expect = [
            root.clone(),
            root.join("parent"),
            cur.clone(),
            cur.join("kid"),
            root.join("parent/sib-a"),
            root.join("parent/sib-b"),
        ];
        for (i, want) in expect.iter().enumerate() {
            assert_eq!(dir_of(&m.targets[i + 2]), Some(want.as_path()), "row {i}");
        }
    }

    #[test]
    fn disk_current_at_the_root_shows_subfolders_and_the_up_row() {
        let base = std::env::temp_dir().join(format!("pb-ftree-root-{}", std::process::id()));
        let root = base.join("Trips");
        std::fs::create_dir_all(root.join("inner")).unwrap();
        // dir == root: the display still hoists to the parent, so the opened root
        // sits current among its siblings with its subfolders below.
        let m = rows_from_disk(&root, &root, None);
        std::fs::remove_dir_all(&base).unwrap();
        assert_eq!(m.rows.len(), 4);
        assert!(m.rows[0].up, "up exits to the grandparent");
        assert_eq!(dir_of(&m.targets[0]), base.parent());
        assert_eq!(dir_of(&m.targets[1]), Some(base.as_path()));
        assert!(
            m.rows[2].current && m.rows[2].open,
            "the opened root is current"
        );
        assert_eq!(m.rows[2].name, "Trips");
        assert_eq!(dir_of(&m.targets[2]), Some(root.as_path()));
        assert_eq!(m.rows[3].name, "inner");
        assert_eq!(m.rows[3].depth, 2);
        let inner = root.join("inner");
        assert_eq!(dir_of(&m.targets[3]), Some(inner.as_path()));
    }

    #[test]
    fn every_level_lists_all_its_folders() {
        // The ~/Pictures repro (owner, 2026-07-03): a recursive open lands on a
        // deep first photo, and with only the current level listed, the root's
        // other folders (2000s/2010s) were invisible — unreachable by click.
        let entries = [
            "1990s/1990-12-24/a.jpg",
            "1990s/1990-12-25/b.jpg",
            "2000s/x.jpg",
            "2010s/y/z.jpg",
        ];
        let m = rows_from_names(entries.iter().copied(), "1990s/1990-12-24", "Pictures");
        assert_eq!(
            names(&m.rows),
            vec![
                (0, "Pictures", true, false),
                (1, "1990s", true, false),
                (2, "1990-12-24", true, true),
                (2, "1990-12-25", false, false),
                (1, "2000s", false, false),
                (1, "2010s", false, false),
            ]
        );
        let counts: Vec<Option<u64>> = m.rows.iter().map(|r| r.count).collect();
        assert_eq!(
            counts,
            vec![Some(4), Some(2), Some(1), Some(1), Some(1), Some(1)]
        );
    }

    #[test]
    fn rows_from_paths_derives_the_tree_without_io() {
        let root = PathBuf::from("/r");
        let paths = ["/r/a/1.jpg", "/r/a/b/2.jpg", "/r/c/3.jpg"].map(PathBuf::from);
        let map = disk_counts(paths.iter().map(|p| p.as_path()), &root);
        let m = rows_from_paths(&root, Path::new("/r/a"), &map);
        // Hoisted heading = "/" (the root's parent — no up row above a filesystem
        // root), then r → siblings (a current, c) → child b, with under-prefix
        // counts where the deck knows them, no read_dir anywhere.
        assert!(
            !m.rows.iter().any(|r| r.up),
            "a filesystem-root heading has no up row"
        );
        assert_eq!(
            names(&m.rows),
            vec![
                (0, "/", true, false),
                (1, "r", true, false),
                (2, "a", true, true),
                (3, "b", false, false),
                (2, "c", false, false),
            ]
        );
        let counts: Vec<Option<u64>> = m.rows.iter().map(|r| r.count).collect();
        assert_eq!(counts, vec![None, Some(3), Some(2), Some(1), Some(1)]);
        let b = PathBuf::from("/r/a/b");
        assert_eq!(dir_of(&m.targets[3]), Some(b.as_path()));
    }

    #[test]
    fn folder_identity_distinguishes_out_of_root_folders() {
        let root = Path::new("/deck/root");
        // Under the root: relative, forward-slashed, file name dropped.
        assert_eq!(
            folder_identity(Path::new("/deck/root/a/b/x.jpg"), root),
            "a/b"
        );
        assert_eq!(folder_identity(Path::new("/deck/root/x.jpg"), root), "");
        // Out of root (multi-folder deck): the absolute parent, so two different
        // folders never collapse to the same signature.
        let a = folder_identity(Path::new("/other/one/x.jpg"), root);
        let b = folder_identity(Path::new("/other/two/y.jpg"), root);
        assert_ne!(a, b);
        assert_eq!(a, "/other/one");
    }

    #[test]
    fn sibling_search_skips_photo_less_folders() {
        use std::sync::atomic::AtomicBool;
        let base = std::env::temp_dir().join(format!("pb-ftree-sib-{}", std::process::id()));
        // alpha: photo deep inside (a candidate counts if ANY descendant has
        // one, matching the recursive open) / beta: a nested photo-less
        // subtree / gamma: photo at the top / delta: truly empty.
        std::fs::create_dir_all(base.join("alpha/deep")).unwrap();
        std::fs::write(base.join("alpha/deep/a.jpg"), b"x").unwrap();
        std::fs::create_dir_all(base.join("beta/nested/empty")).unwrap();
        std::fs::write(base.join("beta/nested/notes.txt"), b"x").unwrap();
        std::fs::create_dir_all(base.join("gamma")).unwrap();
        std::fs::write(base.join("gamma/g.png"), b"x").unwrap();
        std::fs::create_dir_all(base.join("delta")).unwrap();

        let live = AtomicBool::new(false);
        let far = Instant::now() + Duration::from_secs(60);
        let sib = |from: &str, step, cancel: &AtomicBool, cap| {
            sibling_with_photos(&base.join(from), step, true, cancel, cap, far)
        };
        // beta is photo-less → skipped in both directions.
        assert_eq!(sib("alpha", 1, &live, 64), Some(base.join("gamma")));
        assert_eq!(sib("gamma", -1, &live, 64), Some(base.join("alpha")));
        // Past gamma there is only photo-less delta, then the row ends.
        assert_eq!(sib("gamma", 1, &live, 64), None);
        assert_eq!(sib("alpha", -1, &live, 64), None);
        // The hop cap bounds the probing (cap 1 = only beta) — the answer is
        // "nothing found", never a photo-less folder.
        assert_eq!(sib("alpha", 1, &live, 1), None);
        // A pre-set cancel flag (a superseding press, teardown) aborts cleanly.
        let cancelled = AtomicBool::new(true);
        assert_eq!(sib("alpha", 1, &cancelled, 64), None);
        // An expired deadline likewise aborts rather than mislabeling.
        assert_eq!(
            sibling_with_photos(
                &base.join("alpha"),
                1,
                true,
                &live,
                64,
                Instant::now() - Duration::from_secs(1)
            ),
            None
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn subdirs_skips_attribute_hidden_folders() {
        let base = std::env::temp_dir().join(format!("pb-ftree-hidden-{}", std::process::id()));
        std::fs::create_dir_all(base.join("visible")).unwrap();
        std::fs::create_dir_all(base.join("stealth")).unwrap();
        // Set FILE_ATTRIBUTE_HIDDEN the supported-tooling way (std can't write attributes).
        let status = std::process::Command::new("attrib")
            .arg("+h")
            .arg(base.join("stealth"))
            .status()
            .unwrap();
        assert!(status.success(), "attrib +h failed");
        assert_eq!(subdirs(&base), vec!["visible".to_string()]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn disk_counts_aggregate_up_to_the_root() {
        let root = PathBuf::from("/r");
        let paths = [
            PathBuf::from("/r/a/1.jpg"),
            PathBuf::from("/r/a/b/2.jpg"),
            PathBuf::from("/r/c/3.jpg"),
            PathBuf::from("/elsewhere/4.jpg"), // not under the root — ignored
        ];
        let map = disk_counts(paths.iter().map(|p| p.as_path()), &root);
        assert_eq!(map.get(Path::new("/r")).copied(), Some(3));
        assert_eq!(map.get(Path::new("/r/a")).copied(), Some(2));
        assert_eq!(map.get(Path::new("/r/a/b")).copied(), Some(1));
        assert_eq!(map.get(Path::new("/r/c")).copied(), Some(1));
        assert!(!map.keys().any(|k| k.starts_with("/elsewhere")));
    }
}
