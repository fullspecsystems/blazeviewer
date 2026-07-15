//! Overlay / toast / hint **state** types — the "what to show" of the HUD, owned by
//! [`AppCore`](crate::AppCore). The Hud *rasterizer* (the "how to draw" — CPU text/pill
//! compositing) stays shell-side (`pb-app`'s `hud.rs`) with the renderer for now.

use crate::folder_tree::TreeTarget;
use pb_hud::hud::{TreeHit, TreeRow};
use std::time::{Duration, Instant};

/// The folder-tree overlay's interactive state while shown: the bitmap size +
/// margin (its on-screen rect is derived from the live window at hit-test time —
/// resize/DPI-proof), the hit rects **within the bitmap**,
/// each row's click target, the cached display rows (so hover/page re-renders
/// skip the derivation and its `read_dir`s entirely), the hovered hit, and the
/// windowing page offset (the clickable "… n more" markers). RAM-only.
pub struct TreePanel {
    pub w: u32,
    pub h: u32,
    pub margin: u32,
    pub hits: Vec<(TreeHit, [u32; 4])>,
    pub targets: Vec<Option<TreeTarget>>,
    pub rows: Vec<TreeRow>,
    pub hovered: Option<TreeHit>,
    pub page: i32,
    /// When this bitmap was rasterized — rate-limits mid-flight rebuilds.
    pub built: Instant,
}

/// Which tab of the **Inspector** — the single tabbed rich-content panel (task #54,
/// ADR-023): Details (the full EXIF table), Text (recognized text + QR payloads,
/// task #45), Describe (the AI description/answer, task #44). One panel, three
/// facets of "tell me about this image" — tabs make the old single-slot replacement
/// behavior legible instead of silent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InspectorTab {
    Details,
    Text,
    Describe,
}

/// Which tab of the **left pane** (task #83): the folder tree (`Shift+F`) or the
/// Thumbnails strip (`Shift+T`). The pane's open flag stays
/// `AppCore::folder_tree_open` (one pane, two tabs — the Inspector idiom);
/// this picks what it shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LeftTab {
    #[default]
    Folders,
    Thumbnails,
}

/// What the rich layer contributes to the shared HUD overlay slot: Help, or the
/// Inspector's active tab. (The basic `i` info line is the ephemeral layer's —
/// see [`SlotContent`].)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelContent {
    Help,
    Tab(InspectorTab),
}

/// What the single **rich-panel** overlay slot shows, priority-resolved by
/// `AppCore::slot_content` (help > inspector tab). The basic `i` info line is NOT
/// here — it is a separate permanent layer (task #54), so it can coexist with a
/// rich panel rather than replace it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotContent {
    Details,
    Text,
    Describe,
    Help,
}

/// Rich-panel open/visibility state (task #54, owner decisions 2026-07-05). Pure
/// state + semantics, unit-tested without an `AppCore`.
///
/// - The **Inspector** (tabbed content panel) and **Help** are mutually exclusive
///   *on screen* for now — they share the single HUD overlay slot until the
///   egui/native presenters land — so opening one closes the other.
/// - `hidden` is the Photoshop-style `Tab` master switch: **hidden ≠ closed** (the
///   open set survives), any panel toggle while hidden **reveals first** and only
///   ever *shows* (otherwise the keypress would appear to do nothing), and `Tab`
///   with nothing open is a no-op.
/// - The basic `i` info line is deliberately NOT here: it is the ephemeral HUD
///   layer (`AppCore::info_line`), and opening/closing it never touches a rich
///   panel (or vice versa). It shares only `hidden` — `Tab` is a "declutter to see
///   the clean photo" master switch, so it suppresses the line too — threaded into
///   `any_open`/`toggle_hidden` as an external bool the same way `tree_open` is.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Panels {
    /// The Inspector: `None` = closed, `Some(tab)` = open on that tab. Survives
    /// `hidden` (that's the point of hidden ≠ closed).
    pub inspector: Option<InspectorTab>,
    /// The keyboard-help panel (`/` / `?`).
    pub help: bool,
    /// The `Tab` master visibility flag: panels stay open but draw nothing.
    pub hidden: bool,
}

impl Panels {
    /// `Shift+I` / `T` / `D`: open the Inspector at `tab`, switch to it if the
    /// Inspector is open on another tab, close the Inspector if already on it.
    /// While hidden: reveal + ensure open — never close (the reveal rule).
    pub fn toggle_inspector(&mut self, tab: InspectorTab) {
        if self.hidden {
            self.hidden = false;
            self.inspector = Some(tab);
        } else if self.inspector == Some(tab) {
            self.inspector = None;
        } else {
            self.inspector = Some(tab);
        }
        self.help = false; // shared HUD slot: the Inspector replaces Help
    }

