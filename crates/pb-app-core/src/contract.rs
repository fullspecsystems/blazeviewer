//! The `AppCore` ⇄ shell contract — **the live seam** (NS0, ADR-021).
//!
//! ADR-021 inverts ownership on the macOS target: an AppKit/SwiftUI host owns the
//! window + run loop and drives the Rust engine, where today winit *is* the engine.
//! For anything but winit to drive PhotoBlaze, the orchestration layer speaks a
//! **shell-neutral vocabulary**: intent-level events *in* ([`CoreEvent`]), effects
//! the shell must carry out *out* ([`CoreEffect`]), with the existing [`Action`] kept
//! as the single command vocabulary (the plan's explicit rule).
//!
//! This is **wired and load-bearing** now (NS0 5.5/5.6): the winit shell translates its
//! `WindowEvent`s into [`CoreEvent`]s and calls [`AppCore::handle`](crate::AppCore::handle)
//! for keyboard / pointer / the tick loop / dialog outcomes / the archive+scan worker results,
//! and executes the returned [`CoreEffect`]s in its drain. The macOS host will drive the same
//! `handle` + drain the same effects. A few payloads still reference shell/other-crate types and
//! are noted `NS-later`, but the core input + command + open/scan/archive flows all run through
//! this vocabulary today.

use std::path::PathBuf;
use std::time::Instant;

use crate::{Action, ActionKind, Keymap, PbKey};
use pb_core::open::{Cursor, Source};

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

/// A scroll delta, carrying the same distinction winit's `MouseScrollDelta` does — the core needs
/// it because a line-precise wheel and a pixel-precise trackpad swipe use different zoom/pan steps.
/// **Pixels**: a macOS trackpad two-finger swipe (tens of pixels per event). **Lines**: a real
/// mouse wheel (~1 notch) and — on Windows — a precision-trackpad swipe too (winit reports both as
/// lines there). Both honor the `Scroll wheel` = Pan/Zoom setting (Ctrl flips it).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ScrollDelta {
    Lines { x: f32, y: f32 },
    Pixels { x: f32, y: f32 },
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
    /// Ask-about-image text entry (task #44): a multi-line question about the current
    /// photo, answered by the describe backend.
    AskImage,
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
/// [`ScaleMode`]; the info/panel toggles are independent booleans (the basic line
/// and the Inspector's Details tab decoupled in task #54) — the shell maps those
/// onto its individual items.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MenuState {
    /// View ▸ scale group (exactly one checked).
    pub scale: ScaleMode,
    /// View ▸ Show Image Info — the ephemeral basic `i` line.
    pub info_basic: bool,
    /// View ▸ Show All EXIF Info — the Inspector open on its Details tab (checked
    /// even while `Tab`-hidden: hidden ≠ closed, and Hide Panels explains it).
    pub info_full: bool,
    /// View ▸ Hide Panels — checked while rich panels are `Tab`-hidden (task #54).
    pub panels_hidden: bool,
    /// View ▸ Hide Panels — enabled only with a rich panel open (matches the
    /// `Tab` no-op with nothing to hide).
    pub hide_panels_enabled: bool,
    /// View ▸ Recursive (This Folder).
    pub recursive: bool,
    /// View ▸ Fullscreen (the borderless speed mode checkbox).
    pub fullscreen: bool,
    /// View ▸ Slideshow.
    pub slideshow: bool,
    /// Image ▸ Mute Live Photo Audio — checked when Live Photo audio is muted (#38).
    pub mute_live_audio: bool,
    /// Image ▸ Pin for Compare — enabled with a photo on screen (task #43).
    pub compare_pin_enabled: bool,
    /// Image ▸ Pin for Compare — checked while the current photo IS the pin.
    pub compare_pinned_here: bool,
    /// Image ▸ Compare with Pinned — enabled once a pin exists (the `Y` flip).
    pub compare_toggle_enabled: bool,
    /// File ▸ Save Rotation — enabled only with an unsaved rotation on a writable file.
    pub save_rotation_enabled: bool,
    /// File ▸ Show in Finder/Explorer — enabled only for a real on-disk file (not an
    /// archive entry or the empty deck).
    pub reveal_enabled: bool,
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

