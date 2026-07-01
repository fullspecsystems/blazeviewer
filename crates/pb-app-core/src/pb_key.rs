//! `PbKey` — PhotoBlaze's own physical-key model (NS0, ADR-021).
//!
//! The keymap used to store `winit::keyboard::KeyCode` directly in every
//! `KeyChord`. That tied the otherwise-portable `Keymap`/`Action`/`KeyChord`
//! model to the winit shell, which blocks the macOS native-shell track (the
//! AppKit host delivers `NSEvent` keys, not winit ones). `PbKey` is the
//! app-owned, **shell-neutral** key enum the keymap now speaks; each shell
//! supplies a small adapter that maps *its* key type into `PbKey`
//! (winit: [`crate::pb_key_winit`]; a future AppKit one mirrors it).
//!
//! This module has **no winit dependency** on purpose — it is the part that
//! moves into the platform-neutral `pb-app-core` crate untouched. Variant names
//! intentionally mirror winit's `KeyCode` so the adapters are a 1:1 match and a
//! reviewer can diff the two lists at a glance.
//!
//! Only keys the keymap can actually name/persist live here (the set the old
//! `key_to_str`/`str_to_key` handled). Keys outside it — modifiers, exotic
//! function keys — are simply not representable, so an adapter returns `None`
//! and the shell ignores them (exactly as the old code did: an unnamed key
//! produced no usable binding).
//!
//! Privacy (task #2): pure data, no I/O — nothing here records anything.

use std::fmt;

/// A single physical key, identified by its position (layout-independent), for
/// the subset PhotoBlaze can bind, display, and persist. Mirrors the named
/// `winit::keyboard::KeyCode` variants; modifier keys are *not* included (they're
/// tracked as the chord's modifier flags, never as the key itself).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PbKey {
    Space,
    Backspace,
    Enter,
    NumpadEnter,
    Escape,
    Tab,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Equal,
    Minus,
    Comma,
    Period,
    Slash,
    Backslash,
    Semicolon,
    Quote,
    BracketLeft,
    BracketRight,
    Backquote,
    NumpadAdd,
    NumpadSubtract,
    NumpadMultiply,
    NumpadDivide,
    NumpadDecimal,
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    Numpad0,
    Numpad1,
    Numpad2,
    Numpad3,
    Numpad4,
    Numpad5,
    Numpad6,
    Numpad7,
    Numpad8,
    Numpad9,
    KeyA,
    KeyB,
    KeyC,
    KeyD,
    KeyE,
    KeyF,
    KeyG,
    KeyH,
    KeyI,
    KeyJ,
    KeyK,
    KeyL,
    KeyM,
    KeyN,
    KeyO,
    KeyP,
    KeyQ,
    KeyR,
    KeyS,
    KeyT,
    KeyU,
    KeyV,
    KeyW,
    KeyX,
    KeyY,
    KeyZ,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
}

