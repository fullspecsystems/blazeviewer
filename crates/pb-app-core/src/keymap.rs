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

use crate::action::Action;
use crate::pb_key::PbKey;

/// A single key combination: a physical key plus the modifier flags that must be
/// held with it. Modifier order doesn't matter (equality is by the bool flags), so
/// `"Ctrl+Shift+R"` and `"Shift+Ctrl+R"` are the same chord.
///
/// `logo` is the platform "super" key — **Cmd (⌘) on macOS**, the Windows key
/// elsewhere. It's tracked separately from `ctrl` so a Mac binding like `Cmd+S` is
/// a distinct chord from bare `S` (otherwise the OS-standard ⌘-shortcuts would fall
/// through to the bare-key actions — ⌘S → Slideshow, ⌘R → Rotate — when Cmd is held).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct KeyChord {
    pub code: PbKey,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub logo: bool,
}

impl KeyChord {
    /// Build a chord from a physical key + the current modifier state (what the
    /// press handler does on each key-down). `logo` = the Cmd/Win "super" key.
    pub fn new(code: PbKey, ctrl: bool, shift: bool, alt: bool, logo: bool) -> Self {
        Self {
            code,
            ctrl,
            shift,
            alt,
            logo,
        }
    }

    /// Parse `"Ctrl+Shift+R"` / `"Alt+Enter"` / `"Cmd+S"` / `"="` into a chord.
    /// Modifier tokens are case-insensitive (`ctrl`/`control`/`ctl`, `shift`,
    /// `alt`/`opt`/`option`, `cmd`/`command`/`super`/`win`/`meta`/`logo`); the final
    /// `+`-separated token is the key. `None` if a modifier token is unrecognized or
    /// the key name is unknown.
    pub fn parse(s: &str) -> Option<KeyChord> {
        let tokens: Vec<&str> = s.split('+').map(str::trim).collect();
        let (key_tok, mod_toks) = tokens.split_last()?;
        let (mut ctrl, mut shift, mut alt, mut logo) = (false, false, false, false);
        for m in mod_toks {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "ctl" => ctrl = true,
                "shift" => shift = true,
                "alt" | "opt" | "option" => alt = true,
                "cmd" | "command" | "super" | "win" | "windows" | "meta" | "logo" => logo = true,
                _ => return None,
            }
        }
        let code = PbKey::from_name(key_tok)?;
        Some(KeyChord {
            code,
            ctrl,
            shift,
            alt,
            logo,
        })
    }

    /// The shortcut hint shown in the UI: on macOS its compact symbol notation
    /// ([`mac_symbol`](Self::mac_symbol) — e.g. `⇧ O`); elsewhere the spelled-out
    /// [`Display`](fmt::Display) form (e.g. `Shift+O`), matching Windows habits.
    pub fn shortcut_label(&self) -> String {
        #[cfg(target_os = "macos")]
        {
            self.mac_symbol()
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.to_string()
        }
    }

    /// A compact, Apple-HIG-ordered **symbol** rendering — the `⌃⌥⇧⌘` modifiers (Control,
    /// Option, Shift, Command) in that order, then the key (arrows as glyphs) — i.e. macOS's
    /// own shortcut notation. A **thin space** separates the modifier cluster from the key
    /// (`⇧ O`, `⌘ O`) so they don't crowd, matching how the menu bar spaces them. The
    /// word-style [`Display`](fmt::Display) drives menus and text.
    #[cfg(target_os = "macos")]
    pub fn mac_symbol(&self) -> String {
        let mut mods = String::new();
        if self.ctrl {
            mods.push('\u{2303}'); // ⌃ Control
        }
        if self.alt {
            mods.push('\u{2325}'); // ⌥ Option
        }
        if self.shift {
            mods.push('\u{21e7}'); // ⇧ Shift
        }
        if self.logo {
            mods.push('\u{2318}'); // ⌘ Command
        }
        let key = key_symbol(self.code);
        if mods.is_empty() {
            key.to_string()
        } else {
            format!("{mods}\u{2009}{key}") // U+2009 THIN SPACE before the key
        }
    }
}

