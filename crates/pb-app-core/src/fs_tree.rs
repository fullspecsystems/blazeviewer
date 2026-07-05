//! A **resident, lazily-populated folder tree** for the Finder-style folder browser
//! (task #54, owner design 2026-07-05) — the successor to the auto-derived path view in
//! [`folder_tree`](crate::folder_tree).
//!
//! ## Why this exists
//! The old tree re-derived the whole path from the current photo's folder every time the
//! folder changed, folded deep ancestors into a dead "…" row, and coupled *browsing* to
//! *loading photos* (a click re-rooted the deck). This model fixes all three:
//!
//! - **Browsing ≠ loading.** Expanding/collapsing a folder (the chevron) reads its
//!   subfolders and shows them — it never touches the photo deck. Opening a folder (a
//!   name click) is a separate command the shell routes to `open_dir`. So you can walk
//!   through photo-less folders to *find* the one you want.
//! - **Persistent + incremental.** The tree is kept resident and updated as the user
//!   navigates — a folder read once stays read; changing the current photo only re-marks
//!   the current row (and reveals its path), it doesn't re-walk the disk.
//! - **No jail.** The root can be extended upward ([`extend_root_up`](FsTree::extend_root_up))
//!   and any branch expanded, so navigation isn't boxed into the opened subtree.
//!
//! ## Purity / I-O split
//! This type is **pure state** — it never touches the filesystem itself. The shell does
//! the `read_dir` (off the event loop, so a slow share can't freeze a chevron) and hands
//! the result back via [`set_children`](FsTree::set_children); the model tracks expansion,
//! the current marker, and per-folder counts, and flattens to display [`Row`]s. That keeps
//! it unit-testable with no disk, and RAM-only (privacy #2 — folder paths + counts live in
//! memory like `meta_cache`, dropped on exit, never serialized).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Per-folder resident state. Absent from the map ⇒ never touched (an unread, collapsed
/// leaf); `Node::default()` models that.
#[derive(Default, Clone)]
struct Node {
    /// The folder's subfolders, sorted for display — `None` until `read_dir` lands.
    children: Option<Vec<PathBuf>>,
    /// Whether the user has expanded it (independent of whether children are loaded yet).
    expanded: bool,
    /// A photo count for the badge (may be stale; refreshed lazily by the shell).
    count: Option<u64>,
}

/// One flattened, visible tree row (depth-first over the expanded structure).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Row {
    pub path: PathBuf,
    pub name: String,
    /// Indent level (0 = the tree root).
    pub depth: u32,
    /// Expanded right now (its children, if any, follow).
    pub expanded: bool,
    /// Expanded but its children haven't been read yet (show a spinner).
    pub loading: bool,
    /// Worth a disclosure chevron: known to have subfolders, or unread (optimistic —
    /// expanding an empty folder then clears it).
    pub has_children: bool,
    /// The current photo's folder ("you are here").
    pub is_current: bool,
    pub count: Option<u64>,
}

/// The resident folder tree: a `root` path, the per-folder [`Node`] map, and the current
/// photo's folder. Pure — all `read_dir` results arrive via [`set_children`](Self::set_children).
pub struct FsTree {
    root: PathBuf,
    nodes: HashMap<PathBuf, Node>,
    current: Option<PathBuf>,
}

/// The display name of a folder path: its file name, or the whole path for a root like `/`.
pub fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

