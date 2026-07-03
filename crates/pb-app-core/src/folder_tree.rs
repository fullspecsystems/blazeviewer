//! Folder-tree derivation for the `Shift+F` overlay — phase 2: the rows plus what
//! **clicking** each row opens and (where free) per-folder photo counts.
//!
//! Builds the [`FolderTreeModel`] the HUD rasterizes and the click handler
//! consumes, anchored at **the root PhotoBlaze has open** (the opened folder /
//! scan root / archive) — never above it: an "up to the parent" affordance row,
//! the root heading, the **ancestor chain** down to the current photo's folder
//! (the "you are here" path; a chain deeper than [`MAX_ANCESTORS`] collapses its
//! middle into one dim "…" marker row), then the current folder's **siblings**
//! (current highlighted) and its **children** one level deeper.
//!
//! Two derivations, one shape:
//! - **Disk** ([`rows_from_disk`]): the ancestor chain comes from the path
//!   components between the root and the current folder (no I/O); the sibling
//!   and child lists cost two `read_dir`s (parent + current folder),
//!   directories only. Read-only, and only ever run on the explicit `Shift+F`
//!   toggle or when the current folder changes while the overlay is open —
//!   never on the view/decode path (privacy #2). Counts come from the caller's
//!   optional [`disk_counts`] map (recursive decks — free from the in-RAM
//!   playlist; plain decks pass `None` rather than pay a `read_dir` per row).
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
    /// Index into the FULL ancestors list (pre-collapse).
    Ancestor(usize),
    /// Index into the sorted sibling list.
    Sibling(usize),
    /// Index into the sorted child list.
    Child(usize),
}

/// Assemble the display rows: the root heading, the ancestor chain (`ancestors`
/// is every folder strictly between the root and the current one, shallowest
/// first; collapsed past [`MAX_ANCESTORS`]), the current folder's `siblings`
/// (with `current` marked open + highlighted), and its `children` one level
/// deeper. Returns each row's [`Role`] alongside.
fn assemble(
    root_label: &str,
    ancestors: &[String],
    siblings: &[String],
    current: &str,
    children: &[String],
) -> (Vec<TreeRow>, Vec<Role>) {
    let mut rows = Vec::with_capacity(2 + ancestors.len() + siblings.len() + children.len());
    let mut roles = Vec::with_capacity(rows.capacity());
    rows.push(folder(0, root_label, true, false));
    roles.push(Role::Root);
    let mut depth = 1u32;
    let shown: Vec<usize> = if ancestors.len() > MAX_ANCESTORS {
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
        // The marker plus the nearest (MAX_ANCESTORS - 1) keep the block at the cap.
        (ancestors.len() - (MAX_ANCESTORS - 1)..ancestors.len()).collect()
    } else {
        (0..ancestors.len()).collect()
    };
    for i in shown {
        rows.push(folder(depth, &ancestors[i], true, false));
        roles.push(Role::Ancestor(i));
        depth += 1;
    }
    for (si, s) in siblings.iter().enumerate() {
        let is_cur = s == current;
        rows.push(folder(depth, s, is_cur, is_cur));
        roles.push(Role::Sibling(si));
        if is_cur {
            for (ci, c) in children.iter().enumerate() {
                rows.push(folder(depth + 1, c, false, false));
                roles.push(Role::Child(ci));
            }
        }
    }
    (rows, roles)
}

