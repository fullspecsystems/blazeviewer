//! Folder-tree derivation for the `Shift+F` overlay (phase 1: render-only).
//!
//! Builds the [`TreeRow`] list the HUD rasterizes, anchored at **the root
//! PhotoBlaze has open** (the opened folder / scan root / archive) — never
//! above it: the root heading, the **ancestor chain** down to the current
//! photo's folder (the "you are here" path; a chain deeper than
//! [`MAX_ANCESTORS`] collapses its middle into one dim "…" marker row), then
//! the current folder's **siblings** (current highlighted) and its
//! **children** one level deeper.
//!
//! Two derivations, one shape:
//! - **Disk** ([`rows_from_disk`]): the ancestor chain comes from the path
//!   components between the root and the current folder (no I/O); the sibling
//!   and child lists cost two `read_dir`s (parent + current folder),
//!   directories only. Read-only, and only ever run on the explicit `Shift+F`
//!   toggle or when the current folder changes while the overlay is open —
//!   never on the view/decode path (privacy #2).
//! - **Entry names** ([`rows_from_names`]): archive sources (`.zip`/`.7z`)
//!   carry each entry's forward-slashed relative path in `name(i)`, so the
//!   whole tree groups out of the already-in-RAM list — no I/O at all.
//!
//! Pure data in/out (the disk half isolated in one function), so the grouping
//! logic is unit-tested without a filesystem.

use pb_hud::hud::TreeRow;
use std::path::Path;

/// Cap on ancestor rows shown between the root and the current folder. A deeper
/// chain collapses its *middle* into one dim "…" marker row, keeping the root
/// and the nearest ancestors — the ends are what orient you; the middle of a
/// pathological nesting is noise.
const MAX_ANCESTORS: usize = 4;

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

/// Case-insensitive sort + dedup for display (folder names read alphabetically
/// regardless of case; ties break case-sensitively so the order is total).
fn sort_names(mut v: Vec<String>) -> Vec<String> {
    v.sort_unstable_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    v.dedup();
    v
}

fn folder(depth: u32, name: &str, open: bool, current: bool) -> TreeRow {
    TreeRow {
        depth,
        name: name.to_string(),
        open,
        current,
        marker: false,
    }
}

/// Assemble the display rows: the root heading, the ancestor chain (`ancestors`
/// is every folder strictly between the root and the current one, shallowest
/// first; collapsed past [`MAX_ANCESTORS`]), the current folder's `siblings`
/// (with `current` marked open + highlighted), and its `children` one level
/// deeper.
fn assemble(
    root_label: &str,
    ancestors: &[String],
    siblings: &[String],
    current: &str,
    children: &[String],
) -> Vec<TreeRow> {
    let mut rows = Vec::with_capacity(2 + ancestors.len() + siblings.len() + children.len());
    rows.push(folder(0, root_label, true, false));
    let mut depth = 1u32;
    let shown = if ancestors.len() > MAX_ANCESTORS {
        rows.push(TreeRow {
            depth,
            name: "\u{2026}".to_string(),
            open: false,
            current: false,
            marker: true,
        });
        depth += 1;
        // The marker plus the nearest (MAX_ANCESTORS - 1) keep the block at the cap.
        &ancestors[ancestors.len() - (MAX_ANCESTORS - 1)..]
    } else {
        ancestors
    };
    for a in shown {
        rows.push(folder(depth, a, true, false));
        depth += 1;
    }
    for s in siblings {
        let is_cur = s == current;
        rows.push(folder(depth, s, is_cur, is_cur));
        if is_cur {
            rows.extend(children.iter().map(|c| folder(depth + 1, c, false, false)));
        }
    }
    rows
}

/// The degenerate top-of-hierarchy shape (the photo lives at the root itself):
/// the root as the current heading, its folders nested one level in.
fn assemble_root(root_label: &str, children: &[String]) -> Vec<TreeRow> {
    let mut rows = vec![folder(0, root_label, true, true)];
    rows.extend(children.iter().map(|c| folder(1, c, false, false)));
    rows
}

/// Build the tree rows from a flat list of forward-slashed relative entry paths
/// (an archive's `name(i)` list, already in RAM — no I/O). `current` is the
/// current photo's containing folder (`""` = the archive root); `root_label` is
/// the display name for the root level (the archive's file name).
pub fn rows_from_names<'a>(
    names: impl Iterator<Item = &'a str>,
    current: &str,
    root_label: &str,
) -> Vec<TreeRow> {
    let parent = folder_of(current);
    let mut siblings = Vec::new();
    let mut children = Vec::new();
    for n in names {
        let dir = folder_of(n);
        if let Some(seg) = child_segment(dir, parent) {
            siblings.push(seg.to_string());
        }
        if !current.is_empty() {
            if let Some(seg) = child_segment(dir, current) {
                children.push(seg.to_string());
            }
        }
    }
    let children = sort_names(children);
    if current.is_empty() {
        // `siblings` here are the root's own folders (base == "").
        return assemble_root(root_label, &sort_names(siblings));
    }
    let mut chain: Vec<String> = current.split('/').map(str::to_string).collect();
    let cur_name = chain.pop().unwrap_or_default();
    assemble(
        root_label,
        &chain,
        &sort_names(siblings),
        &cur_name,
        &children,
    )
}