/// A key's macOS glyph where one is conventional (the arrow keys + erase-left), else its
/// spelled-out [`PbKey::as_str`] name — the key half of a shortcut hint. Only these are
/// substituted (they're safe in the UI font); other keys stay spelled out. Used by
/// [`KeyChord::mac_symbol`].
#[cfg(target_os = "macos")]
fn key_symbol(code: PbKey) -> &'static str {
    match code {
        PbKey::ArrowLeft => "\u{2190}",  // ←
        PbKey::ArrowRight => "\u{2192}", // →
        PbKey::ArrowUp => "\u{2191}",    // ↑
        PbKey::ArrowDown => "\u{2193}",  // ↓
        PbKey::Backspace => "\u{232b}",  // ⌫ (erase-left; the Mac "delete" key, ⌘⌫ trash chord)
        _ => code.as_str(),
    }
}

impl fmt::Display for KeyChord {
    /// Canonical `Ctrl+Cmd+Alt+Shift+Key` order (so `Ctrl+Cmd+F` reads as written),
    /// e.g. `"Ctrl+S"`, `"Cmd+S"`, `"Shift+R"`, `"="`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            write!(f, "Ctrl+")?;
        }
        if self.logo {
            write!(f, "Cmd+")?;
        }
        if self.alt {
            write!(f, "Alt+")?;
        }
        if self.shift {
            write!(f, "Shift+")?;
        }
        write!(f, "{}", self.code.as_str())
    }
}

