//! Configurable keyboard bindings: a `KeyChord → Action` map with embedded
//! defaults (task #8). The defaults make the viewer work with no config file; an
//! optional `keymap.toml` in the config dir (and, later, the Settings keybinding
//! editor) layer over them. Chords parse from / print to human strings like
//! `"Ctrl+S"`, `"Shift+R"`, `"="`.
//!
//! Privacy (task #2): the keymap is read-only config input — loading it writes
//! nothing and it never records anything photo-derived.

use std::collections::HashMap;
use std::fmt;

use winit::keyboard::KeyCode;

use crate::action::Action;

/// A single key combination: a physical key plus the modifier flags that must be
/// held with it. Modifier order doesn't matter (equality is by the bool flags), so
/// `"Ctrl+Shift+R"` and `"Shift+Ctrl+R"` are the same chord.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct KeyChord {
    pub code: KeyCode,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl KeyChord {
    /// Build a chord from a physical key + the current modifier state (what the
    /// press handler does on each key-down).
    pub fn new(code: KeyCode, ctrl: bool, shift: bool, alt: bool) -> Self {
        Self {
            code,
            ctrl,
            shift,
            alt,
        }
    }

    /// Parse `"Ctrl+Shift+R"` / `"Alt+Enter"` / `"="` into a chord. Modifier tokens
    /// are case-insensitive (`ctrl`/`control`/`ctl`, `shift`, `alt`/`opt`/`option`);
    /// the final `+`-separated token is the key. `None` if a modifier token is
    /// unrecognized or the key name is unknown.
    pub fn parse(s: &str) -> Option<KeyChord> {
        let tokens: Vec<&str> = s.split('+').map(str::trim).collect();
        let (key_tok, mod_toks) = tokens.split_last()?;
        let (mut ctrl, mut shift, mut alt) = (false, false, false);
        for m in mod_toks {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "ctl" => ctrl = true,
                "shift" => shift = true,
                "alt" | "opt" | "option" => alt = true,
                _ => return None,
            }
        }
        let code = str_to_key(key_tok)?;
        Some(KeyChord {
            code,
            ctrl,
            shift,
            alt,
        })
    }
}

impl fmt::Display for KeyChord {
    /// Canonical `Ctrl+Alt+Shift+Key` order (modifiers alphabetical-ish by
    /// convention), e.g. `"Ctrl+S"`, `"Shift+R"`, `"="`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            write!(f, "Ctrl+")?;
        }
        if self.alt {
            write!(f, "Alt+")?;
        }
        if self.shift {
            write!(f, "Shift+")?;
        }
        write!(f, "{}", key_to_str(self.code))
    }
}

/// Is this a numeric-keypad key? Used to keep numpad aliases out of the help
/// overlay (they're bound, just not worth showing next to the primary key).
pub fn is_numpad(code: KeyCode) -> bool {
    key_to_str(code).starts_with("Numpad")
}

/// The configurable key→action table. Holds both directions: chord→action for the
/// input dispatch, and action→chords for the help overlay / editor.
pub struct Keymap {
    by_chord: HashMap<KeyChord, Action>,
    by_action: HashMap<Action, Vec<KeyChord>>,
}

impl Keymap {
    /// The built-in defaults (today's hardcoded bindings). Always valid.
    pub fn defaults() -> Keymap {
        let mut km = Keymap {
            by_chord: HashMap::new(),
            by_action: HashMap::new(),
        };
        for (action, chords) in default_bindings() {
            km.by_action.insert(action, chords);
        }
        km.rebuild_index();
        km
    }

    /// Load the keymap: defaults with the user's `keymap.toml` (if present) merged
    /// over them. Best-effort + privacy-safe (read-only); a missing or unreadable or
    /// malformed file just means "use defaults." Any validation issues are logged.
    pub fn load() -> Keymap {
        let mut km = Keymap::defaults();
        if let Some(text) = read_config() {
            for w in km.merge_toml(&text) {
                eprintln!("PhotoBlaze keymap: {w}");
            }
        }
        km
    }