/// The non-hidden subdirectory names of `dir`, display-sorted. Read-only; a
/// missing/unreadable directory just yields an empty list.
fn subdirs(dir: &Path) -> Vec<String> {
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
fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Build the tree rows for a real on-disk photo whose containing folder is
/// `dir`, anchored at `root` (the folder PhotoBlaze opened — the tree never
/// walks above it): the ancestor chain is `dir`'s components below `root` (no
/// I/O), siblings come from one `read_dir` on `dir`'s parent, children from one
/// on `dir` itself. When `dir` *is* the root (a plain non-recursive open) — or
/// isn't under it (an explicit file list) — the tree is just `dir` + its
/// subfolders.
pub fn rows_from_disk(root: &Path, dir: &Path) -> Vec<TreeRow> {
    let children = subdirs(dir);
    let chain: Vec<String> = match dir.strip_prefix(root) {
        Ok(rel) => rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    let Some((cur, ancestors)) = chain.split_last() else {
        return assemble_root(&name_of(dir), &children);
    };
    let mut siblings = dir.parent().map(subdirs).unwrap_or_default();
    // A hidden or unreadable parent listing must still show where we are.
    if !siblings.contains(cur) {
        siblings.push(cur.clone());
        siblings = sort_names(siblings);
    }
    assemble(&name_of(root), ancestors, &siblings, cur, &children)
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
    fn archive_names_show_the_chain_from_the_root() {
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
        let rows = rows_from_names(entries.iter().copied(), "a/b", "trip.zip");
        assert_eq!(
            names(&rows),
            vec![
                (0, "trip.zip", true, false),
                (1, "a", true, false),
                (2, "b", true, true),
                (3, "c", false, false),
                (3, "x", false, false),
                (2, "d", false, false),
            ]
        );
    }

    #[test]
    fn current_at_archive_root_lists_top_level_folders() {
        let entries = ["top.jpg", "a/one.jpg", "B/two.jpg", "a/deep/x.jpg"];
        let rows = rows_from_names(entries.iter().copied(), "", "trip.zip");
        assert_eq!(
            names(&rows),
            vec![
                (0, "trip.zip", true, true),
                (1, "a", false, false),
                (1, "B", false, false),
            ]
        );
    }

    #[test]
    fn deep_chains_collapse_their_middle_into_a_marker() {
        let entry = "l1/l2/l3/l4/l5/l6/deep/img.jpg";
        let rows = rows_from_names([entry].into_iter(), "l1/l2/l3/l4/l5/l6/deep", "x.zip");
        // Root, "…" marker, then the nearest MAX_ANCESTORS-1 ancestors, then current.
        assert_eq!(
            names(&rows),
            vec![
                (0, "x.zip", true, false),
                (1, "\u{2026}", false, false),
                (2, "l4", true, false),
                (3, "l5", true, false),
                (4, "l6", true, false),
                (5, "deep", true, true),
            ]
        );
        assert!(rows[1].marker, "the collapse row is a marker");
        assert!(rows.iter().filter(|r| r.marker).count() == 1);
    }

    #[test]
    fn sibling_prefixes_do_not_collide() {
        // "ab" must not read as a sibling under "a"'s parent chain confusion.
        let entries = ["a/one.jpg", "ab/two.jpg", "a/sub/three.jpg"];
        let rows = rows_from_names(entries.iter().copied(), "a", "x.zip");
        assert_eq!(
            names(&rows),
            vec![
                (0, "x.zip", true, false),
                (1, "a", true, true),
                (2, "sub", false, false),
                (1, "ab", false, false),
            ]
        );
    }

    #[test]
    fn sorting_is_case_insensitive() {
        assert_eq!(
            sort_names(vec!["Zed".into(), "apple".into(), "Beta".into()]),
            vec!["apple".to_string(), "Beta".into(), "Zed".into()]
        );
    }

    #[test]
    fn disk_rows_chain_from_the_opened_root() {
        // A throwaway tree under the OS temp dir; no dev-dep needed.
        let root = std::env::temp_dir().join(format!("pb-ftree-{}", std::process::id()));
        let cur = root.join("parent").join("current");
        for d in ["sib-a", "current/kid", "sib-b"] {
            std::fs::create_dir_all(root.join("parent").join(d)).unwrap();
        }
        let rows = rows_from_disk(&root, &cur);
        let root_name = rows[0].name.clone();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(root_name.starts_with("pb-ftree-"), "anchored at the root");
        assert_eq!(
            names(&rows)[1..],
            vec![
                (1, "parent", true, false),
                (2, "current", true, true),
                (3, "kid", false, false),
                (2, "sib-a", false, false),
                (2, "sib-b", false, false),
            ]
        );
    }

    #[test]
    fn disk_current_at_the_root_shows_only_its_subfolders() {
        let root = std::env::temp_dir().join(format!("pb-ftree-root-{}", std::process::id()));
        std::fs::create_dir_all(root.join("inner")).unwrap();
        // dir == root: the tree never walks above what PhotoBlaze opened.
        let rows = rows_from_disk(&root, &root);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows[0].current && rows[0].open,
            "root is the current folder"
        );
        assert_eq!(rows[1].name, "inner");
        assert_eq!(rows[1].depth, 1);
    }
}
