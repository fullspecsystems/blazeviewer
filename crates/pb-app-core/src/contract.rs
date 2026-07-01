//! The `AppCore` ⇄ shell contract — **a type sketch, not yet wired** (NS0, ADR-021).
//!
//! ADR-021 inverts ownership on the macOS target: an AppKit/SwiftUI host owns the
//! window + run loop and drives the Rust engine, where today winit *is* the engine.
//! For anything but winit to drive PhotoBlaze, the orchestration layer has to speak
//! a **shell-neutral vocabulary**: intent-level events *in* ([`CoreEvent`]), effects
//! the shell must carry out *out* ([`CoreEffect`]), with the existing [`Action`] kept
//! as the single command vocabulary (the plan's explicit rule).
//!
//! This module defines that vocabulary so it can be reviewed and tested **before**
//! any code moves behind it. Nothing here is consumed yet: `main.rs` still owns the
//! winit loop directly. Landing the types first (per the plan: "Define the `AppCore`
//! contract before moving code") lets the eventual extraction be a sequence of small,
//! mechanical, behavior-preserving moves rather than a big-bang rewrite.
//!
//! **Scope note.** Several payloads the final contract needs reference types that
//! still live in the winit shell or other crates (a `Settings` form, `LaunchInput`
//! from `pb-core::open`, the dialog request/result structs, the GPU surface handle).
//! Those variants are intentionally **omitted or stubbed with a `NS-later:` note**
//! rather than dragging their types into this crate prematurely — the sketch covers
//! the part of the contract that is already expressible with shell-neutral types
//! ([`Action`], [`PbKey`], [`Keymap`]) plus `std`. It will firm up as NS1/NS2 wire it.

use std::path::PathBuf;
use std::time::Instant;

use crate::{Action, ActionKind, Keymap, PbKey};

/// The keyboard modifier flags carried with a key event — the shell-neutral mirror
/// of the four bools the winit handler already tracks. `logo` is the platform
/// "super" key: **Cmd (⌘) on macOS**, the Windows key elsewhere (matching
/// [`KeyChord`](crate::KeyChord)).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub logo: bool,
}

impl Modifiers {
    /// No modifiers held.
    pub const NONE: Modifiers = Modifiers {
        ctrl: false,
        shift: false,
        alt: false,
        logo: false,
    };

    /// Resolve a physical key + these modifiers to a [`KeyChord`](crate::KeyChord),
    /// the lookup key the [`Keymap`] is indexed by. The bridge a shell uses to turn
    /// "this key went down with these modifiers" into a keymap query, identically on
    /// every platform.
    pub fn chord_with(self, key: PbKey) -> crate::KeyChord {
        crate::KeyChord::new(key, self.ctrl, self.shift, self.alt, self.logo)
    }
}

/// How the image is scaled to the window — the one-of-three View-menu group
/// (`Fit` / `Crop to Fill` / `Original 1:1`), exactly one active at a time.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ScaleMode {
    #[default]
    Fit,
    Fill,
    Original,
}

/// The info overlay state — the two mutually-exclusive View-menu toggles
/// (`Show Image Info` / `Show All EXIF Info`); at most one is on.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum InfoOverlay {
    #[default]
    Hidden,
    Basic,
    FullExif,
}

/// The window presentation mode the shell should apply. `Fullscreen` is the
/// borderless chrome-free "speed mode" (distinct from macOS's native Spaces
/// fullscreen, which the shell tracks separately).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum WindowMode {
    #[default]
    Windowed,
    Fullscreen,
}

/// The pointer cursor the shell should show. The viewer hides the cursor in the
/// chrome-free hot path and restores it on movement.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CursorKind {
    #[default]
    Default,
    Hidden,
    /// Panning is available (an open hand) — shown when the image is zoomed past the
    /// window so a drag would pan it.
    Grab,
    /// Actively dragging to pan (a closed hand).
    Grabbing,
    /// Over a clickable affordance (e.g. the Cancel-scan chip).
    Pointer,
}

/// Which chrome dialog the shell should present — the shell-neutral mirror of the
/// existing `dialog::DialogKind`. (The *payload* each needs — settings form, error
/// text, progress handles — is `NS-later`: those types still live in the shell.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DialogKind {
    About,
    Settings,
    /// Destructive-action confirm (e.g. delete).
    Confirm,
    /// An error / informational message.
    Message,
    /// Password entry (e.g. an encrypted archive).
    Password,
    /// Indeterminate "opening…" progress.
    Loading,
    /// Determinate folder-scan progress.
    Scanning,
}