    /// The action bound to `chord`, if any (the input-dispatch lookup).
    pub fn action_for(&self, chord: &KeyChord) -> Option<Action> {
        self.by_chord.get(chord).copied()
    }

    /// The chords currently bound to `action` (for the help overlay / editor).
    pub fn bindings_for(&self, action: Action) -> &[KeyChord] {
        self.by_action.get(&action).map_or(&[], Vec::as_slice)
    }

    /// Merge a `keymap.toml` over the current bindings. Schema: a `[keys]` table
    /// mapping action ids to a chord string or an array of chord strings, e.g.
    /// ```toml
    /// [keys]
    /// rotate_cw = "R"
    /// random    = ["Enter", "NumpadEnter"]
    /// ```
    /// A configured action *replaces* that action's bindings. Returns human warnings
    /// (unparseable file, unknown action, bad chord, duplicate key) — collected
    /// rather than printed so it's unit-testable.
    pub fn merge_toml(&mut self, text: &str) -> Vec<String> {
        let mut warnings = Vec::new();
        let table: toml::Table = match text.parse() {
            Ok(t) => t,
            Err(e) => {
                warnings.push(format!("ignoring config (parse error: {e})"));
                return warnings;
            }
        };
        let Some(keys) = table.get("keys").and_then(toml::Value::as_table) else {
            // No [keys] table — nothing to override (not an error).
            return warnings;
        };
        for (id, value) in keys {
            let Some(action) = Action::from_id(id) else {
                warnings.push(format!("unknown action {id:?} ignored"));
                continue;
            };
            let raw = chord_strings(value);
            if raw.is_empty() {
                warnings.push(format!("{id}: expected a key string or array of strings"));
                continue;
            }
            let mut chords = Vec::new();
            for s in raw {
                match KeyChord::parse(&s) {
                    Some(c) => chords.push(c),
                    None => warnings.push(format!("{id}: unrecognized key {s:?}")),
                }
            }
            self.by_action.insert(action, chords);
        }
        warnings.extend(self.rebuild_index());
        warnings
    }

    /// Rebuild the chord→action index from `by_action`, in stable [`Action::ALL`]
    /// order so a duplicate key resolves deterministically (first action in that
    /// order wins). Returns a warning per collision.
    fn rebuild_index(&mut self) -> Vec<String> {
        self.by_chord.clear();
        let mut warnings = Vec::new();
        for &action in Action::ALL {
            let Some(chords) = self.by_action.get(&action) else {
                continue;
            };
            for &chord in chords {
                if let Some(prev) = self.by_chord.get(&chord).copied() {
                    warnings.push(format!(
                        "key {chord} bound to both {} and {} (keeping {})",
                        prev.id(),
                        action.id(),
                        prev.id()
                    ));
                } else {
                    self.by_chord.insert(chord, action);
                }
            }
        }
        warnings
    }
}

