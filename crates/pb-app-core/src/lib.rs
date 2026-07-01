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

pub mod action;
pub mod app_core;
pub mod config;
pub mod contract;
pub mod decode_pool;
pub mod keymap;
pub mod meta;
pub mod metrics;
pub mod overlay;
pub mod pb_key;
pub mod settings;
pub mod slideshow;
pub mod timing;

pub use action::{Action, ActionKind};
pub use app_core::{AppCore, Nav, Viewport};
pub use config::config_dir;
pub use contract::{CoreEffect, CoreEvent, KeyResolution, MenuState, Modifiers};
pub use keymap::{KeyChord, Keymap};
pub use meta::PhotoMeta;
pub use overlay::{InfoMode, OpenButton, OpenPanel, PlayHint, Toast};
pub use pb_key::PbKey;
pub use slideshow::Slideshow;
