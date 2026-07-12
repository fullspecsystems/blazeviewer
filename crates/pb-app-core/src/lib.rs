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
/// alone atop `--help` and sits under the "PhotoBlaze" name in the About box.
pub const TAGLINE: &str = "An ultra-fast, capable image viewer";

pub mod action;
pub mod animation;
pub mod app_core;
pub mod app_core_impl;
pub mod archive;
pub mod config;
pub mod contract;
pub mod decode_pool;
pub mod delete;
pub mod describe;
pub mod engine;
pub mod folder_tree;
pub mod follow;
pub mod fs_tree;
pub mod image_text;
pub mod keymap;
pub mod launch;
pub mod meta;
pub mod metrics;
pub mod overlay;
pub mod panels;
pub mod pb_key;
pub mod prompt;
pub mod save_rotation;
pub mod scan;
pub mod settings;
pub mod slideshow;
pub mod thumbs;
pub mod timing;
pub mod undo;
pub mod video;
pub mod video_native;
pub mod video_session;

pub use action::{Action, ActionKind};
pub use app_core::{AppCore, ArchiveScope, Nav, Viewport};
pub use config::config_dir;
pub use contract::{CoreEffect, CoreEvent, KeyResolution, MenuState, Modifiers};
pub use follow::{FollowState, ScrollTo};
pub use keymap::{KeyChord, Keymap};
pub use launch::{LaunchOverrides, SlideshowStart, StartAt};
pub use meta::PhotoMeta;
pub use overlay::{InspectorTab, LeftTab, NativeToast, Panels, SlotContent, Toast, ToastIcon};
pub use pb_key::PbKey;
pub use slideshow::Slideshow;
pub use undo::UndoAction;
pub use video::{
    AudioClockSample, AudioClockState, LibraryItemKind, VideoContainer, VideoMetadata,
    VideoQueueBudget, VideoSessionState,
};