/// Extract chord strings from a TOML value: a bare string, or an array of strings.
/// Anything else yields an empty list (the caller warns).
fn chord_strings(value: &toml::Value) -> Vec<String> {
    match value {
        toml::Value::String(s) => vec![s.clone()],
        toml::Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// The default bindings — today's hardcoded keymap, expressed as the config would.
/// Panics only on a typo in a literal here (caught by the `defaults_all_parse` test).
fn default_bindings() -> Vec<(Action, Vec<KeyChord>)> {
    let p = |s: &str| KeyChord::parse(s).expect("default chord must parse");
    let one = |a: Action, s: &str| (a, vec![p(s)]);
    vec![
        (Action::Next, vec![p("Space")]),
        (Action::Prev, vec![p("Backspace")]),
        (Action::Random, vec![p("Enter"), p("NumpadEnter")]),
        (
            Action::RandomPrev,
            vec![p("Shift+Enter"), p("Shift+NumpadEnter")],
        ),
        one(Action::PanLeft, "Left"),
        one(Action::PanRight, "Right"),
        one(Action::PanUp, "Up"),
        one(Action::PanDown, "Down"),
        (Action::ZoomIn, vec![p("="), p("NumpadAdd")]),
        (Action::ZoomOut, vec![p("-"), p("NumpadSubtract")]),
        one(Action::ScaleFit, "8"),
        one(Action::ScaleFill, "9"),
        one(Action::ToggleOriginal, "0"),
        // ScaleOriginal is menu-only (the keyboard's `0` toggles); no default key.
        (Action::ScaleOriginal, vec![]),
        one(Action::RotateCw, "R"),
        one(Action::RotateCcw, "Shift+R"),
        one(Action::Copy, "Ctrl+C"),
        one(Action::SaveRotation, "Ctrl+S"),
        one(Action::Delete, "Delete"),
        one(Action::DeletePermanent, "Shift+Delete"),
        one(Action::OpenFile, "O"),
        one(Action::OpenFolder, "Shift+O"),
        one(Action::Info, "I"),
        one(Action::FullExif, "Shift+I"),
        (Action::Help, vec![p("/"), p("Shift+/")]),
        (Action::Fullscreen, vec![p("F11"), p("Alt+Enter")]),
        one(Action::Recursive, "Ctrl+R"),
        one(Action::Settings, "Ctrl+,"),
        // About is menu-only (no default key).
        (Action::About, vec![]),
        one(Action::Quit, "Esc"),
    ]
}

/// Read the user's `keymap.toml` from the config dir, if it exists and is readable.
/// `None` (use defaults) on any failure — read-only, privacy-safe.
fn read_config() -> Option<String> {
    let path = crate::settings::config_dir()?.join("keymap.toml");
    std::fs::read_to_string(path).ok()
}

/// Human name for a physical key — config-friendly (`"Left"`, `"NumpadAdd"`), used
/// for parsing and for the help overlay. Unknown keys print as `"?"` (only reachable
/// for an unbound key, since every bound chord uses a named key).
fn key_to_str(code: KeyCode) -> &'static str {
    use KeyCode::*;
    match code {
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
        _ => "?",
    }
}

/// Parse a key name (the final token of a chord) into a physical key. Letters and
/// digits are accepted case-insensitively; named keys match [`key_to_str`] (plus a
/// few friendly aliases). `None` for an unknown name.
fn str_to_key(s: &str) -> Option<KeyCode> {
    use KeyCode::*;
    // Single letter or digit (case-insensitive for letters).
    if s.len() == 1 {
        let ch = s.chars().next().unwrap();
        if ch.is_ascii_alphabetic() {
            let idx = ch.to_ascii_uppercase() as u8 - b'A';
            const LETTERS: [KeyCode; 26] = [
                KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN,
                KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
            ];
            return Some(LETTERS[idx as usize]);
        }
        if ch.is_ascii_digit() {
            const DIGITS: [KeyCode; 10] = [
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_every_action() {
        let km = Keymap::defaults();
        // Every action appears in the table (some, like About, with no chord).
        for &a in Action::ALL {
            assert!(km.by_action.contains_key(&a), "default table missing {a:?}");
        }
    }

    #[test]
    fn defaults_have_no_duplicate_keys() {
        // `defaults()` calls `rebuild_index`; a clean default table must collide-free.
        let mut km = Keymap::defaults();
        assert!(
            km.rebuild_index().is_empty(),
            "default bindings have a duplicate key"
        );
    }

    #[test]
    fn chord_parse_and_display_round_trip() {
        for s in ["Ctrl+S", "Shift+R", "Ctrl+,", "Alt+Enter", "=", "F11", "/"] {
            let c = KeyChord::parse(s).unwrap_or_else(|| panic!("parse {s:?}"));
            assert_eq!(c.to_string(), s, "round-trip {s:?}");
        }
    }

    #[test]
    fn chord_parse_is_modifier_order_and_case_insensitive() {
        let a = KeyChord::parse("Ctrl+Shift+R").unwrap();
        let b = KeyChord::parse("shift+control+r").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.code, KeyCode::KeyR);
        assert!(a.ctrl && a.shift && !a.alt);
    }

    #[test]
    fn chord_parse_rejects_unknown() {
        assert_eq!(KeyChord::parse("Hyper+R"), None); // unknown modifier
        assert_eq!(KeyChord::parse("Ctrl+Nope"), None); // unknown key
    }

    #[test]
    fn default_lookups_match_expectations() {
        let km = Keymap::defaults();
        let chord = |s: &str| KeyChord::parse(s).unwrap();
        assert_eq!(km.action_for(&chord("Ctrl+C")), Some(Action::Copy));
        assert_eq!(km.action_for(&chord("Shift+R")), Some(Action::RotateCcw));
        assert_eq!(km.action_for(&chord("R")), Some(Action::RotateCw));
        assert_eq!(km.action_for(&chord("Ctrl+R")), Some(Action::Recursive));
        assert_eq!(km.action_for(&chord("Alt+Enter")), Some(Action::Fullscreen));
        assert_eq!(km.action_for(&chord("Space")), Some(Action::Next));
        assert_eq!(km.action_for(&chord("D")), None); // unbound
    }

    #[test]
    fn merge_rebinds_an_action() {
        let mut km = Keymap::defaults();
        let warnings = km.merge_toml("[keys]\nrotate_cw = \"F5\"\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        let chord = |s: &str| KeyChord::parse(s).unwrap();
        assert_eq!(km.action_for(&chord("F5")), Some(Action::RotateCw));
        // The old default key is no longer bound (the action's set was replaced).
        assert_eq!(km.action_for(&chord("R")), None);
    }

    #[test]
    fn merge_accepts_an_array_of_keys() {
        let mut km = Keymap::defaults();
        let warnings = km.merge_toml("[keys]\nnext = [\"Space\", \"N\"]\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        let chord = |s: &str| KeyChord::parse(s).unwrap();
        assert_eq!(km.action_for(&chord("Space")), Some(Action::Next));
        assert_eq!(km.action_for(&chord("N")), Some(Action::Next));
    }

    #[test]
    fn merge_warns_on_unknown_action_and_bad_key() {
        let mut km = Keymap::defaults();
        let warnings = km.merge_toml("[keys]\nteleport = \"T\"\ncopy = \"Ctrl+Nope\"\n");
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("teleport")));
        assert!(warnings.iter().any(|w| w.contains("Nope")));
    }

    #[test]
    fn merge_warns_on_a_duplicate_key() {
        let mut km = Keymap::defaults();
        // Bind Copy's key onto another action too → collision on rebuild.
        let warnings = km.merge_toml("[keys]\ninfo = \"Ctrl+C\"\n");
        assert!(
            warnings.iter().any(|w| w.contains("bound to both")),
            "expected a duplicate-key warning, got {warnings:?}"
        );
    }

    #[test]
    fn malformed_toml_is_ignored_with_a_warning() {
        let mut km = Keymap::defaults();
        let warnings = km.merge_toml("this is not = = toml");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("parse error"));
        // Defaults survive intact.
        let chord = KeyChord::parse("Ctrl+C").unwrap();
        assert_eq!(km.action_for(&chord), Some(Action::Copy));
    }

    #[test]
    fn numpad_detection() {
        assert!(is_numpad(KeyCode::NumpadAdd));
        assert!(is_numpad(KeyCode::NumpadEnter));
        assert!(!is_numpad(KeyCode::Enter));
        assert!(!is_numpad(KeyCode::Equal));
    }
}