impl PbKey {
    /// Config-friendly name for this key (`"Left"`, `"NumpadAdd"`, `"="`), used
    /// for parsing chords and for the help overlay. Total over `PbKey` — every
    /// variant is nameable (the old `key_to_str` needed a `"?"` fallback only
    /// because `KeyCode` had unnamed variants this enum doesn't model).
    pub fn as_str(self) -> &'static str {
        use PbKey::*;
        match self {
            Space => "Space",
            Backspace => "Backspace",
            Enter => "Enter",
            NumpadEnter => "NumpadEnter",
            Escape => "Esc",
            Tab => "Tab",
            Delete => "Delete",
            Insert => "Insert",
            Home => "Home",
            End => "End",
            PageUp => "PageUp",
            PageDown => "PageDown",
            ArrowLeft => "Left",
            ArrowRight => "Right",
            ArrowUp => "Up",
            ArrowDown => "Down",
            Equal => "=",
            Minus => "-",
            Comma => ",",
            Period => ".",
            Slash => "/",
            Backslash => "\\",
            Semicolon => ";",
            Quote => "'",
            BracketLeft => "[",
            BracketRight => "]",
            Backquote => "`",
            NumpadAdd => "NumpadAdd",
            NumpadSubtract => "NumpadSubtract",
            NumpadMultiply => "NumpadMultiply",
            NumpadDivide => "NumpadDivide",
            NumpadDecimal => "NumpadDecimal",
            Digit0 => "0",
            Digit1 => "1",
            Digit2 => "2",
            Digit3 => "3",
            Digit4 => "4",
            Digit5 => "5",
            Digit6 => "6",
            Digit7 => "7",
            Digit8 => "8",
            Digit9 => "9",
            Numpad0 => "Numpad0",
            Numpad1 => "Numpad1",
            Numpad2 => "Numpad2",
            Numpad3 => "Numpad3",
            Numpad4 => "Numpad4",
            Numpad5 => "Numpad5",
            Numpad6 => "Numpad6",
            Numpad7 => "Numpad7",
            Numpad8 => "Numpad8",
            Numpad9 => "Numpad9",
            KeyA => "A",
            KeyB => "B",
            KeyC => "C",
            KeyD => "D",
            KeyE => "E",
            KeyF => "F",
            KeyG => "G",
            KeyH => "H",
            KeyI => "I",
            KeyJ => "J",
            KeyK => "K",
            KeyL => "L",
            KeyM => "M",
            KeyN => "N",
            KeyO => "O",
            KeyP => "P",
            KeyQ => "Q",
            KeyR => "R",
            KeyS => "S",
            KeyT => "T",
            KeyU => "U",
            KeyV => "V",
            KeyW => "W",
            KeyX => "X",
            KeyY => "Y",
            KeyZ => "Z",
            F1 => "F1",
            F2 => "F2",
            F3 => "F3",
            F4 => "F4",
            F5 => "F5",
            F6 => "F6",
            F7 => "F7",
            F8 => "F8",
            F9 => "F9",
            F10 => "F10",
            F11 => "F11",
            F12 => "F12",
        }
    }

    /// Parse a key name (the final token of a chord) into a `PbKey`. Letters and
    /// digits are accepted case-insensitively; named keys match [`PbKey::as_str`]
    /// (plus a few friendly aliases). `None` for an unknown name.
    pub fn from_name(s: &str) -> Option<PbKey> {
        use PbKey::*;
        // Single letter or digit (case-insensitive for letters).
        if s.len() == 1 {
            let ch = s.chars().next().unwrap();
            if ch.is_ascii_alphabetic() {
                let idx = ch.to_ascii_uppercase() as u8 - b'A';
                const LETTERS: [PbKey; 26] = [
                    KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM,
                    KeyN, KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
                ];
                return Some(LETTERS[idx as usize]);
            }
            if ch.is_ascii_digit() {
                const DIGITS: [PbKey; 10] = [
                    Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
                ];
                return Some(DIGITS[(ch as u8 - b'0') as usize]);
            }
        }
        let m = match s.to_ascii_lowercase().as_str() {
            "space" => Space,
            "backspace" => Backspace,
            "enter" | "return" => Enter,
            "numpadenter" => NumpadEnter,
            "esc" | "escape" => Escape,
            "tab" => Tab,
            "delete" | "del" => Delete,
            "insert" | "ins" => Insert,
            "home" => Home,
            "end" => End,
            "pageup" | "pgup" => PageUp,
            "pagedown" | "pgdn" => PageDown,
            "left" => ArrowLeft,
            "right" => ArrowRight,
            "up" => ArrowUp,
            "down" => ArrowDown,
            "=" | "equal" | "plus" => Equal,
            "-" | "minus" => Minus,
            "," | "comma" => Comma,
            "." | "period" => Period,
            "/" | "slash" => Slash,
            "\\" | "backslash" => Backslash,
            ";" | "semicolon" => Semicolon,
            "'" | "quote" => Quote,
            "[" => BracketLeft,
            "]" => BracketRight,
            "`" => Backquote,
            "numpadadd" => NumpadAdd,
            "numpadsubtract" => NumpadSubtract,
            "numpadmultiply" => NumpadMultiply,
            "numpaddivide" => NumpadDivide,
            "numpaddecimal" => NumpadDecimal,
            "f1" => F1,
            "f2" => F2,
            "f3" => F3,
            "f4" => F4,
            "f5" => F5,
            "f6" => F6,
            "f7" => F7,
            "f8" => F8,
            "f9" => F9,
            "f10" => F10,
            "f11" => F11,
            "f12" => F12,
            _ => return None,
        };
        Some(m)
    }

    /// Is this a numeric-keypad key? Used to keep numpad aliases out of the help
    /// overlay (they're bound, just not worth showing next to the primary key).
    pub fn is_numpad(self) -> bool {
        self.as_str().starts_with("Numpad")
    }
}