    /// Open the Inspector at `tab` unconditionally (the describe/ask flows: showing
    /// a result must never *close* the panel). Reveals if hidden.
    pub fn open_inspector(&mut self, tab: InspectorTab) {
        self.hidden = false;
        self.inspector = Some(tab);
        self.help = false;
    }

    /// `/` / `?`: toggle Help. While hidden: reveal + ensure open, never close.
    pub fn toggle_help(&mut self) {
        if self.hidden {
            self.hidden = false;
            self.help = true;
        } else {
            self.help = !self.help;
        }
        if self.help {
            self.inspector = None; // shared HUD slot: Help replaces the Inspector
        }
    }

    /// `Tab`: flip master visibility. Returns `false` (no-op) when nothing is open —
    /// flipping invisible state would only set a trap for the next panel toggle.
    /// `tree_open` counts: the folder tree is a rich panel too. `info_line` counts
    /// too: the basic `i` line is owned outside `Panels` (`AppCore::info_line`), so
    /// its on/off state is threaded in the same way `tree_open` is.
    pub fn toggle_hidden(&mut self, tree_open: bool, info_line: bool) -> bool {
        if !self.any_open(tree_open, info_line) {
            return false;
        }
        self.hidden = !self.hidden;
        true
    }

    /// Reveal (clear `hidden`), returning whether anything changed. The folder-tree
    /// toggle and every panel-opening path call this first.
    pub fn reveal(&mut self) -> bool {
        std::mem::take(&mut self.hidden)
    }

    /// Whether any rich panel — or the basic info line — is open (regardless of
    /// `hidden`).
    pub fn any_open(&self, tree_open: bool, info_line: bool) -> bool {
        self.inspector.is_some() || self.help || tree_open || info_line
    }

    /// What the rich layer shows in the shared HUD slot right now (`None` when
    /// everything is closed or hidden — the basic line may still claim the slot).
    pub fn content(&self) -> Option<PanelContent> {
        if self.hidden {
            return None;
        }
        if self.help {
            return Some(PanelContent::Help);
        }
        self.inspector.map(PanelContent::Tab)
    }

    /// Whether the folder tree (open-ness tracked by the caller) should draw.
    pub fn tree_visible(&self, tree_open: bool) -> bool {
        tree_open && !self.hidden
    }
}

/// The **semantic** icon for a status toast — the HUD maps it to a Font Awesome glyph, the
/// native macOS shell maps it to an SF Symbol, so a toast stays shell-neutral (one call site,
/// each shell picks its own art). The discriminant is the stable FFI wire value.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ToastIcon {
    #[default]
    None = 0,
    Mute = 1,
    Unmute = 2,
    Save = 3,
    Undo = 4,
    Delete = 5,
    Recycle = 6,
    Pin = 7,
    Unpin = 8,
    RotateLeft = 9,
    RotateRight = 10,
    Copy = 11,
    Captions = 12,
    CaptionsOff = 13,
    /// An audio track switched (task #99) — a speaker with waves, distinct from
    /// [`Unmute`](Self::Unmute): "you are now hearing a different track", not "sound is
    /// back on".
    AudioTrack = 14,
    /// A switch that did **not** happen — the previous track is still playing.
    AudioTrackFailed = 15,
}

impl ToastIcon {
    /// The stable FFI wire value (the native host maps it to an SF Symbol).
    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

/// A native toast's data (message + [`ToastIcon`] + start), held for the shell to render when
/// the CPU rasterizer is suppressed (`native_toast`). `seq` increments per toast so the shell
/// re-triggers its entrance animation even when the same message fires twice in a row.
pub struct NativeToast {
    pub message: String,
    pub icon: ToastIcon,
    pub started: Instant,
    pub seq: u64,
}

/// A transient bottom-center status toast (e.g. "Recursive folders: on"): a pill
/// rasterized once, held briefly at full opacity, then faded out by re-uploading the
/// bitmap with scaled alpha. Command feedback with no other on-screen cue — deliberately
/// NOT shown for next/prev/zoom.
pub struct Toast {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub started: Instant,
    /// Alpha last pushed to the renderer, so the fade re-uploads only on change.
    pub uploaded_alpha: f32,
}

impl Toast {
    /// Full-opacity hold, then a short linear fade (~1.3 s total).
    pub const HOLD: Duration = Duration::from_millis(950);
    pub const FADE: Duration = Duration::from_millis(380);