/// The configurable key→action table. Holds both directions: chord→action for the
/// input dispatch, and action→chords for the help overlay / editor. `Clone` so the
/// Settings editor can edit a draft and commit it on Save (or discard on Cancel).
#[derive(Clone, Debug)]
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

    /// The chord in positional `slot` (0 = primary, 1 = secondary) of `action`, if
    /// any — what the two-slot editor displays.
    pub fn slot(&self, action: Action, slot: usize) -> Option<KeyChord> {
        self.bindings_for(action).get(slot).copied()
    }

    /// Assign `chord` to `action`'s positional `slot` (0 = primary, 1 = secondary),
    /// stealing it from any other action so every chord has exactly one owner (the
    /// standard rebind behavior). Returns the action it was taken from, if it had been
    /// bound elsewhere, so the editor can note "moved from …". Rebuilds the index.
    pub fn set_slot(&mut self, action: Action, slot: usize, chord: KeyChord) -> Option<Action> {
        let stolen_from = self.action_for(&chord).filter(|&a| a != action);
        // Remove this chord wherever it currently lives (incl. the target), so it can't
        // end up bound to two actions.
        for chords in self.by_action.values_mut() {
            chords.retain(|c| *c != chord);
        }
        // Place it in the requested slot of the target's (now ≤2) list.
        let cur = self.by_action.get(&action).cloned().unwrap_or_default();
        let mut slots: [Option<KeyChord>; 2] = [cur.first().copied(), cur.get(1).copied()];
        if slot < slots.len() {
            slots[slot] = Some(chord);
        }
        self.by_action
            .insert(action, slots.into_iter().flatten().collect());
        self.rebuild_index();
        stolen_from
    }

    /// Clear the chord in positional `slot` of `action` (removing the binding; a
    /// cleared primary promotes the secondary). Rebuilds the index.
    pub fn clear_slot(&mut self, action: Action, slot: usize) {
        if let Some(chords) = self.by_action.get_mut(&action) {
            if slot < chords.len() {
                chords.remove(slot);
            }
        }
        self.rebuild_index();
    }

    /// Restore every binding to the built-in defaults (the editor's "reset").
    pub fn reset_to_defaults(&mut self) {
        *self = Keymap::defaults();
    }

    /// Serialize to the `keymap.toml` schema (`[keys]` = id → array of chord strings),
    /// writing **every** action — including ones the user cleared (as `[]`) — so a
    /// reload reproduces the exact map (a cleared default stays cleared). Stable
    /// [`Action::ALL`] order for a clean, reviewable diff.
    pub fn to_toml(&self) -> String {
        let mut keys = toml::map::Map::new();
        for &action in Action::ALL {
            let arr: Vec<toml::Value> = self
                .bindings_for(action)
                .iter()
                .map(|c| toml::Value::String(c.to_string()))
                .collect();
            keys.insert(action.id().to_string(), toml::Value::Array(arr));
        }
        let mut root = toml::map::Map::new();
        root.insert("keys".to_string(), toml::Value::Table(keys));
        let body = toml::to_string_pretty(&toml::Value::Table(root)).unwrap_or_default();
        format!("# PhotoBlaze keymap (preferences only, never photo data)\n{body}")
    }

    /// Persist to `keymap.toml`, atomically (temp + rename). Best-effort; an explicit
    /// user action only (Settings ▸ Save) — privacy #2 (config, never photo data).
    pub fn save(&self) -> bool {
        let Some(dir) = crate::config_dir() else {
            return false;
        };
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("keymap.toml");
        let tmp = path.with_extension("toml.tmp");
        if std::fs::write(&tmp, self.to_toml()).is_err() {
            return false;
        }
        std::fs::rename(&tmp, &path).is_ok()
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
            let Some(raw) = chord_strings(value) else {
                warnings.push(format!("{id}: expected a key string or array of strings"));
                continue;
            };
            // An explicit empty array clears the action (it overrides the default with
            // "no binding") — that's how a cleared binding round-trips through `to_toml`.
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

/// Extract chord strings from a TOML value: a bare string → one, or an array of
/// strings → that list (possibly empty = "clear this action"). `None` for any other
/// type (the caller warns); an empty array is `Some(vec![])`, distinct from `None`.
fn chord_strings(value: &toml::Value) -> Option<Vec<String>> {
    match value {
        toml::Value::String(s) => Some(vec![s.clone()]),
        toml::Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        ),
        _ => None,
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
        // Copy the current file's path to the clipboard. Shift+Ctrl+C on Windows; on
        // macOS the menu's ⇧⌘C accelerator drives it (this real-Control chord also works).
        one(Action::CopyPath, "Shift+Ctrl+C"),
        one(Action::SaveRotation, "Ctrl+S"),
        one(Action::Delete, "Delete"),
        one(Action::DeletePermanent, "Shift+Delete"),
        // Undo the last reversible edit (currently a saved rotation). Ctrl+Z on Windows;
        // on macOS the menu's ⌘Z accelerator drives it (this real-Control chord also works).
        one(Action::Undo, "Ctrl+Z"),
        one(Action::OpenFile, "O"),
        one(Action::OpenFolder, "Shift+O"),
        one(Action::Info, "I"),
        one(Action::FullExif, "Shift+I"),
        (Action::Help, vec![p("/"), p("Shift+/")]),
        // Fullscreen is a core action, so it's worth a few bindings: F11 (Windows
        // convention), Alt+Enter / Option+Enter (the discoverable-by-habit one), and
        // bare `F` (the most memorable — and the only one that's discoverable at all).
        (Action::Fullscreen, vec![p("F11"), p("Alt+Enter"), p("F")]),
        one(Action::Recursive, "Ctrl+R"),
        // Stop an in-flight folder scan — menu-only by default (Esc stays Quit); a user can
        // bind a key in Settings if they want one.
        (Action::CancelScan, vec![]),
        one(Action::SlideshowToggle, "S"),
        // `[` shortens the interval (faster), `]` lengthens it (slower).
        one(Action::SlideshowFaster, "["),
        one(Action::SlideshowSlower, "]"),
        // Animation playback (on-demand; `P` toggles play/pause). `.`/`,` step a
        // frame forward/back (hold to scrub); they pause playback first.
        one(Action::PlayPause, "P"),
        one(Action::FrameNext, "."),
        one(Action::FramePrev, ","),
        one(Action::MuteLiveAudio, "M"),
        one(Action::Settings, "Ctrl+,"),
        // About is menu-only (no default key).
        (Action::About, vec![]),
        one(Action::Quit, "Esc"),
    ]
}

