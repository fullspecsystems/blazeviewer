//! `pb-hud` — PhotoBlaze's HUD compositor (the CPU rasterizer for on-image overlays).
//!
//! Split out of the winit shell (`pb-app`) in NS0 (ADR-021) so the orchestration core
//! (`pb-app-core`) can own the render path without dragging a font engine + SVG rasterizer
//! into the "no UI toolkit" orchestration crate, and so every shell (winit today,
//! AppKit/SwiftUI + iOS later) composites overlays the same way — or swaps in a native
//! overlay backend behind this seam.
//!
//! - [`hud`] — the panel/toast/pie/chip compositor (text via `fontdue`).
//! - [`icon`] — Font Awesome SVG → white-tinted RGBA sprite (via `resvg`), used by [`hud`]
//!   and by the shell's toast layer.
//!
//! Pure compute: no winit/egui/wgpu/Swift, no filesystem on the draw path — it takes state
//! and returns RGBA bitmaps the renderer uploads as quads.

pub mod hud;
// The subtitle rasterizer (task #90.3): cues + style -> one RGBA8 bitmap, shell-neutral
// so the wgpu overlay and the macOS SwiftUI overlay show identical pixels.
pub mod icon;
pub mod subtitle;