/// What the right-click **photo context menu** should offer for the current photo (task
/// #41) — the shell-neutral description the host turns into a native popup (a muda `Menu`
/// now, an `NSMenu` later). The core fills it from live state on right-click; the shell
/// builds + shows the menu. Items whose target doesn't apply are simply omitted.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ContextMenuState {
    /// A real photo is displayed (not the empty deck) — the per-photo items are shown. The
    /// core omits the whole menu when this is false, so `true` in practice today.
    pub has_image: bool,
    /// The current photo has a motion component (animated container / Live Photo) → include
    /// the Play/Pause item.
    pub has_motion: bool,
    /// The current photo is a real on-disk file (not an archive entry) → include Show in
    /// Finder/Explorer.
    pub can_reveal: bool,
    /// Currently in the borderless fullscreen speed mode → the Fullscreen item reads
    /// "Exit Fullscreen" (else "Enter Fullscreen"). Especially useful in fullscreen, where
    /// the menu bar is hidden and the only other way out is a keyboard shortcut.
    pub fullscreen: bool,
    /// A compare pin exists (task #43) → include "Compare with Pinned" (the `Y` flip).
    /// "Pin for Compare" is always offered (it reads "Unpin" when `pinned_here`).
    pub compare_pinned: bool,
    /// The displayed photo IS the pin → the pin item reads "Unpin from Compare".
    pub compare_pinned_here: bool,
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
    /// Plain text — a file path, an archive entry's name, or text recognized in the
    /// photo (task #45). `toast` overrides the shell's default "Copied …" feedback
    /// when the core knows better (e.g. "Copied text + 1 QR code"); `None` keeps the
    /// shell's path-based heuristic.
    Text { text: String, toast: Option<String> },
}