impl fmt::Display for PbKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_round_trips_for_every_nameable_variant() {
        // Every key whose name `from_name` accepts must parse back to itself, so a
        // chord that prints as `X` reloads as `X` (the keymap.toml round-trip).
        // `as_str` is total (every variant is named); the numpad *digit* keys are a
        // long-standing display-only exception covered separately below.
        for key in ALL_FOR_TEST {
            let name = key.as_str();
            assert!(!name.is_empty(), "{key:?} has an empty name");
            if NUMPAD_DIGITS_NOT_PARSEABLE.contains(&key) {
                continue;
            }
            assert_eq!(
                PbKey::from_name(name),
                Some(key),
                "{key:?} prints as {name:?} but does not parse back",
            );
        }
    }

    #[test]
    fn numpad_digits_are_named_but_not_parseable() {
        // Inherited from the original `key_to_str`/`str_to_key` split: the keypad
        // digit keys have display names but no parser arm, so they can be shown yet
        // never bound from config. Preserved verbatim in the refactor (no default
        // binds them); documented here so a future change is a deliberate decision.
        for key in NUMPAD_DIGITS_NOT_PARSEABLE {
            assert!(key.as_str().starts_with("Numpad"));
            assert_eq!(PbKey::from_name(key.as_str()), None);
        }
    }

    #[test]
    fn from_name_is_case_insensitive_for_letters_and_aliases() {
        assert_eq!(PbKey::from_name("r"), Some(PbKey::KeyR));
        assert_eq!(PbKey::from_name("R"), Some(PbKey::KeyR));
        assert_eq!(PbKey::from_name("return"), Some(PbKey::Enter));
        assert_eq!(PbKey::from_name("ESCAPE"), Some(PbKey::Escape));
        assert_eq!(PbKey::from_name("plus"), Some(PbKey::Equal));
        assert_eq!(PbKey::from_name("pgdn"), Some(PbKey::PageDown));
    }

    #[test]
    fn from_name_rejects_unknown() {
        assert_eq!(PbKey::from_name("Nope"), None);
        assert_eq!(PbKey::from_name(""), None);
        assert_eq!(PbKey::from_name("F13"), None);
    }

    #[test]
    fn numpad_detection() {
        assert!(PbKey::NumpadAdd.is_numpad());
        assert!(PbKey::NumpadEnter.is_numpad());
        assert!(!PbKey::Enter.is_numpad());
        assert!(!PbKey::Equal.is_numpad());
    }

    /// The keypad digit keys: named by `as_str` but intentionally not accepted by
    /// `from_name` (the inherited display-only asymmetry).
    const NUMPAD_DIGITS_NOT_PARSEABLE: [PbKey; 10] = [
        PbKey::Numpad0,
        PbKey::Numpad1,
        PbKey::Numpad2,
        PbKey::Numpad3,
        PbKey::Numpad4,
        PbKey::Numpad5,
        PbKey::Numpad6,
        PbKey::Numpad7,
        PbKey::Numpad8,
        PbKey::Numpad9,
    ];

    /// Every variant, for exhaustive round-trip coverage. Kept in sync with the
    /// enum by the `as_str` match being total (a new variant there forces a name).
    const ALL_FOR_TEST: [PbKey; 90] = [
        PbKey::Space,
        PbKey::Backspace,
        PbKey::Enter,
        PbKey::NumpadEnter,
        PbKey::Escape,
        PbKey::Tab,
        PbKey::Delete,
        PbKey::Insert,
        PbKey::Home,
        PbKey::End,
        PbKey::PageUp,
        PbKey::PageDown,
        PbKey::ArrowLeft,
        PbKey::ArrowRight,
        PbKey::ArrowUp,
        PbKey::ArrowDown,
        PbKey::Equal,
        PbKey::Minus,
        PbKey::Comma,
        PbKey::Period,
        PbKey::Slash,
        PbKey::Backslash,
        PbKey::Semicolon,
        PbKey::Quote,
        PbKey::BracketLeft,
        PbKey::BracketRight,
        PbKey::Backquote,
        PbKey::NumpadAdd,
        PbKey::NumpadSubtract,
        PbKey::NumpadMultiply,
        PbKey::NumpadDivide,
        PbKey::NumpadDecimal,
        PbKey::Digit0,
        PbKey::Digit1,
        PbKey::Digit2,
        PbKey::Digit3,
        PbKey::Digit4,
        PbKey::Digit5,
        PbKey::Digit6,
        PbKey::Digit7,
        PbKey::Digit8,
        PbKey::Digit9,
        PbKey::Numpad0,
        PbKey::Numpad1,
        PbKey::Numpad2,
        PbKey::Numpad3,
        PbKey::Numpad4,
        PbKey::Numpad5,
        PbKey::Numpad6,
        PbKey::Numpad7,
        PbKey::Numpad8,
        PbKey::Numpad9,
        PbKey::KeyA,
        PbKey::KeyB,
        PbKey::KeyC,
        PbKey::KeyD,
        PbKey::KeyE,
        PbKey::KeyF,
        PbKey::KeyG,
        PbKey::KeyH,
        PbKey::KeyI,
        PbKey::KeyJ,
        PbKey::KeyK,
        PbKey::KeyL,
        PbKey::KeyM,
        PbKey::KeyN,
        PbKey::KeyO,
        PbKey::KeyP,
        PbKey::KeyQ,
        PbKey::KeyR,
        PbKey::KeyS,
        PbKey::KeyT,
        PbKey::KeyU,
        PbKey::KeyV,
        PbKey::KeyW,
        PbKey::KeyX,
        PbKey::KeyY,
        PbKey::KeyZ,
        PbKey::F1,
        PbKey::F2,
        PbKey::F3,
        PbKey::F4,
        PbKey::F5,
        PbKey::F6,
        PbKey::F7,
        PbKey::F8,
        PbKey::F9,
        PbKey::F10,
        PbKey::F11,
        PbKey::F12,
    ];
}
