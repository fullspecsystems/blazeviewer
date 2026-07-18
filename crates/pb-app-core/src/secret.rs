//! `SecretString` — a password held in RAM for the session (session-archive-password-cache).
//!
//! Three properties the plaintext `String` it replaces does not have:
//! - **Zeroized on drop** (`zeroize`), so a freed password buffer is wiped rather than left
//!   in reclaimed memory.
//! - **Redacted `Debug`** — prints `SecretString(…)`, never the value — so a password can ride
//!   through `#[derive(Debug)]` types (`DialogResult::PasswordSubmitted`,
//!   `CoreEffect::BeginArchiveOpen`) without a stray `{:?}`/log leaking it.
//! - **No `Display`, no `Serialize`** — it can't be formatted into user text or written to a
//!   settings/config file by accident (privacy #2).
//!
//! It is **not** a claim of protection against OS-level exposure of live process memory (a
//! kernel crash dump, swap, hibernation): a running program's RAM can always be captured by
//! the OS. This type protects the values the app *retains* (the session cache, the in-flight
//! open, the contract messages) from Rust-level leaks, not the momentary copy a decoder holds.

use zeroize::Zeroize;

/// A password kept in RAM for the session — zeroized on drop, redacted in `Debug`, never
/// `Display`ed or serialized. Construct with [`SecretString::new`] / `From`; read the plaintext
/// only at the point it is handed to a decoder via [`expose`](SecretString::expose).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a password value.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The plaintext — only for handing to an archive decoder (which needs a real password to
    /// derive keys). Keep the borrow short; do not clone it into anything retained or logged.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the password is empty (an empty entry is never cached / auto-tried).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for SecretString {
    /// Redacted — never prints the value (or its length), so a password riding a
    /// `#[derive(Debug)]` type can't leak through a log or `{:?}`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString(…)")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SecretString {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let s = SecretString::new("hunter2");
        let dbg = format!("{s:?}");
        assert!(
            !dbg.contains("hunter2"),
            "Debug must not print the password: {dbg}"
        );
        assert_eq!(dbg, "SecretString(…)");
        // The length must not leak either — the redaction is the same for any value.
        assert_eq!(format!("{:?}", SecretString::new("x")), dbg);
    }

    #[test]
    fn debug_is_redacted_inside_a_derived_debug_type() {
        // The property that matters: a password riding a `#[derive(Debug)]` container stays
        // redacted (this is why the contract enums carry `SecretString`, not `String`).
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Carrier(Option<SecretString>);
        let dbg = format!("{:?}", Carrier(Some(SecretString::new("s3cr3t"))));
        assert!(!dbg.contains("s3cr3t"), "{dbg}");
    }

    #[test]
    fn expose_round_trips_and_equality_works() {
        let a = SecretString::new("pw");
        assert_eq!(a.expose(), "pw");
        assert_eq!(a, SecretString::from("pw".to_string()));
        assert_ne!(a, SecretString::new("other"));
        assert!(SecretString::default().is_empty());
        assert!(!a.is_empty());
    }
}