    /// The toast's alpha at `now`, or `None` once it has fully expired.
    pub fn alpha(&self, now: Instant) -> Option<f32> {
        let e = now.saturating_duration_since(self.started);
        if e <= Self::HOLD {
            Some(1.0)
        } else {
            let f = (e - Self::HOLD).as_secs_f32() / Self::FADE.as_secs_f32();
            (f < 1.0).then_some(1.0 - f)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use InspectorTab::*;

    #[test]
    fn inspector_toggle_opens_switches_and_closes() {
        let mut p = Panels::default();
        p.toggle_inspector(Details);
        assert_eq!(p.inspector, Some(Details), "opens on the pressed tab");
        p.toggle_inspector(Text);
        assert_eq!(p.inspector, Some(Text), "switches tabs while open");
        p.toggle_inspector(Text);
        assert_eq!(p.inspector, None, "same tab again closes");
    }

    #[test]
    fn inspector_and_help_share_the_slot() {
        let mut p = Panels::default();
        p.toggle_help();
        assert!(p.help);
        p.toggle_inspector(Describe);
        assert!(!p.help, "opening the Inspector replaces Help");
        assert_eq!(p.inspector, Some(Describe));
        p.toggle_help();
        assert!(p.help, "opening Help replaces the Inspector");
        assert_eq!(p.inspector, None);
    }

    #[test]
    fn tab_hides_without_closing_and_is_a_noop_when_nothing_open() {
        let mut p = Panels::default();
        assert!(!p.toggle_hidden(false, false), "nothing open → no-op");
        assert!(!p.hidden);
        p.toggle_inspector(Details);
        assert!(p.toggle_hidden(false, false));
        assert!(p.hidden, "Tab hides…");
        assert_eq!(p.inspector, Some(Details), "…but the panel stays open");
        assert_eq!(p.content(), None, "hidden panels draw nothing");
        assert!(p.toggle_hidden(false, false));
        assert!(!p.hidden, "Tab again reveals");
        assert_eq!(p.content(), Some(PanelContent::Tab(Details)));
    }

    #[test]
    fn tab_counts_the_folder_tree_as_open() {
        let mut p = Panels::default();
        assert!(p.toggle_hidden(true, false), "tree open → Tab works");
        assert!(p.hidden);
        assert!(!p.tree_visible(true), "hidden tree draws nothing");
        assert!(p.toggle_hidden(true, false));
        assert!(p.tree_visible(true));
    }

    #[test]
    fn tab_counts_and_hides_the_info_line() {
        let mut p = Panels::default();
        assert!(
            p.toggle_hidden(false, true),
            "info line alone → Tab is not a no-op"
        );
        assert!(p.hidden, "Tab hides the line along with the rich panels");
        assert!(p.toggle_hidden(false, true), "Tab again reveals");
        assert!(!p.hidden);
    }

    #[test]
    fn panel_toggles_while_hidden_reveal_and_never_close() {
        let mut p = Panels::default();
        p.toggle_inspector(Text);
        p.toggle_hidden(false, false);
        assert!(p.hidden);
        // Same tab pressed while hidden: reveal + keep open (NOT close).
        p.toggle_inspector(Text);
        assert!(!p.hidden, "toggle while hidden reveals first");
        assert_eq!(p.inspector, Some(Text), "…and only ever shows, never hides");
        // Help while hidden likewise reveals and opens (even if already open).
        p.toggle_help();
        p.toggle_hidden(false, false);
        p.toggle_help();
        assert!(!p.hidden);
        assert!(p.help, "help toggle while hidden reveals, not closes");
    }

    #[test]
    fn open_inspector_never_closes() {
        let mut p = Panels::default();
        p.open_inspector(Describe);
        p.open_inspector(Describe);
        assert_eq!(
            p.inspector,
            Some(Describe),
            "open is idempotent, not a toggle"
        );
        p.toggle_hidden(false, false);
        p.open_inspector(Describe);
        assert!(!p.hidden, "open reveals");
    }

    #[test]
    fn content_priority_is_help_over_inspector() {
        let mut p = Panels {
            inspector: Some(Details),
            help: true,    // not reachable via the toggles (they're exclusive), but the
            hidden: false, // priority must still be deterministic if state ever allows it
        };
        assert_eq!(p.content(), Some(PanelContent::Help));
        p.help = false;
        assert_eq!(p.content(), Some(PanelContent::Tab(Details)));
    }
}