/// What the user did in a chrome dialog — the shell-neutral result the host hands the core
/// (via [`CoreEvent::DialogResolved`]) after it drives the dialog UI and extracts any payload
/// (the edited settings/keymap, a confirm answer). The core runs the reaction (apply settings,
/// confirm a delete, cancel an in-flight op) and emits the housekeeping effects (close the
/// dialog, cancel the worker). NS0 5.6 — the *results* half of the dialog seam, now core-owned.
/// (The password-submit case still spawns the archive worker, so it stays a shell path for now.)
#[derive(Clone, Debug)]
pub enum DialogResult {
    /// Esc / close button dismissed a dialog of this kind (cancels the matching in-flight op).
    Dismissed(Option<DialogKind>),
    /// Password entry submitted (archive unlock); `None` if extraction failed. The core shows the
    /// "Checking…" state and re-opens the pending archive with the entry (via `BeginArchiveOpen`).
    PasswordSubmitted(Option<String>),
    /// The password prompt's Cancel — abandon the pending archive.
    PasswordCancelled,
    /// An ask-about-image question was submitted (task #44): run it through the describe
    /// backend for the current photo. Empty/whitespace is ignored by the core.
    AskSubmitted(String),
    /// Settings saved, carrying the (optionally) edited settings + keymap. The
    /// settings payload is boxed: it rides inside [`CoreEvent`], which travels on
    /// every keypress — the rare dialog result pays the indirection so the hot
    /// event stays small (clippy `large_enum_variant`).
    SettingsSaved {
        settings: Option<Box<crate::settings::Settings>>,
        keymap: Option<Keymap>,
    },
    /// A live edit from an auto-saving Settings window (the macOS idiom — no Save
    /// button): apply + persist the payload immediately, but the window stays open,
    /// so — unlike [`DialogResult::SettingsSaved`] — no `CloseDialog` is emitted.
    SettingsEdited {
        settings: Option<Box<crate::settings::Settings>>,
        keymap: Option<Keymap>,
    },
    /// Settings dialog's Cancel, or the window closing in an auto-saving shell (where
    /// edits were already applied live, so "cancel" only clears the dialog-open state).
    /// Its Esc goes through [`DialogResult::Dismissed`].
    SettingsCancelled,
    /// The archive "Opening…" dialog's Cancel.
    LoadingCancelled,
    /// The folder "Scanning…" dialog's Cancel.
    ScanningCancelled,
    /// A Confirm dialog answered (`true` = the destructive action was confirmed).
    ConfirmAnswered(bool),
    /// A Message (or any other) dialog's OK / close.
    Closed,
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
    /// The OS light/dark theme changed (winit `ThemeChanged` / AppKit
    /// `viewDidChangeEffectiveAppearance`), or its initial value at startup. The core
    /// re-resolves the `Appearance` preference (task #46) and re-themes the HUD +
    /// letterbox when the effective theme actually flipped.
    OsThemeChanged { dark: bool },
    /// A clock tick (held-key pacing / slideshow dwell are evaluated against it).
    Tick(Instant),
    /// The shell is about to draw and wants the core's current frame decision.
    Redraw,
    /// The surface resized (or its backing scale changed): the core updates the viewport + the
    /// fit box, reconfigures the swapchain (`renderer.resize`), rescales the CPU overlays on a
    /// scale change, and debounces a crisp decode-to-fit. The host does its platform-specific
    /// surface bits around this (the macOS EDR re-assert + the redraw) — see the winit shell.
    Resized { width: u32, height: u32, scale: f32 },
    /// Pointer moved (un-hides the cursor; may pan while dragging).
    PointerMoved { x: f32, y: f32 },
    /// Scroll wheel / two-finger scroll — pan or zoom per the `Scroll wheel` setting (Ctrl flips
    /// it), with the line-vs-pixel distinction the core needs for the right step size.
    Scroll(ScrollDelta),
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
    /// A chrome dialog resolved (Save/Cancel/OK/Esc) — the host drove the UI + extracted any
    /// payload; the core runs the reaction + emits the close/cancel effects. NS0 5.6.
    DialogResolved(DialogResult),
    /// The streaming directory-scan worker produced a growing playlist snapshot (NS0 5.6 Step 3).
    /// The host owns the worker thread + generation check; the core filters deleted items, then
    /// bootstraps the playlist on the first non-empty batch (`scan_bootstrapped`) or extends it.
    ScanBatch(crate::scan::Resolved),
    /// The streaming directory scan finished (NS0 5.6 Step 3): the core resumes normal prefetch
    /// and — if nothing was ever shown and the deck is empty — restores the "Press O to open" hint.
    ScanDone,
    /// A background archive open resolved to a non-empty playlist (NS0 5.6 Step 3): the core
    /// installs it (`rebuild_playlist`) and forgets any pending password. The host owns the worker
    /// thread + generation check + closing the progress dialog; the *failure* cases (empty /
    /// password-required / cancelled / error) stay host-side (they drive native dialogs).
    ArchiveResolved(crate::scan::Resolved),
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
    /// Set when the core next wants to be ticked: `Some(at)` → wake at that instant (held-key
    /// pacing / slideshow dwell / the animation's next-frame deadline), `None` → go idle until
    /// the next real event. Emitted by the `Tick` handler; the host takes the min of this and any
    /// host-side wake (e.g. the winit shell's dialog-repaint deadline) for its control-flow.
    SetWake(Option<Instant>),
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
    /// Open the native file picker (images + archives filter) at `start_dir`. The shell
    /// runs the modal panel and re-enters the core with the picked paths.
    OpenFilePanel { start_dir: PathBuf },
    /// Open the native folder picker at `start_dir`.
    OpenFolderPanel { start_dir: PathBuf },
    /// Start opening an archive off the event loop (NS0 5.6 Step 3): the host spawns the worker
    /// (a `.zip` is synchronous; a `.7z` decompresses on a thread after a RAM pre-flight), holds
    /// the progress-dialog handle + generation, and feeds the result back as `ArchiveResolved` (or
    /// drives the failure dialogs). `password` is `Some` only on a re-open with an entered password.
    BeginArchiveOpen {
        path: PathBuf,
        password: Option<String>,
    },
    /// Start scanning a folder off the event loop (NS0 5.6 Step 3): the host spawns the streaming
    /// walk worker, holds its handle + generation + progress dialog, and feeds snapshots back as
    /// `ScanBatch` / `ScanDone`.
    BeginDirScan { source: Source, cursor: Cursor },
    /// Present a chrome dialog (payload is `NS-later`; see [`DialogKind`]).
    ShowDialog(DialogKind),
    /// Close the open dialog.
    CloseDialog,
    /// Put the open password dialog into its "Checking…" state (while the just-entered password is
    /// validated — a zip is synchronous, a 7z re-opens off-thread). NS0 5.6. No-op if not a password
    /// dialog.
    SetDialogChecking,
    /// Cancel the in-flight directory scan (request the worker stop + drop its handle). Emitted
    /// when the Scanning dialog is dismissed / cancelled. No-op if no scan is running. NS0 5.6.
    CancelScan,
    /// Cancel the in-flight archive open (request the worker stop; the poll frees it). Emitted
    /// when a dialog dismiss / the Loading-cancel abandons an open. No-op if none. NS0 5.6.
    CancelArchiveLoad,
    /// Sync the native menu's check/enabled marks.
    SetMenuState(MenuState),
    /// A **natively-presented** rich panel's visibility or content changed (task #54,
    /// mac-first): a marker telling the host to re-pull the panel model (`help_rows` /
    /// `help_visible`, etc.) and update its native view. Emitted only when the host has
    /// declared it presents that panel natively (so the core suppressed the panel's HUD
    /// rasterization); the winit shell, which keeps the HUD panels, never sees it.
    PanelsChanged,
    /// Surface a user-facing error (message dialog / toast).
    ReportError(String),
    /// Write an image or text payload to the system clipboard (an explicit user Copy /
    /// Copy File Path command — never the view path). The shell does the platform write
    /// and surfaces the success/failure toast.
    WriteClipboard(ClipboardPayload),
    /// Reveal a file in the OS file manager — open its containing folder and select it
    /// (macOS `NSWorkspace`/`open -R`, Windows `explorer /select`, Linux best-effort
    /// `xdg-open <dir>`). An explicit user command on a path already being viewed: it only
    /// launches the file manager, never reads pixels or writes a trace (privacy #2, same
    /// category as Copy File Path). The core validates a real on-disk file exists before
    /// emitting this; the shell runs the per-OS launch behind a small helper.
    RevealPath(PathBuf),
    /// Show the right-click **photo context menu** at the cursor (task #41). The core emits
    /// this on a secondary-click over a photo, carrying the item description; the shell builds
    /// a native popup from it (a muda `Menu` at the cursor now, an `NSMenu` later) whose clicks
    /// arrive as `MenuAction`s on the shared dispatch path — no parallel wiring.
    ShowContextMenu(ContextMenuState),
    /// Start (or restart) the Live Photo's audio — its companion `.mov` track — at `at_secs`,
    /// replacing any currently-playing clip. The shell owns the `AVAudioPlayer` handle (an ObjC
    /// object that can't live in the platform-neutral core); the core only decides *when* audio
    /// should play and from *where* (task #38). A no-op on non-macOS (the stub player).
    StartLiveAudio { path: PathBuf, at_secs: f64 },
    /// Stop and drop the Live Photo audio (navigate away / pause-to-step / mute / finish).
    StopLiveAudio,
    /// Pause the playing Live Photo audio, leaving it resumable at the same position.
    PauseLiveAudio,
    /// Resume the paused Live Photo audio.
    ResumeLiveAudio,
    /// Open the **video item's** audio track, PAUSED, for the playing `VideoSession`
    /// (task #79 phase 5). The shell owns the platform player (WinRT `MediaPlayer`)
    /// and reports its clock back ~4×/s via `AppCore::video_audio_clock` — the audio
    /// side of the core⇄shell clock bridge. `muted` applies the user's mute state at
    /// open (the clock still runs muted, so A/V sync is mute-independent). `input`
    /// is the same container the video producer reads: a path for a loose file, or
    /// an archive entry's `Arc`-shared in-RAM bytes (one copy feeds both pipelines).
    StartVideoAudio {
        input: crate::video::VideoInput,
        session_id: crate::video::VideoSessionId,
        muted: bool,
    },
    /// Drop the video audio player (session ended / stopped / navigated away).
    StopVideoAudio,
    /// Pause the video audio (session paused or rebuffering — freeze together).
    PauseVideoAudio,
    /// Seek the video audio to `position` (task #79 phase 6). The ack is implicit:
    /// the clock bridge ignores samples until one reports a position near the
    /// target, so a stale pre-seek sample can never re-anchor the session.
    SeekVideoAudio { position: std::time::Duration },
    /// Start/resume the video audio (session entered `Playing` — resume together).
    ResumeVideoAudio,
    /// Mute/unmute the video audio in place (the mute toggle while a video plays).
    SetVideoAudioMuted(bool),
    // ── macOS native video (task 79.9) ───────────────────────────────────────
    // On macOS the whole media pipeline is the shell's native `AVPlayer` +
    // `AVPlayerLayer` (system decode/color/HDR/audio/timing/seeking) — NOT the
    // Windows/Linux `VideoSession` + its separate audio player (the effects
    // above). These command that player; `AppCore` keeps only a passive
    // `NativeVideoProxy` mirror (see `video_native`). Every command carries a
    // `session_id` so a stale player is ignored; seeks carry a `generation` token
    // so a superseded seek's async completion can't affect the current session.
    // AVPlayer is the single timing authority — the core issues no position, only
    // relative/fractional intent it resolves against its own clock.
    /// Open `path` in the native player for `session_id`, honoring `muted`. The
    /// shell opens paused, prerolls, reveals the layer on the first displayable
    /// frame, then plays (so audio never leads the picture over the poster).
    PlayVideo {
        path: PathBuf,
        session_id: crate::video::VideoSessionId,
        muted: bool,
    },
    /// Play a video from **in-RAM bytes** — an archive (ZIP/7z) entry, which has no file
    /// URL (task #30 macOS parity). The container bytes are stashed for the shell to pull
    /// (`AppCore::take_pending_video_bytes`), then served to `AVPlayer` through a custom
    /// resource loader — never extracted to disk (privacy #2). `name` carries the entry's
    /// real extension so the shell can resolve the content type.
    PlayVideoBytes {
        name: String,
        session_id: crate::video::VideoSessionId,
        muted: bool,
    },
    /// Pause the native player (`P` while playing; rebuffer never applies — the
    /// system player owns buffering).
    PauseVideo {
        session_id: crate::video::VideoSessionId,
    },
    /// Resume the native player (`P` while paused).
    ResumeVideo {
        session_id: crate::video::VideoSessionId,
    },
    /// Seek by a signed delta from the player's current time (the ±2 s / Shift
    /// ±10 s arrow-seek + hold-scrub). The core owns the delta magnitude; the
    /// shell resolves it against `AVPlayer`'s clock and clamps to the seekable
    /// range. `generation` supersedes any older in-flight seek.
    SeekVideoBy {
        session_id: crate::video::VideoSessionId,
        generation: crate::video::SeekGeneration,
        delta_ms: i64,
    },
    /// Seek to `fraction` (0..=1) of the duration — the info-line scrubber. Same
    /// generation contract as [`SeekVideoBy`](Self::SeekVideoBy).
    SeekVideoFraction {
        session_id: crate::video::VideoSessionId,
        generation: crate::video::SeekGeneration,
        fraction: f32,
    },
    /// Frame-step while paused (native `AVPlayerItem.step(byCount:)`; the shell
    /// honors `canStepForward`/`canStepBackward` and no-ops when unsupported).
    StepVideo {
        session_id: crate::video::VideoSessionId,
        forward: bool,
    },
    /// Mute/unmute the native player in place.
    SetVideoMuted {
        session_id: crate::video::VideoSessionId,
        muted: bool,
    },
    /// Tear down the native player (navigate away / delete / stop / failure). The
    /// shell hides + detaches the layer and cancels all observers; stale callbacks
    /// are rejected by `session_id`.
    StopVideo {
        session_id: crate::video::VideoSessionId,
    },
    /// Capture the *displayed* frame for an explicit user command (Copy / OCR /
    /// Describe / Compare). The shell replies via `AppCore::native_video_frame_ready`
    /// with the pixels; `request_id` + `session_id` + `item` reject a frame captured
    /// before a navigation. On-demand only — never a continuous copy path.
    CaptureNativeVideoFrame {
        session_id: crate::video::VideoSessionId,
        item: usize,
        purpose: crate::video::NativeCapturePurpose,
        request_id: u64,
    },
    /// Perform a genuinely **host-side command** — one whose execution *is* a platform
    /// operation, not core orchestration. After NS0 5.6 this carries the residue that can't be
    /// pure core: **DeletePermanent** (opens the themed confirm dialog; the Yes then calls the
    /// core `do_delete`), **Recursive** / **CancelScan** (spawn / cancel the off-thread directory
    /// walk + its progress dialog), and **Quit** (hide-window teardown, also reached from the
    /// window-close / Esc paths). `AppCore::dispatch_action` routes these here so the *whole*
    /// action vocabulary still dispatches through one core entry point; the host matches on the
    /// `Action` and runs the native operation. The core-owned commands (nav / zoom / scale /
    /// rotate / copy / info / slideshow / play / **mute** / **save-rotation** / **undo** /
    /// **delete-to-trash** / **fullscreen**) and the dialog opens (**About** / **Settings** →
    /// `ShowDialog`) have been lifted OUT of this seam into their own core arms / effects.
    ShellFlowAction(Action),
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
        assert!(!m.info_basic && !m.info_full);
        assert!(!m.panels_hidden && !m.hide_panels_enabled);
        assert!(!m.recursive && !m.fullscreen && !m.slideshow);
        assert!(!m.save_rotation_enabled && !m.cancel_scan_enabled && !m.reveal_enabled);
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
        // Unbound bare key (J has no default binding; D is now Describe).
        assert_eq!(
            resolve_key_down(&km, PbKey::KeyJ, Modifiers::NONE, false),
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