/// The degenerate top-of-hierarchy shape (the photo lives at the root itself,
/// or the deck is empty): the root as the current heading, its folders nested
/// one level in.
fn assemble_root(root_label: &str, children: &[String]) -> (Vec<TreeRow>, Vec<Role>) {
    let mut rows = vec![folder(0, root_label, true, true)];
    let mut roles = vec![Role::Root];
    for (ci, c) in children.iter().enumerate() {
        rows.push(folder(1, c, false, false));
        roles.push(Role::Child(ci));
    }
    (rows, roles)
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
    let parent = folder_of(current);
    // The ancestor chain (all folders strictly between root and current) and its
    // cumulative prefixes, for per-ancestor counts.
    let chain: Vec<String> = if current.is_empty() {
        Vec::new()
    } else {
        current.split('/').map(str::to_string).collect()
    };
    let (cur_name, ancestors) = match chain.split_last() {
        Some((c, a)) => (c.clone(), a.to_vec()),
        None => (String::new(), Vec::new()),
    };
    let anc_prefixes: Vec<String> = (0..ancestors.len())
        .map(|i| ancestors[..=i].join("/"))
        .collect();

    let mut total = 0u64;
    let mut anc_counts = vec![0u64; ancestors.len()];
    let mut sibling_counts: HashMap<String, u64> = HashMap::new();
    let mut child_counts: HashMap<String, u64> = HashMap::new();
    for n in names {
        total += 1;
        let dir = folder_of(n);
        if let Some(seg) = child_segment(dir, parent) {
            *sibling_counts.entry(seg.to_string()).or_insert(0) += 1;
        }
        if !current.is_empty() {
            if let Some(seg) = child_segment(dir, current) {
                *child_counts.entry(seg.to_string()).or_insert(0) += 1;
            }
        }
        for (i, p) in anc_prefixes.iter().enumerate() {
            if dir == p
                || dir
                    .strip_prefix(p.as_str())
                    .is_some_and(|r| r.starts_with('/'))
            {
                anc_counts[i] += 1;
            }
        }
    }

    let siblings = sort_names(sibling_counts.keys().cloned().collect());
    let children = sort_names(child_counts.keys().cloned().collect());
    let (mut rows, roles) = if current.is_empty() {
        // `siblings` here are the root's own folders (base == "").
        assemble_root(root_label, &siblings)
    } else {
        assemble(root_label, &ancestors, &siblings, &cur_name, &children)
    };
    for (row, role) in rows.iter_mut().zip(&roles) {
        row.count = match role {
            Role::Root => Some(total),
            Role::Marker => None,
            Role::Ancestor(i) => Some(anc_counts[*i]),
            // At the root level the "children" of assemble_root came from the
            // sibling grouping (base == ""), so counts come from that map too.
            Role::Sibling(i) => sibling_counts.get(&siblings[*i]).copied(),
            Role::Child(i) => {
                if current.is_empty() {
                    sibling_counts.get(&siblings[*i]).copied()
                } else {
                    child_counts.get(&children[*i]).copied()
                }
            }
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
    let children = list(dir);
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
    let (rows, targets) = match chain.split_last() {
        None => {
            let (rows, roles) = assemble_root(&name_of(anchor), &children);
            let targets: Vec<Option<PathBuf>> = roles
                .iter()
                .map(|role| match role {
                    Role::Root => Some(anchor.to_path_buf()),
                    Role::Child(i) => Some(anchor.join(&children[*i])),
                    _ => None,
                })
                .collect();
            let _ = roles;
            (rows, targets)
        }
        Some((cur, ancestors)) => {
            let parent = dir.parent().unwrap_or(root);
            let mut siblings = list(parent);
            // A hidden or unreadable parent listing must still show where we are.
            if !siblings.contains(cur) {
                siblings.push(cur.clone());
                siblings = sort_names(siblings);
            }
            // Each ancestor's absolute path (cumulative joins below the root).
            let anc_paths: Vec<PathBuf> = (0..ancestors.len())
                .map(|i| {
                    ancestors[..=i]
                        .iter()
                        .fold(root.to_path_buf(), |p, s| p.join(s))
                })
                .collect();
            let (rows, roles) = assemble(&name_of(root), ancestors, &siblings, cur, &children);
            let targets: Vec<Option<PathBuf>> = roles
                .iter()
                .map(|role| match role {
                    Role::Root => Some(root.to_path_buf()),
                    Role::Marker => None,
                    Role::Ancestor(i) => Some(anc_paths[*i].clone()),
                    Role::Sibling(i) => Some(parent.join(&siblings[*i])),
                    Role::Child(i) => Some(dir.join(&children[*i])),
                })
                .collect();
            let _ = roles;
            (rows, targets)
        }
    };
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
