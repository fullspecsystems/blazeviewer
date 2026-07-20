//! `pb-app-core` — PhotoBlaze's platform-neutral orchestration model (NS0, ADR-021).
//!
//! The winit shell (`pb-app`) used to own the command vocabulary, the physical-key
//! model, and the keymap directly. The macOS native-UI track ([ADR-021]) needs the
//! *same* model driven by an AppKit/SwiftUI host instead of winit, so it is lifted
//! here — into a crate with **no winit/egui/wgpu/Swift dependency**. Each shell
//! supplies a thin adapter from *its* key/event types into these:
//!
//! - winit: `pb_app::pb_key_winit` (`winit::KeyCode → PbKey`);
//! - AppKit (future): an `NSEvent → PbKey` mirror.
//!
//! What lives here is shell-neutral but *not* necessarily I/O-free: the keymap
//! loader reads/writes `keymap.toml` under [`config_dir`]. That's platform code,
//! not shell code — the distinction the crate boundary enforces is "no UI toolkit,"
//! not "no filesystem." (Contrast `pb-core`, which is strictly pure.)
//!
//! Privacy (task #2): the only disk access here is read-only keymap config plus an
//! explicit user-initiated keymap save — never anything photo-derived.
//!
//! [ADR-021]: ../../../.taskmaster/docs/decisions.md

/// The one-line product tagline, shared across every surface that shows it — the CLI
/// `--help` header (`pb-cli`), the About dialog, and the Windows file-association
/// description — so the three can never drift apart again. Keep it short: it stands
/// alone atop `--help` and sits under the [`APP_NAME`] in the About box.
pub const TAGLINE: &str = "An ultra-fast image and video viewer";

/// The product's **display name** — the form a user reads. The About box, the Windows
/// `ApplicationName` capability, Explorer's ProgId labels, the folder verb.
///
/// Deliberately separate from [`APP_IDENT`]: a display name may contain spaces where an
/// identifier may not. Do not collapse them back into one constant.
pub const APP_NAME: &str = "Blaze Viewer";

/// The product name as a **space-free identifier**. Used for the Windows ProgIds
/// (`<APP_IDENT>.Image`), the `SOFTWARE\<APP_IDENT>` registry tree, the
/// `RegisteredApplications` value name — and the `ms-settings:defaultapps?registeredApp*=`
/// URI that references it, where a space would need percent-encoding — plus the
/// single-instance mutex, and the config-dir name on Windows/macOS.
///
/// Not [`APP_NAME`] (may have spaces) and not [`APP_SLUG`] (lowercase); this is the
/// PascalCase middle ground.
pub const APP_IDENT: &str = "BlazeViewer";

/// The **lowercase slug**: the executable name (`<APP_SLUG>.exe`) and the Linux config
/// dir (`~/.config/<APP_SLUG>`).
pub const APP_SLUG: &str = "blazeviewer";

pub mod action;
pub mod animation;
pub mod app_core;
pub mod app_core_impl;
pub mod archive;
// The archive-open worker lifecycle (task #126 step 2), the companion to `dir_scan`: one-shot,
// retrying and secret-bearing where the walk is streaming. Read its privacy note before
// touching the password path.
pub mod archive_open;
pub mod background;
pub mod config;
pub mod contract;
pub mod cues;
pub mod decode_pool;
pub mod delete;
pub mod describe;
pub mod dir_scan;
pub mod engine;
pub mod folder_tree;
pub mod follow;
pub mod fs_tree;
pub mod image_text;
pub mod keymap;
pub mod launch;
// The off-thread video Details probe (task #98): opening a container is an unbounded
// wait, so it never happens on the event loop.
pub mod media_details;
pub mod meta;
pub mod metrics;
pub mod perf;
// Double-encoded-UTF-8 repair (task #90.2). Subtitles are the worst-encoded text on a
// computer; this undoes the `â™ª`-for-`♪` transform when — and only when — it can prove
// the text really was double-encoded. Pure.
pub mod mojibake;
pub mod overlay;
pub mod panels;
pub mod pb_key;
pub mod poster_select;
pub mod prompt;
pub mod retry;
pub mod save_rotation;
pub mod scan;
pub mod secret;
pub mod settings;
// Sidecar subtitle discovery (task #90.1): pure matching rules over a list of sibling
// names, so one implementation serves both loose files and archive entries.
pub mod sidecar;
pub mod slideshow;
// Subtitle selection + style + placement (task #90.3/#90.4): the pure decisions behind
// the overlay — which track shows, how it looks, and where it goes.
pub mod subtitle;
// The runtime that joins them to the screen: workers, the cue clock, and the bitmap +
// rect both shells composite.
pub mod subtitle_engine;
// The Settings preview (task #90.4): a sample frame drawn with the REAL rasterizer and
// the REAL placement math, so it cannot drift from what a film actually shows.
pub mod subtitle_preview;
pub mod thumbs;
pub mod timing;
// The shared media-track formatter (task #98): one place a track turns into the line
// a human reads — Details now, the #99 picker later.
pub mod tracks;
pub mod undo;
pub mod video;
pub mod video_native;
pub mod video_session;

pub use action::{Action, ActionKind};
pub use app_core::{AppCore, ArchiveScope, FitStash, Nav, PreviewWatchdog, Viewport};
pub use config::config_dir;
pub use contract::{CoreEffect, CoreEvent, KeyResolution, MenuState, Modifiers};
pub use follow::{FollowState, ScrollTo};
pub use keymap::{KeyChord, Keymap};
pub use launch::{LaunchOverrides, SlideshowStart, StartAt};
pub use meta::PhotoMeta;
pub use overlay::{InspectorTab, LeftTab, NativeToast, Panels, SlotContent, Toast, ToastIcon};
/// The FFmpeg video-audio decoder (task #84 §7), re-exported so the macOS FFI
/// crate can feed its AVAudioEngine sink without a direct pb-decode edge.
#[cfg(feature = "ffvideo")]
pub use pb_decode::{AudioError, FfAudioDecoder};
/// The demux-only compressed packet source (video-overhaul Phase 3), re-exported
/// so the macOS FFI crate can feed the AVSampleBufferDisplayLayer presenter
/// without a direct pb-decode edge.
#[cfg(feature = "ffvideo")]
pub use pb_decode::{DemuxPacket, DemuxStreamInfo, DoviConfig, VideoCodec, VideoDemuxer};
pub use pb_key::PbKey;
pub use secret::SecretString;
pub use slideshow::Slideshow;
pub use undo::UndoAction;
pub use video::{
    AudioClockSample, AudioClockState, LibraryItemKind, VideoContainer, VideoMetadata,
    VideoQueueBudget, VideoSessionState,
};