impl FsTree {
    /// A fresh tree rooted at `root`, expanded (so its children show once loaded).
    pub fn new(root: PathBuf) -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            root.clone(),
            Node {
                expanded: true,
                ..Node::default()
            },
        );
        FsTree {
            root,
            nodes,
            current: None,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The display name of the root's parent, or `None` at the filesystem root — the label
    /// for the "up to parent" row (clicking it [`extend_root_up`](Self::extend_root_up)s).
    pub fn parent_name(&self) -> Option<String> {
        self.root
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(display_name)
    }

    /// Whether `path`'s subfolders still need reading (expanded and unread) — the shell
    /// polls this to decide when to kick an off-thread `read_dir`.
    pub fn needs_children(&self, path: &Path) -> bool {
        self.nodes
            .get(path)
            .is_some_and(|n| n.expanded && n.children.is_none())
    }

    /// Install a folder's subfolders (an off-thread `read_dir` result). Sorted
    /// case-insensitively for stable display; idempotent (a re-read just refreshes).
    pub fn set_children(&mut self, path: &Path, mut children: Vec<PathBuf>) {
        children.sort_by(|a, b| {
            let (an, bn) = (
                display_name(a).to_lowercase(),
                display_name(b).to_lowercase(),
            );
            an.cmp(&bn).then_with(|| a.cmp(b))
        });
        self.nodes.entry(path.to_path_buf()).or_default().children = Some(children);
    }

    /// Set (or clear) a folder's photo-count badge.
    pub fn set_count(&mut self, path: &Path, count: Option<u64>) {
        self.nodes.entry(path.to_path_buf()).or_default().count = count;
    }

    /// Expand a folder (idempotent). Its children load lazily — [`needs_children`](Self::needs_children)
    /// then reports it until [`set_children`](Self::set_children) lands.
    pub fn expand(&mut self, path: &Path) {
        self.nodes.entry(path.to_path_buf()).or_default().expanded = true;
    }

    /// Collapse a folder (keeps its already-read children cached for a cheap re-expand).
    pub fn collapse(&mut self, path: &Path) {
        if let Some(n) = self.nodes.get_mut(path) {
            n.expanded = false;
        }
    }

    /// Toggle a folder's expansion — the chevron action.
    pub fn toggle(&mut self, path: &Path) {
        let n = self.nodes.entry(path.to_path_buf()).or_default();
        n.expanded = !n.expanded;
    }

    /// Mark `folder` as the current photo's folder and reveal it: expand every ancestor
    /// from the root down to it (so the highlight is visible). A folder outside the root
    /// is recorded but not revealed — the shell re-roots (or extends up) first.
    pub fn set_current(&mut self, folder: PathBuf) {
        self.current = Some(folder.clone());
        if let Ok(rel) = folder.strip_prefix(&self.root) {
            let mut p = self.root.clone();
            self.expand(&p.clone());
            for comp in rel.components() {
                p = p.join(comp);
                self.expand(&p.clone());
            }
        }
    }

    pub fn current(&self) -> Option<&Path> {
        self.current.as_deref()
    }

    /// Re-root one level up (the "up to parent" affordance): the parent becomes the new
    /// root, expanded, with the old root still expanded beneath it. No-op at the
    /// filesystem root. Returns whether it moved.
    pub fn extend_root_up(&mut self) -> bool {
        let Some(parent) = self.root.parent().map(Path::to_path_buf) else {
            return false;
        };
        if parent.as_os_str().is_empty() {
            return false;
        }
        self.nodes.entry(parent.clone()).or_default().expanded = true;
        self.root = parent;
        true
    }

    /// Flatten the expanded structure into display rows (depth-first, root first).
    pub fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        self.push_rows(&self.root, 0, &mut out);
        out
    }

    fn push_rows(&self, path: &Path, depth: u32, out: &mut Vec<Row>) {
        let node = self.nodes.get(path);
        let children = node.and_then(|n| n.children.as_ref());
        let expanded = node.is_some_and(|n| n.expanded);
        out.push(Row {
            path: path.to_path_buf(),
            name: display_name(path),
            depth,
            expanded,
            loading: expanded && children.is_none(),
            // Optimistic: unread folders show a chevron; an expanded-empty one clears it.
            has_children: children.is_none_or(|c| !c.is_empty()),
            is_current: self.current.as_deref() == Some(path),
            count: node.and_then(|n| n.count),
        });
        if expanded {
            if let Some(children) = children {
                for child in children {
                    self.push_rows(child, depth + 1, out);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn new_root_is_the_only_row_until_children_load() {
        let t = FsTree::new(p("/photos"));
        let rows = t.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "photos");
        assert_eq!(rows[0].depth, 0);
        assert!(rows[0].expanded, "root starts expanded");
        assert!(rows[0].loading, "…but its children aren't read yet");
    }

    #[test]
    fn children_show_sorted_and_indented_once_loaded() {
        let mut t = FsTree::new(p("/photos"));
        assert!(t.needs_children(&p("/photos")));
        t.set_children(&p("/photos"), vec![p("/photos/Zoo"), p("/photos/alps")]);
        assert!(!t.needs_children(&p("/photos")));
        let rows = t.rows();
        assert_eq!(rows.len(), 3);
        // Case-insensitive sort: "alps" before "Zoo".
        assert_eq!(rows[1].name, "alps");
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[2].name, "Zoo");
        assert!(!rows[0].loading, "root is read now");
    }

    #[test]
    fn collapse_hides_children_expand_reveals_without_reread() {
        let mut t = FsTree::new(p("/a"));
        t.set_children(&p("/a"), vec![p("/a/b")]);
        t.collapse(&p("/a"));
        assert_eq!(t.rows().len(), 1, "collapsed root hides its child");
        assert!(
            !t.needs_children(&p("/a")),
            "children stay cached while collapsed"
        );
        t.expand(&p("/a"));
        assert_eq!(t.rows().len(), 2, "re-expand needs no re-read");
    }

    #[test]
    fn nested_expand_and_empty_folder_clears_its_chevron() {
        let mut t = FsTree::new(p("/a"));
        t.set_children(&p("/a"), vec![p("/a/b")]);
        // /a/b is unread → optimistic chevron.
        assert!(t.rows()[1].has_children);
        t.expand(&p("/a/b"));
        t.set_children(&p("/a/b"), vec![]); // empty
        let rows = t.rows();
        assert_eq!(rows.len(), 2, "empty child adds no rows");
        assert!(
            !rows[1].has_children,
            "expanded-empty folder drops its chevron"
        );
    }

    #[test]
    fn set_current_reveals_the_path_and_marks_it() {
        let mut t = FsTree::new(p("/a"));
        // Nothing read yet; set_current expands the ancestor chain.
        t.set_current(p("/a/b/c"));
        // Load the chain so the rows materialize.
        t.set_children(&p("/a"), vec![p("/a/b")]);
        t.set_children(&p("/a/b"), vec![p("/a/b/c")]);
        t.set_children(&p("/a/b/c"), vec![]);
        let rows = t.rows();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert!(rows[2].is_current, "the current folder is marked");
        assert!(!rows[0].is_current);
    }

    #[test]
    fn extend_root_up_hoists_the_root_keeping_the_old_subtree() {
        let mut t = FsTree::new(p("/a/b"));
        t.set_children(&p("/a/b"), vec![p("/a/b/c")]);
        assert!(t.extend_root_up());
        assert_eq!(t.root(), p("/a"));
        // /a is expanded; its children unread until the shell reads them.
        assert!(t.needs_children(&p("/a")));
        t.set_children(&p("/a"), vec![p("/a/b"), p("/a/x")]);
        let rows = t.rows();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        // a → (b expanded → c), x
        assert_eq!(names, ["a", "b", "c", "x"]);
    }
}