/// The runtime, app-driven state of the native menu — everything `main.rs` mirrors
/// onto the live `muda` items today (`App::refresh_*` + `ViewChecks`). Emitted as a
/// single [`CoreEffect::SetMenuState`] so a shell (muda now, AppKit later) can keep
/// its check/enabled marks in sync without the core knowing the menu's mechanism.
///
/// Models *semantics*, not menu widgets: the one-of-three scale group is a
/// [`ScaleMode`], the two info toggles an [`InfoOverlay`] — the shell maps those onto
/// its individual items.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MenuState {
    /// View ▸ scale group (exactly one checked).
    pub scale: ScaleMode,
    /// View ▸ info overlays (at most one checked).
    pub info: InfoOverlay,
    /// View ▸ Recursive (This Folder).
    pub recursive: bool,
    /// View ▸ Fullscreen (the borderless speed mode checkbox).
    pub fullscreen: bool,
    /// View ▸ Slideshow.
    pub slideshow: bool,
    /// Image ▸ Mute Live Photo Audio — checked when Live Photo audio is muted (#38).
    pub mute_live_audio: bool,
    /// File ▸ Save Rotation — enabled only with an unsaved rotation on a writable file.
    pub save_rotation_enabled: bool,
    /// File ▸ Stop Scanning — enabled only while a folder scan is streaming in.
    pub cancel_scan_enabled: bool,
    /// Edit ▸ Undo — the dynamic title/enabled state mirroring the top of the undo
    /// stack: `None` = nothing to undo (disabled, shown as the bare "Undo"); `Some(label)`
    /// = enabled, the item titled with `label` (e.g. "Undo Save Rotation"). The shell
    /// supplies the static label strings, so the core stays unaware of the edit kinds.
    pub undo: Option<&'static str>,
    /// macOS only: whether native (Spaces) full-screen is engaged, so the shell can
    /// flip the item's "Enter/Exit Full Screen" label. Always `false` on Windows.
    pub native_fullscreen_engaged: bool,
}

/// What to write to the system clipboard — the shell-neutral payload of
/// [`CoreEffect::WriteClipboard`]. The core builds this (decode + rotate is pure data
/// prep); the shell performs the actual platform write (Win32 `CF_DIBV5`/`CF_HDROP`,
/// `NSPasteboard`, or `arboard` text) so the core stays free of clipboard APIs.
#[derive(Clone, Debug)]
pub enum ClipboardPayload {
    /// A decoded RGBA8 image (`w*h*4`), plus the source file to also offer as a
    /// file-drop when one exists on disk (`None` for an archive entry → image-only).
    Image {
        rgba: Vec<u8>,
        w: u32,
        h: u32,
        file: Option<PathBuf>,
    },
    /// Plain text — a file path or (for an archive entry) its name.
    Text(String),
}

/// An intent-level event delivered *to* the core by the shell. The winit shell would
/// translate `WindowEvent`s into these; the AppKit shell would translate `NSEvent`s /
/// gesture recognizers — the core handles both identically.
#[derive(Clone, Debug)]
pub enum CoreEvent {
    /// A physical key went down (OS auto-repeat reported via `repeat`, which the core
    /// ignores for held actions just like the winit handler does today).
    KeyDown {
        key: PbKey,
        mods: Modifiers,
        repeat: bool,
    },
    /// A physical key was released.
    KeyUp { key: PbKey },
    /// The window lost focus — the core clears held keys (the focus-loss release net).
    FocusLost,
    /// A clock tick (held-key pacing / slideshow dwell are evaluated against it).
    Tick(Instant),
    /// The shell is about to draw and wants the core's current frame decision.
    Redraw,
    /// Surface geometry / display headroom changed.
    Resized {
        width: u32,
        height: u32,
        scale: f32,
        edr_headroom: f32,
    },
    /// Pointer moved (un-hides the cursor; may pan while dragging).
    PointerMoved { x: f32, y: f32 },
    /// Scroll wheel / two-finger scroll (pan or zoom per settings).
    Scroll { dx: f32, dy: f32 },
    /// Pinch / magnify gesture (zoom).
    Pinch { delta: f32 },
    /// Double-tap / double-click (toggle 1:1 ↔ fit).
    DoubleTap,
    /// Files dropped onto the window.
    DroppedPaths(Vec<PathBuf>),
    /// A menu item was chosen — routed through the same [`Action`] vocabulary as keys.
    MenuAction(Action),
    /// The shortcut editor committed a new keymap (Keymap already lives in the core).
    KeymapSubmitted(Keymap),
    /// A dialog was cancelled / dismissed.
    CancelDialog,
    // NS-later (payload types still in the shell or other crates):
    //   Started { surface, size, scale, refresh_hz, edr_headroom }  — GPU surface handle
    //   Open(LaunchInput)            — pb-core::open (ADR-019)
    //   SettingsSubmitted(Settings)  — settings form (shell)
    //   PasswordSubmitted(String)    — paired with the archive open flow
    //   DialogResult(..)             — per-dialog result payloads
}