/// Read the user's `keymap.toml` from the config dir, if it exists and is readable.
/// `None` (use defaults) on any failure — read-only, privacy-safe.
fn read_config() -> Option<String> {
    let path = crate::config_dir()?.join("keymap.toml");
    std::fs::read_to_string(path).ok()
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
        for s in [
            "Ctrl+S",
            "Shift+R",
            "Ctrl+,",
            "Alt+Enter",
            "=",
            "F11",
            "/",
            "Cmd+S",
            "Ctrl+Cmd+F",
        ] {
            let c = KeyChord::parse(s).unwrap_or_else(|| panic!("parse {s:?}"));
            assert_eq!(c.to_string(), s, "round-trip {s:?}");
        }
    }

    #[test]
    fn cmd_aliases_parse_to_logo() {
        for s in ["Cmd+S", "Command+S", "Super+S", "Win+S", "Meta+S", "Logo+S"] {
            let c = KeyChord::parse(s).unwrap_or_else(|| panic!("parse {s:?}"));
            assert!(c.logo && !c.ctrl, "{s:?} should set logo, not ctrl");
            assert_eq!(c.code, PbKey::KeyS);
        }
    }

    #[test]
    fn cmd_is_distinct_from_bare_key_so_it_cannot_misfire() {
        // The whole point of the `logo` field: on macOS, holding Cmd must NOT fall
        // through to the bare-key action. `S` is Slideshow, but `Cmd+S` is a separate
        // chord (Save lives on the menu's ⌘S there) and resolves to nothing here.
        let km = Keymap::defaults();
        let chord = |s: &str| KeyChord::parse(s).unwrap();
        assert_ne!(chord("S"), chord("Cmd+S"));
        assert_eq!(km.action_for(&chord("S")), Some(Action::SlideshowToggle));
        assert_eq!(km.action_for(&chord("Cmd+S")), None);
        // Likewise ⌘R / ⌘O don't trigger the bare Rotate / Open actions.
        assert_eq!(km.action_for(&chord("Cmd+R")), None);
        assert_eq!(km.action_for(&chord("Cmd+O")), None);
    }

    #[test]
    fn bare_f_toggles_fullscreen() {
        // Discoverable fullscreen: F joins F11 and Alt+Enter (see `default_bindings`).
        let km = Keymap::defaults();
        let chord = |s: &str| KeyChord::parse(s).unwrap();
        assert_eq!(km.action_for(&chord("F")), Some(Action::Fullscreen));
        assert_eq!(km.action_for(&chord("F11")), Some(Action::Fullscreen));
        assert_eq!(km.action_for(&chord("Alt+Enter")), Some(Action::Fullscreen));
    }

    #[test]
    fn chord_parse_is_modifier_order_and_case_insensitive() {
        let a = KeyChord::parse("Ctrl+Shift+R").unwrap();
        let b = KeyChord::parse("shift+control+r").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.code, PbKey::KeyR);
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
    fn set_slot_steals_from_a_previous_owner() {
        let mut km = Keymap::defaults();
        let chord = |s: &str| KeyChord::parse(s).unwrap();
        // Assign Copy's key (Ctrl+C) to Info's primary slot → stolen from Copy.
        let stolen = km.set_slot(Action::Info, 0, chord("Ctrl+C"));
        assert_eq!(stolen, Some(Action::Copy));
        assert_eq!(km.action_for(&chord("Ctrl+C")), Some(Action::Info));
        // Copy no longer owns it (single-owner invariant), and no duplicate warning.
        assert!(!km.bindings_for(Action::Copy).contains(&chord("Ctrl+C")));
        assert!(km.clone().rebuild_index().is_empty());
    }

    #[test]
    fn set_slot_sets_primary_and_secondary_independently() {
        let mut km = Keymap::defaults();
        let chord = |s: &str| KeyChord::parse(s).unwrap();
        assert_eq!(km.set_slot(Action::RotateCw, 0, chord("F6")), None);
        assert_eq!(km.set_slot(Action::RotateCw, 1, chord("F7")), None);
        assert_eq!(km.slot(Action::RotateCw, 0), Some(chord("F6")));
        assert_eq!(km.slot(Action::RotateCw, 1), Some(chord("F7")));
        assert_eq!(km.action_for(&chord("F6")), Some(Action::RotateCw));
        assert_eq!(km.action_for(&chord("F7")), Some(Action::RotateCw));
    }

    #[test]
    fn clear_slot_removes_a_binding_and_promotes() {
        let mut km = Keymap::defaults();
        let chord = |s: &str| KeyChord::parse(s).unwrap();
        // Random defaults to [Enter, NumpadEnter]; clearing the primary promotes it.
        km.clear_slot(Action::Random, 0);
        assert_eq!(km.slot(Action::Random, 0), Some(chord("NumpadEnter")));
        assert_eq!(km.action_for(&chord("Enter")), None);
    }

    #[test]
    fn to_toml_round_trips_including_a_cleared_action() {
        let mut km = Keymap::defaults();
        let chord = |s: &str| KeyChord::parse(s).unwrap();
        km.set_slot(Action::RotateCw, 0, chord("F6"));
        km.clear_slot(Action::Help, 0); // drop "/"
        km.clear_slot(Action::Help, 0); // drop "Shift+/" → Help now unbound
                                        // Serialize, then load fresh defaults and merge the serialized text over them.
        let text = km.to_toml();
        let mut reloaded = Keymap::defaults();
        let warnings = reloaded.merge_toml(&text);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(reloaded.action_for(&chord("F6")), Some(Action::RotateCw));
        // The cleared Help binding stays cleared (explicit `[]` in the file).
        assert_eq!(reloaded.action_for(&chord("/")), None);
        assert!(reloaded.bindings_for(Action::Help).is_empty());
    }

    #[test]
    fn reset_to_defaults_restores_bindings() {
        let mut km = Keymap::defaults();
        let chord = |s: &str| KeyChord::parse(s).unwrap();
        km.set_slot(Action::RotateCw, 0, chord("F6"));
        km.reset_to_defaults();
        assert_eq!(km.action_for(&chord("R")), Some(Action::RotateCw));
        assert_eq!(km.action_for(&chord("F6")), None);
    }

    #[test]
    fn every_default_chord_round_trips_through_its_string() {
        // The defaults are the contract the help overlay and editor render. Each
        // default chord must survive Display → parse, so what the user sees is what
        // reloads from config. (This is the guard that would catch binding a default
        // to a display-only key like the numpad digits.)
        let km = Keymap::defaults();
        for &action in Action::ALL {
            for chord in km.bindings_for(action) {
                let printed = chord.to_string();
                assert_eq!(
                    KeyChord::parse(&printed),
                    Some(*chord),
                    "{action:?} default chord {printed:?} does not round-trip",
                );
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_symbol_uses_apple_modifier_glyphs_in_hig_order() {
        let p = |s: &str| KeyChord::parse(s).unwrap();
        // A bare key is just the key (no modifier → no thin space); the open-screen hints are
        // "O" and "⇧ O".
        assert_eq!(p("O").mac_symbol(), "O");
        assert_eq!(p("Shift+O").mac_symbol(), "\u{21e7}\u{2009}O");
        assert_eq!(p("Cmd+S").mac_symbol(), "\u{2318}\u{2009}S");
        // Modifiers always print ⌃⌥⇧⌘ (tight), then a thin space, then the key — regardless of
        // how the chord was written.
        assert_eq!(
            p("Shift+Ctrl+Alt+Cmd+F").mac_symbol(),
            "\u{2303}\u{2325}\u{21e7}\u{2318}\u{2009}F"
        );
        // Arrow keys render as glyphs; shortcut_label defers to mac_symbol on macOS.
        assert_eq!(p("Left").mac_symbol(), "\u{2190}");
        assert_eq!(p("Shift+O").shortcut_label(), "\u{21e7}\u{2009}O");
    }

    #[test]
    fn full_default_map_survives_a_toml_round_trip() {
        // Serialize the entire default keymap and reload it: every action's bindings
        // must come back identical (stronger than the single-cleared-action case).
        let km = Keymap::defaults();
        let mut reloaded = Keymap::defaults();
        let warnings = reloaded.merge_toml(&km.to_toml());
        assert!(warnings.is_empty(), "{warnings:?}");
        for &action in Action::ALL {
            assert_eq!(
                reloaded.bindings_for(action),
                km.bindings_for(action),
                "{action:?} bindings drifted across the TOML round-trip",
            );
        }
    }
}
