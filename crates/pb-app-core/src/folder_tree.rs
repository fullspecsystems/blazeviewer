//! Folder-tree derivation for the `Shift+F` overlay — phase 2: the rows plus what
//! **clicking** each row opens and (where free) per-folder photo counts.
//!
//! Builds the [`FolderTreeModel`] the HUD rasterizes and the click handler
//! consumes, anchored at **the root PhotoBlaze has open** (the opened folder /
//! scan root / archive) — never above it: an "up to the parent" affordance row,
//! the root heading, then **every folder at every level along the path** down
//! to the current photo's folder, with the on-path folder expanding in place
//! (the tree-view shape — so a recursive open that lands deep still leaves the
//! root's other folders one click away), the current folder highlighted, and
//! its children one level deeper. A path deeper than [`MAX_ANCESTORS`] + 1
//! levels folds its shallow levels into one dim "…" marker row.
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
//!   list with no I/O at all. Archive rows aren't clickable yet (`targets` =
//!   `None`; prefix re-scoping is the planned follow-up), but the up row the
//!   caller prepends can open the folder on disk containing the archive.
//!
//! Pure data in/out (the disk half isolated in one function), so the grouping
//! logic is unit-tested without a filesystem.

use pb_hud::hud::TreeRow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Cap on ancestor rows shown between the root and the current folder. A deeper
/// chain collapses its *middle* into one dim "…" marker row, keeping the root
/// and the nearest ancestors — the ends are what orient you; the middle of a
/// pathological nesting is noise.
const MAX_ANCESTORS: usize = 4;

/// The derived tree: display rows plus, index-aligned, what clicking each row
/// opens (`None` = not clickable: collapse markers, and archive rows until
/// prefix re-scoping lands).
pub struct FolderTreeModel {
    pub rows: Vec<TreeRow>,
    pub targets: Vec<Option<PathBuf>>,
}

impl FolderTreeModel {
    /// Prepend the "up to the parent" affordance row — the parent's real name
    /// with the folder-up glyph. `target` is what clicking it opens (the parent
    /// directory; for an archive deck, the folder containing the archive).
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
        self.targets.insert(0, Some(target));
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
/// name for the root level (the archive's file name). Rows aren't clickable
/// (`targets` all `None`) until archive re-scoping lands.
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
    FolderTreeModel {
        targets: vec![None; rows.len()],
        rows,
    }
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
                let name = e.file_name().to_string_lossy().into_owned();
                (!name.starts_with('.')).then_some(name)
            })
            .collect()
        })
        .unwrap_or_default();
    sort_names(v)
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
/// deck) instead of `read_dir`. The hold-to-fly fast path — the tree keeps
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

/// Build the tree for a real on-disk photo whose containing folder is `dir`,
/// anchored at `root` (the folder PhotoBlaze opened — the tree never walks
/// above it; the "up" row is the one deliberate exit): the ancestor chain is
/// `dir`'s components below `root` (no I/O), siblings come from one `read_dir`
/// on `dir`'s parent, children from one on `dir` itself. When `dir` *is* the
/// root (a plain non-recursive open, or an empty deck) — or isn't under it (an
/// explicit file list) — the tree is `dir` + its subfolders. Every row is
/// clickable (`targets` = the folder's absolute path); `counts` decorates rows
/// where the caller has them (see [`disk_counts`]).
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
    let targets: Vec<Option<PathBuf>> = roles
        .iter()
        .map(|role| match role {
            Role::Root => Some(anchor.to_path_buf()),
            Role::Marker => None,
            Role::At { level, index } => Some(level_dirs[*level].join(&lists[*level][*index])),
        })
        .collect();
    let mut model = FolderTreeModel { rows, targets };
    if let Some(map) = counts {
        for (row, target) in model.rows.iter_mut().zip(&model.targets) {
            row.count = target.as_ref().and_then(|t| map.get(t)).copied();
        }
    }
    if let Some(par) = anchor.parent().filter(|p| !p.as_os_str().is_empty()) {
        model.push_up(&name_of(par), par.to_path_buf());
    }
    model
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(rows: &[TreeRow]) -> Vec<(u32, &str, bool, bool)> {
        rows.iter()
            .map(|r| (r.depth, r.name.as_str(), r.open, r.current))
            .collect()
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
        // Archive rows aren't clickable yet.
        assert!(m.targets.iter().all(Option::is_none));
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
        // Up row first (the base dir), then root → chain → siblings/children.
        assert!(m.rows[0].up, "the up affordance leads");
        assert_eq!(m.targets[0].as_deref(), Some(base.as_path()));
        assert_eq!(
            names(&m.rows)[1..],
            vec![
                (0, "Photos", true, false),
                (1, "parent", true, false),
                (2, "current", true, true),
                (3, "kid", false, false),
                (2, "sib-a", false, false),
                (2, "sib-b", false, false),
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
            assert_eq!(m.targets[i + 1].as_deref(), Some(want.as_path()), "row {i}");
        }
    }

    #[test]
    fn disk_current_at_the_root_shows_subfolders_and_the_up_row() {
        let base = std::env::temp_dir().join(format!("pb-ftree-root-{}", std::process::id()));
        let root = base.join("Trips");
        std::fs::create_dir_all(root.join("inner")).unwrap();
        // dir == root: the tree never walks above what PhotoBlaze opened — except
        // the explicit up affordance.
        let m = rows_from_disk(&root, &root, None);
        std::fs::remove_dir_all(&base).unwrap();
        assert_eq!(m.rows.len(), 3);
        assert!(m.rows[0].up);
        assert_eq!(m.targets[0].as_deref(), Some(base.as_path()));
        assert!(m.rows[1].current && m.rows[1].open, "root is current");
        assert_eq!(m.targets[1].as_deref(), Some(root.as_path()));
        assert_eq!(m.rows[2].name, "inner");
        let inner = root.join("inner");
        assert_eq!(m.targets[2].as_deref(), Some(inner.as_path()));
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
        // Up row (the filesystem root), then r → siblings (a current, c) → child b,
        // all with under-prefix counts, no read_dir anywhere.
        assert!(m.rows[0].up);
        assert_eq!(
            names(&m.rows)[1..],
            vec![
                (0, "r", true, false),
                (1, "a", true, true),
                (2, "b", false, false),
                (1, "c", false, false),
            ]
        );
        let counts: Vec<Option<u64>> = m.rows[1..].iter().map(|r| r.count).collect();
        assert_eq!(counts, vec![Some(3), Some(2), Some(1), Some(1)]);
        let b = PathBuf::from("/r/a/b");
        assert_eq!(m.targets[3].as_deref(), Some(b.as_path()));
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
