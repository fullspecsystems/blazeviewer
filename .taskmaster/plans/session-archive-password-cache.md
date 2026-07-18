# Session archive-password cache (harvest + auto-try)

**Status:** planned (revised after Codex review) · **Owner-requested** 2026-07-17 · builds on
the shipped archive password flow (ZIP/7z/RAR5, tasks #30/#102/#103) and the door /
Show-Archives work (#104).

## Goal

When a user unlocks an encrypted archive, remember that password **for the session only**
(RAM, never persisted) and automatically try it (MRU-first, plus any other passwords used
this session) on the next encrypted archive **before** prompting. A folder of same-password
archives should ask once — matching 7-Zip / WinRAR / Keka / The Unarchiver.

## Non-negotiable constraints

1. **Privacy (ADR-018 / task #2).** RAM-only, session-lived, never written anywhere
   recoverable — not settings.toml, not a log, not a temp file, not a `Debug` string. Joins
   the RAM-cache inventory (dropped/wiped at teardown; the no-trace integration test must
   still pass — nothing new touches disk).
2. **Zeroize + redaction.** Passwords are more sensitive than the other RAM caches, so they
   live in a dedicated `SecretString` newtype: zeroized on drop, with a **redacted `Debug`**
   (`SecretString(…)`) and **no `Display`**. This also closes a *pre-existing* leak Codex
   found: `DialogResult::PasswordSubmitted(Option<String>)` and
   `CoreEffect::BeginArchiveOpen{ password: Option<String> }` derive `Debug` over plaintext
   today — threading `SecretString` through them redacts the whole in-app password path.
3. **No event-loop stall (owner: "don't slow down opening").** Auto-try runs **only** on
   `PasswordRequired` (an *encrypted* archive). Crucially, the sync ZIP password check is
   **not** cheap — `ZipSource::password_ok()` → `bytes()` decrypts and reads the entire first
   encrypted entry (up to ~1 GiB). So auto-try **always runs on a worker thread when the
   cache is non-empty**, never inline on the event loop. Empty cache (a fresh session, the
   first archive, every non-encrypted-archive session) = the existing code path, unchanged.
4. **No new user-facing control** (owner call). On always. **Silent on success** — no
   "trying saved password" message (dropped: it would need macOS FFI + Swift changes, and
   the owner said silent is enough).

## Honest scope of the privacy guarantee

The cache is never written to any app-controlled store, and is wiped on `Drop`/teardown. It
does **not** claim protection against OS-level exposure of live process memory (a kernel
crash dump, swap, hibernation) — that is outside the app's control and is the same exposure
as the password while the user is typing it. Comments/CLAUDE.md state this precisely rather
than overclaiming "unrecoverable from a crash dump."

## Design

### A. `SecretString` — pb-app-core/src/secret.rs (new)

```rust
/// A password held in RAM for the session. Zeroized on drop; Debug is redacted; no Display.
/// The archive libraries still receive a plain `&str`/`String` transiently to decrypt (an
/// unavoidable exposure, the same as the user typing it) — this type protects the values we
/// *retain* (the session cache, the in-flight open, the contract messages), not the momentary
/// copy inside a decoder.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
    pub fn expose(&self) -> &str { &self.0 }
    pub fn is_empty(&self) -> bool { self.0.is_empty() }
}
impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("SecretString(…)") }
}
impl Drop for SecretString {
    fn drop(&mut self) { use zeroize::Zeroize; self.0.zeroize(); }
}
impl From<String> for SecretString { fn from(s: String) -> Self { Self(s) } }
impl From<&str> for SecretString { fn from(s: &str) -> Self { Self(s.to_owned()) } }
```

- `zeroize` added to **pb-app-core** only (already in `Cargo.lock` transitively; pure Rust).
  `SecretString` is re-exported so the shells use `pb_app_core::SecretString` — they need no
  `zeroize` dep of their own (fixes Codex's cross-crate point).
- Derives `Clone/PartialEq/Eq` (needed because the contract enums derive them and for cache
  dedup); hand-writes `Debug` (redacted) and `Drop` (zeroize). No `Display`, no `Serialize`.

### B. The cache — on `AppCore`

`AppCore` is built via **full struct literals** in four places (`headless`
[app_core_impl.rs:88], `new_host` [:265], the winit shell [main.rs:749], the macOS shell),
so the new field is **`pub`** and each literal gets `archive_passwords: Vec::new()`:

```rust
/// Session-only passwords that have successfully unlocked encrypted archives (harvest +
/// auto-try). MRU-ordered, capped, `SecretString` (zeroized, redacted). **Never persisted**
/// (privacy #2) — dropped/wiped at teardown. Survives in-session navigation (that is the
/// point); cleared only by `clear_archive_passwords()` on teardown.
pub archive_passwords: Vec<SecretString>,
```

Methods on `AppCore`:

```rust
pub const MAX_ARCHIVE_PASSWORDS: usize = 8; // bound the all-miss auto-try cost

/// Remember a password that just unlocked an archive. Used for BOTH harvest (a new user-
/// entered password) and MRU promotion (a cached password that just worked): dedup removes an
/// existing equal entry, then it is inserted at the front; empty ignored; truncate to MAX.
pub fn remember_archive_password(&mut self, pw: &SecretString) {
    if pw.is_empty() { return; }
    self.archive_passwords.retain(|p| p != pw);
    self.archive_passwords.insert(0, pw.clone());
    self.archive_passwords.truncate(Self::MAX_ARCHIVE_PASSWORDS);
}

/// MRU-ordered snapshot to auto-try (cloned SecretStrings; ≤ MAX short strings).
pub fn archive_passwords_snapshot(&self) -> Vec<SecretString> { self.archive_passwords.clone() }

/// Wipe the cache (teardown). Vec drop zeroizes each entry; this makes it auditable + covers
/// a shell that calls exit() without running Drop (macOS).
pub fn clear_archive_passwords(&mut self) { self.archive_passwords.clear(); }
```

### C. Contract + dialog: `Option<String>` → `Option<SecretString>`

- `contract::DialogResult::PasswordSubmitted(Option<SecretString>)`.
- `contract::CoreEffect::BeginArchiveOpen { path, password: Option<SecretString> }`.
- `dialog::DialogWindow`: keep the live typing buffer `password_input: String` (egui binds a
  `&mut String`; it is scrubbed on close today), but `submitted_password: Option<SecretString>`
  — wrap at submit (`take_submitted_password() -> Option<SecretString>`).
- `app_core_impl` `PasswordSubmitted` handler [~:1043] and `open_plan`/archive arms [~:1226]
  build `BeginArchiveOpen{ password: Option<SecretString> }`. `password_archive` stays a
  `PathBuf` (not a secret).
- Update every construction/extraction site incl. the macOS shell and the mac-ffi tests that
  read the password out of `BeginArchiveOpen` / drive `PasswordSubmitted`
  ([lib.rs:5658/5669], [app_core_impl.rs:15075]).

### D. Auto-try helper — pb-app-core/src/scan.rs

```rust
/// Open an archive, auto-trying `cached` session passwords (MRU-first) when it is encrypted,
/// before giving up with `PasswordRequired`. Returns on the FIRST success: an unencrypted
/// archive (opens with `None`, `winner = None`) or a cached password that works (`winner =
/// Some(pw)`), or the first hard error. Only `PasswordRequired` advances to the next
/// candidate; a corrupt/too-large/unsupported/io error stops immediately. Bounded by
/// `cached.len()` (≤ MAX). Honours the progress cancel flag (checked BEFORE the first open and
/// between candidates) and resets the progress counters between attempts (they are cumulative).
/// The returned `winner` lets the shell promote/harvest it (MRU) — a plain `Ok` is not enough
/// to know which candidate matched.
pub fn load_archive_with_cache(
    path: &Path,
    kind: ArchiveKind,
    cached: &[SecretString],
    progress: &OpenProgress,
) -> (Result<Resolved, ArchiveOpenError>, Option<SecretString>) {
    if progress.is_cancelled() { return (Err(ArchiveOpenError::Cancelled), None); }
    match load_archive(path, kind, None, progress) {
        Err(ArchiveOpenError::PasswordRequired) => {}
        other => return (other, None), // Ok (unencrypted), or a non-password error → done
    }
    for pw in cached {
        if progress.is_cancelled() { return (Err(ArchiveOpenError::Cancelled), None); }
        progress.reset_counters(); // done/total are cumulative; each attempt starts fresh
        match load_archive(path, kind, Some(pw.expose().to_owned()), progress) {
            Err(ArchiveOpenError::PasswordRequired) => continue, // wrong → next
            Ok(r) => return (Ok(r), Some(pw.clone())),           // unlocked (maybe empty)
            Err(e) => return (Err(e), None),                     // hard error / cancelled
        }
    }
    (Err(ArchiveOpenError::PasswordRequired), None) // none worked → shell prompts (fresh)
}
```

Add to `pb_source::OpenProgress` (it already holds `done`/`total`/`cancel` atomics):

```rust
/// Reset the streamed/total counters (keep the cancel flag) — for reusing one handle across
/// several open attempts (the password-cache auto-try) so the bar restarts per attempt.
pub fn reset_counters(&self) {
    self.inner.done.store(0, Ordering::Relaxed);
    self.inner.total.store(0, Ordering::Relaxed);
}
```

### E. Shell wiring — `begin_archive_open` / `finish_archive_open` (winit + macOS)

`begin_archive_open(path, password: Option<SecretString>)`:

- `let cached = if password.is_none() { self.core.archive_passwords_snapshot() } else { Vec::new() };`
- **Route to the worker thread whenever an auto-try may run** — i.e. use the async worker for
  a background_open kind **or** whenever `password.is_none() && !cached.is_empty()`. Only the
  no-cache, no-password, non-background (ZIP) case keeps today's synchronous shortcut. This is
  what keeps the expensive ZIP `password_ok` off the event loop.
- Worker body: `let out = match password { Some(pw) => (load_archive(&p, kind, Some(pw.expose().to_owned()), &prog), None), None => load_archive_with_cache(&p, kind, &cached, &prog) };` and send `(generation, out)` where `out: (Result<Resolved,Err>, Option<SecretString>)`.
- Stash `attempted_password = password` (the user-entered one, if any) in `ArchiveLoad`.
- Replace `ArchiveLoad.was_password_attempt: bool` with `attempted_password: Option<SecretString>`; the channel item type becomes `(u64, (Result<Resolved, ArchiveOpenError>, Option<SecretString>))`.
- **macOS supersede fix (Codex):** align the mac path to `self.archive_load.take()` +
  `request_cancel()` like winit [main.rs:1098], so a superseded (esp. now-async ZIP) result
  can't overwrite a newer open. Flag for on-device retest.

`finish_archive_open(result: (Result<Resolved,Err>, Option<SecretString> winner), attempted: Option<SecretString>, path)`:

```rust
match result {
    (Ok(r), winner) => {
        // Harvest (new user password) OR MRU-promote (cached winner). Remember BEFORE the
        // empty check — a password that unlocked an *empty* archive is still correct.
        if let Some(pw) = attempted.as_ref().or(winner.as_ref()) {
            self.core.remember_archive_password(pw);
        }
        if r.source.is_empty() { self.fail_archive_open(&ArchiveOpenError::Empty); }
        else { self.close_dialog(); self.core.handle(CoreEvent::ArchiveResolved(r)); }
    }
    (Err(PasswordRequired), _) => self.prompt_archive_password(path, attempted.is_some()),
    (Err(Cancelled), _) => { self.core.password_archive = None; self.close_dialog(); }
    (Err(e), _) => self.fail_archive_open(&e),
}
```

`attempted.is_some()` reproduces the old `was_password_attempt` (a repeat = the user's entry
was wrong → re-prompt with the inline error). Auto-try misses are silent → a *fresh* prompt.

### F. Teardown — wipe in both shells

- **winit:** `clear_session_state` [main.rs:3637] adds `self.core.clear_archive_passwords();`
  (this is the exit/Esc teardown that already drops the RAM caches — it does NOT run on
  folder change / empty-state, so the cache correctly survives in-session navigation).
- **macOS:** call `self.core.clear_archive_passwords()` in the shell's teardown/quit path
  before it terminates (macOS may `exit(0)` and bypass `Drop`, so the explicit wipe matters).

### G. Formats

- **Covered:** ZIP, 7z, RAR5 (in-app decryption exists; both per-file `-p` and header `-hp`
  surface `PasswordRequired` from the `None` open and decrypt with `Some(pw)`).
- **Untouched:** the tar family (no in-app decryption) and **RAR4** (encrypted entries are
  refused *per-entry*; a fully header-encrypted RAR4 is `Unsupported` at open). Auto-try never
  engages because these don't return `PasswordRequired`.

### H. Out of scope / acknowledged (Codex)

pb-source's *internal* password handling is unchanged: `ZipSource` retains the raw password
for lazy reads (hand-scrubbed, not `zeroize`), 7z allocates a UTF-16 password, RAR keeps
derived AES `RunKey`s — all pre-existing and receive the password to decrypt by necessity.
Hardening those to `zeroize` is a separate follow-up; this feature does not worsen them and
adds no new *retained* plaintext beyond the `SecretString` cache.

## Files to change

- `crates/pb-app-core/Cargo.toml` — `zeroize` dep.
- `crates/pb-app-core/src/secret.rs` (new) + `lib.rs` re-export `SecretString`.
- `crates/pb-app-core/src/app_core.rs` — `archive_passwords` field.
- `crates/pb-app-core/src/app_core_impl.rs` — 4 struct-literal inits; `MAX_ARCHIVE_PASSWORDS`,
  `remember_archive_password`, `archive_passwords_snapshot`, `clear_archive_passwords`;
  `PasswordSubmitted`/`BeginArchiveOpen` now `SecretString`.
- `crates/pb-app-core/src/contract.rs` — `PasswordSubmitted` + `BeginArchiveOpen.password` → `Option<SecretString>`.
- `crates/pb-app-core/src/scan.rs` — `load_archive_with_cache`.
- `crates/pb-source/src/lib.rs` — `OpenProgress::reset_counters`.
- `crates/pb-app/src/main.rs` — `ArchiveLoad` field + channel type; `begin_archive_open`
  routing/worker/attempted; `finish_archive_open` signature + harvest; `clear_session_state`
  wipe; `PasswordSubmitted` construction in `dialog_event`.
- `crates/pb-app/src/dialog.rs` — `submitted_password: Option<SecretString>`, `take_submitted_password`.
- `crates/pb-mac-ffi/src/lib.rs` — mirror `ArchiveLoad`/`begin_archive_open`/`finish_archive_open`,
  supersede `take()`, teardown wipe, `PasswordSubmitted` construction, and the two direct tests.
- `CLAUDE.md` — privacy RAM-cache inventory: add the password cache (SecretString, never
  persisted, wiped on teardown; honest crash-dump qualification).
- `CHANGELOG.md` — one `Added` line.

## Tests

pb-app-core (pure / integration):
- **`SecretString`**: `Debug` prints no plaintext (`format!("{:?}", …)` contains `"…"`, not the
  password); equality works (dedup); `expose` round-trips.
- **Contract redaction**: `format!("{:?}", DialogResult::PasswordSubmitted(Some("hunter2".into())))`
  and `CoreEffect::BeginArchiveOpen{ password: Some("hunter2".into()), .. }` contain no
  plaintext.
- **`remember_archive_password`**: empty ignored; dedup moves to front (MRU); cap evicts oldest;
  snapshot is MRU-ordered; `clear_archive_passwords` empties it.
- **`load_archive_with_cache`** (temp fixtures via the `zip` crate's encryption writer; add the
  `aes-crypto`/encryption dev-feature if needed, else a committed fixture):
  - unencrypted → `(Ok, None)`, loop never entered;
  - encrypted, correct pw is **not first** in cache → `(Ok, Some(pw))`, wrong ones skipped;
  - encrypted, no matching / empty cache → `(Err(PasswordRequired), None)`;
  - hard error from the `None` open (corrupt) → returned as-is, not a password loop;
  - cancel set before the call → `(Err(Cancelled), None)` without any attempt;
  - **harvest-after-empty**: an encrypted archive that unlocks but has no images still returns
    the winning pw (so the shell remembers it).
- Existing no-trace `viewing_a_zip_writes_nothing_to_disk` family still passes (extend the note:
  unlocking an encrypted zip + harvesting a password writes nothing — a read-only decrypt).

Manual / on-device (macOS, real desktop):
- macOS async-open → newer synchronous(ZIP) supersession (the `take()` fix).
- Full format matrix: 7z `-p`/`-hp`, RAR5 `-p`/`-hp` same-password folder asks once.
- All-cache-miss timing on a large encrypted ZIP (worker stays responsive; event loop never
  blocks). Sanity-time an unencrypted open with a full cache vs empty — confirm no perceptible
  delta (the accurate claim: unchanged with an empty cache; one worker spawn otherwise).

## Manual test checklist (Windows unless noted)

Build: `pwsh scripts/build-windows.ps1 -Run`. Fixtures: a folder with **2+ encrypted
archives sharing one password** (make with 7-Zip: `.zip` AES, `.7z` `-p`, `.7z` `-hp`; RAR5
`-p` and `-hp`), plus one archive with a **different** password, plus normal unencrypted
archives and loose photos.

1. **Ask once per folder.** Open the first encrypted archive, enter the password → it opens.
   Go back out (`Alt+↑`) and open the next same-password archive → it opens **with no prompt**.
2. **Different password still prompts.** Open the odd-one-out archive → you get a **fresh**
   password prompt (not "incorrect password"), enter its password → opens; it's now remembered
   too.
3. **Wrong entry still re-prompts.** Open an encrypted archive, type a wrong password → the
   prompt returns with "incorrect password" (unchanged behaviour).
4. **Formats:** repeat #1 for `.7z -p`, `.7z -hp`, RAR5 `-p`, RAR5 `-hp` (header-encrypted).
5. **No slowdown on normal opens.** Opening loose photos, unencrypted `.zip`/`.7z`, and a big
   unencrypted ZIP feels exactly as before — even after several passwords are cached. The
   event loop never hitches (a large *encrypted* ZIP auto-trying wrong passwords stays
   responsive because it runs off-thread).
6. **Cancel during a slow auto-try** (a large encrypted 7z): the Cancel button / Esc on the
   "Opening…" dialog stops it promptly.
7. **Session boundary.** Quit and relaunch → the next encrypted archive prompts again (nothing
   was persisted). Confirm `settings.toml` contains no password text.
8. **macOS (on a Mac — the mac shell can't be built on Windows):** repeat #1–#3 and #7; plus
   the supersede fix — start opening a big encrypted 7z, immediately open a small unencrypted
   ZIP over it → the ZIP shows, the stale 7z result never replaces it.