/// An effect the core asks the shell to carry out. The core never touches the window,
/// menu, dialogs, or panels directly — it returns these and the shell (on its main
/// thread/actor) executes them.
#[derive(Clone, Debug)]
pub enum CoreEffect {
    /// Request a redraw on the next opportunity.
    RequestRender,
    /// Schedule a wake-up at this instant (held-key pacing / slideshow dwell).
    WakeAt(Instant),
    /// Set the window title.
    SetTitle(String),
    /// Set the pointer cursor.
    SetCursor(CursorKind),
    /// Switch window presentation mode.
    SetWindowMode(WindowMode),
    /// Hide the window (Esc teardown step, before exit).
    HideWindow,
    /// Quit the application (clean teardown — privacy #6).
    Quit,
    /// Open the native file picker.
    OpenFilePanel,
    /// Open the native folder picker.
    OpenFolderPanel,
    /// Present a chrome dialog (payload is `NS-later`; see [`DialogKind`]).
    ShowDialog(DialogKind),
    /// Close the open dialog.
    CloseDialog,
    /// Sync the native menu's check/enabled marks.
    SetMenuState(MenuState),
    /// Surface a user-facing error (message dialog / toast).
    ReportError(String),
    /// Write an image or text payload to the system clipboard (an explicit user Copy /
    /// Copy File Path command — never the view path). The shell does the platform write
    /// and surfaces the success/failure toast.
    WriteClipboard(ClipboardPayload),
    // NS-later (payload types still in the shell or other crates):
    //   UpdateDialog(DialogUpdate)        — progress ticks into an open dialog
    //   ShowNativeAbout(AboutInfo)         — the standard NSApplication about panel
}

/// The core's decision for a physical key-down, routed by the resolved [`Action`]'s
/// [`ActionKind`]. The pure output of [`resolve_key_down`]: the shell *executes* it (run
/// the command, begin hold-to-fly, or start tracking a held key), but the decision itself
/// is shell-neutral — winit and AppKit resolve a key identically.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyResolution {
    /// No binding (or an ignored OS auto-repeat) — do nothing.
    Ignore,
    /// Run a one-shot command now (rotate, copy, toggle a panel, open Settings, …).
    OneShot(Action),
    /// Begin hold-to-fly navigation: fire once now, then repeat while the key is held.
    NavStart(Action),
    /// Begin tracking a continuous held action (pan/zoom, applied each frame).
    HeldStart(Action),
    /// Begin an animation frame-step: step one frame now, then repeat while held to
    /// scrub through the frames (`,` / `.`).
    FrameStepStart(Action),
}

