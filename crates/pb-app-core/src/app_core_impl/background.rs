//! **Background-operation orchestration** — the `AppCore` half of [`crate::background`]
//! (task #125; the two-halves rule in `docs/where-code-goes.md`).
//!
//! `background.rs` owns the one generation space shared by both operation kinds; this file
//! holds the `impl AppCore` transitions over it. Today that is just supersession — the
//! point where a dir scan and an archive open can displace each other — which is exactly
//! the cross-kind logic that belongs to neither sibling alone.

use super::*;

impl AppCore {
    /// Stop whatever operation `superseded` names. The core owns **both** workers now, so this
    /// is the one place cross-type supersession is performed as well as decided — the split
    /// that made it a per-call-site convention (and a recurring bug) is gone.
    pub(super) fn supersede(
        &mut self,
        superseded: Option<(crate::background::OpId, crate::background::OpKind)>,
    ) {
        match superseded {
            Some((_, crate::background::OpKind::DirScan)) => {
                if let Some(prev) = self.dir_scan.take() {
                    prev.request_cancel();
                }
                self.scanning = false;
            }
            Some((_, crate::background::OpKind::ArchiveOpen)) => {
                if let Some(prev) = self.archive_load.take() {
                    prev.request_cancel();
                }
            }
            None => {}
        }
    }
}
