//! **Session archive-password cache** — the `AppCore` half of [`crate::secret`]
//! (task #125; the two-halves rule in `docs/where-code-goes.md`).
//!
//! ⚠ Read the privacy guarantee in the root `CLAUDE.md` before touching any of this. These
//! hold `SecretString`s: zeroized on drop, redacted `Debug`, never `Display`ed or
//! serialized, and deliberately NOT a `Settings` field so `settings.save()` cannot write
//! them. The cache is wiped at teardown. Nothing here may grow a persistence path.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Remember a password that just unlocked an encrypted archive — used for BOTH **harvest**
    /// (a new user-entered password) and **MRU promotion** (a cached password that just
    /// worked). Deduped (an existing equal entry moves to the front), empty ignored, truncated
    /// to [`MAX_ARCHIVE_PASSWORDS`](Self::MAX_ARCHIVE_PASSWORDS). RAM-only, never persisted.
    pub fn remember_archive_password(&mut self, pw: &crate::SecretString) {
        if pw.is_empty() {
            return;
        }
        self.archive_passwords.retain(|p| p != pw);
        self.archive_passwords.insert(0, pw.clone());
        self.archive_passwords.truncate(Self::MAX_ARCHIVE_PASSWORDS);
    }

    /// A MRU-ordered snapshot of the session passwords for the shell's archive-open worker to
    /// auto-try before prompting. Cheap — at most [`MAX_ARCHIVE_PASSWORDS`](Self::MAX_ARCHIVE_PASSWORDS)
    /// short strings.
    pub fn archive_passwords_snapshot(&self) -> Vec<crate::SecretString> {
        self.archive_passwords.clone()
    }

    /// Wipe the session password cache (teardown). The `Vec` drop zeroizes each entry; doing
    /// it explicitly keeps the privacy guarantee auditable and covers a shell that terminates
    /// via `exit()` without running `Drop` (macOS).
    pub fn clear_archive_passwords(&mut self) {
        self.archive_passwords.clear();
    }
}