/// Resolve a physical key-down against the loaded keymap — the pure heart of the input
/// path, identical on every shell (the winit handler and the future AppKit one both call
/// this). Encapsulates two input-layer policies so no shell has to re-implement them:
///
/// - **OS auto-repeat is ignored** (`repeat == true` → [`KeyResolution::Ignore`]) so a
///   held key can't queue duplicate dispatches; held actions drive their own repeat from
///   the frame loop, and nav its hold-to-fly.
/// - **An unbound chord resolves to `Ignore`** — including a Cmd/⌘ chord with no binding,
///   so holding ⌘ never falls through to a bare-key action (the [`Modifiers::logo`] rule).
pub fn resolve_key_down(
    keymap: &Keymap,
    key: PbKey,
    mods: Modifiers,
    repeat: bool,
) -> KeyResolution {
    if repeat {
        return KeyResolution::Ignore;
    }
    match keymap.action_for(&mods.chord_with(key)) {
        None => KeyResolution::Ignore,
        Some(action) => match action.kind() {
            ActionKind::OneShot => KeyResolution::OneShot(action),
            ActionKind::Nav => KeyResolution::NavStart(action),
            ActionKind::Held => KeyResolution::HeldStart(action),
            ActionKind::FrameStep => KeyResolution::FrameStepStart(action),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_default_is_none() {
        assert_eq!(Modifiers::default(), Modifiers::NONE);
    }

    #[test]
    fn modifiers_build_the_same_chord_the_keymap_is_indexed_by() {
        // The bridge a shell uses: (key, mods) → KeyChord must equal a directly-built
        // chord, so a keymap lookup resolves identically however the chord was formed.
        let mods = Modifiers {
            ctrl: true,
            shift: true,
            alt: false,
            logo: false,
        };
        let via_mods = mods.chord_with(PbKey::KeyR);
        let direct = crate::KeyChord::new(PbKey::KeyR, true, true, false, false);
        assert_eq!(via_mods, direct);

        // And that chord resolves through the real default keymap (Shift+Ctrl... here
        // Ctrl+Shift+R is unbound by default; use a known one instead).
        let copy = Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        }
        .chord_with(PbKey::KeyC);
        assert_eq!(Keymap::defaults().action_for(&copy), Some(Action::Copy));
    }

    #[test]
    fn logo_chord_is_distinct_from_a_ctrl_chord() {
        // Guards the Cmd-vs-Ctrl distinction at the contract layer: ⌘C and Ctrl+C are
        // different chords, so a Mac ⌘ shortcut can't fall through to a Ctrl action.
        let cmd_c = Modifiers {
            logo: true,
            ..Modifiers::NONE
        }
        .chord_with(PbKey::KeyC);
        let ctrl_c = Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        }
        .chord_with(PbKey::KeyC);
        assert_ne!(cmd_c, ctrl_c);
        assert_eq!(Keymap::defaults().action_for(&ctrl_c), Some(Action::Copy));
        assert_eq!(Keymap::defaults().action_for(&cmd_c), None);
    }

    #[test]
    fn menu_state_default_is_the_launch_state() {
        // Fresh launch: nothing pending, fit mode, no overlays — mirrors how the menu
        // handles start (Save Rotation / Stop Scanning / Undo all disabled).
        let m = MenuState::default();
        assert_eq!(m.scale, ScaleMode::Fit);
        assert_eq!(m.info, InfoOverlay::Hidden);
        assert!(!m.recursive && !m.fullscreen && !m.slideshow);
        assert!(!m.save_rotation_enabled && !m.cancel_scan_enabled);
        assert_eq!(m.undo, None);
        assert!(!m.native_fullscreen_engaged);
    }

    #[test]
    fn resolve_key_down_routes_each_action_kind() {
        let km = Keymap::defaults();
        let m = Modifiers::NONE;
        // Nav: Space → Next, started as hold-to-fly.
        assert_eq!(
            resolve_key_down(&km, PbKey::Space, m, false),
            KeyResolution::NavStart(Action::Next),
        );
        // Held: Left → PanLeft, tracked continuously.
        assert_eq!(
            resolve_key_down(&km, PbKey::ArrowLeft, m, false),
            KeyResolution::HeldStart(Action::PanLeft),
        );
        // One-shot: Ctrl+C → Copy, run now.
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        };
        assert_eq!(
            resolve_key_down(&km, PbKey::KeyC, ctrl, false),
            KeyResolution::OneShot(Action::Copy),
        );
        // FrameStep: `.` → FrameNext, scrubbing one animation frame per press/repeat.
        assert_eq!(
            resolve_key_down(&km, PbKey::Period, m, false),
            KeyResolution::FrameStepStart(Action::FrameNext),
        );
    }

    #[test]
    fn resolve_key_down_ignores_unbound_and_repeats() {
        let km = Keymap::defaults();
        // Unbound bare key.
        assert_eq!(
            resolve_key_down(&km, PbKey::KeyD, Modifiers::NONE, false),
            KeyResolution::Ignore,
        );
        // OS auto-repeat is ignored even for a bound key (held actions self-repeat).
        assert_eq!(
            resolve_key_down(&km, PbKey::Space, Modifiers::NONE, true),
            KeyResolution::Ignore,
        );
    }

    #[test]
    fn resolve_key_down_never_lets_a_cmd_chord_fall_through_to_a_bare_key() {
        // The ⌘ guarantee at the input seam: ⌘S is unbound in the default keymap (Save
        // lives on the menu's ⌘S), so it resolves to Ignore — it must NOT fire bare `S`
        // (Slideshow). Same for ⌘R / ⌘O vs bare R / O.
        let km = Keymap::defaults();
        let cmd = Modifiers {
            logo: true,
            ..Modifiers::NONE
        };
        assert_eq!(
            resolve_key_down(&km, PbKey::KeyS, cmd, false),
            KeyResolution::Ignore,
        );
        assert_eq!(
            resolve_key_down(&km, PbKey::KeyS, Modifiers::NONE, false),
            KeyResolution::OneShot(Action::SlideshowToggle),
        );
        assert_eq!(
            resolve_key_down(&km, PbKey::KeyR, cmd, false),
            KeyResolution::Ignore,
        );
    }

    #[test]
    fn events_and_effects_carry_the_shared_action_vocabulary() {
        // Smoke test that the vocabulary composes: a menu action arrives as a CoreEvent
        // and a menu-state sync leaves as a CoreEffect, both shell-neutral.
        let ev = CoreEvent::MenuAction(Action::Next);
        match ev {
            CoreEvent::MenuAction(a) => assert_eq!(a, Action::Next),
            _ => unreachable!(),
        }
        let eff = CoreEffect::SetMenuState(MenuState {
            slideshow: true,
            ..MenuState::default()
        });
        match eff {
            CoreEffect::SetMenuState(m) => assert!(m.slideshow),
            _ => unreachable!(),
        }
    }
}
