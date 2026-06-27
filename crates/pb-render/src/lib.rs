//! `pb-render` — GPU presentation and the geometry that frames each photo.
//!
//! Phase 1: a **wgpu** presenter (DX12 backend on Windows, Metal on macOS) that
//! draws one image as a textured quad, letterboxed to the screen via `fit_rect`
//! (no crop) over a dark clear color. The same scene path serves the on-screen
//! surface ([`WgpuRenderer`]) and the headless golden-image tests
//! ([`render_offscreen`]).
//!
//! The pure **fit-to-screen** math lives in [`fit`]; it is deterministic,
//! unit-testable, and on the hot path every frame.

pub mod fit;
pub mod gpu;

pub use fit::{fit_rect, FitRect};
pub use gpu::{render_offscreen, test_pattern, WgpuRenderer, LETTERBOX};

/// How the image is sized to the viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScaleMode {
    /// Scale to fit the viewport, preserving aspect, no crop (the default).
    #[default]
    Fit,
    /// Native pixel-for-pixel size, centered (may overflow the viewport; no pan).
    Original,
}

/// The swappable rendering seam (A/B backends slot in here; see ADR-002).
pub trait Renderer {
    /// React to a surface/window resize.
    fn resize(&mut self, width: u32, height: u32);
    /// Replace the displayed image with a new RGBA8 buffer (`width*height*4`).
    fn set_image(&mut self, rgba: &[u8], width: u32, height: u32);
    /// Choose how the image is sized to the viewport (fit vs. original).
    fn set_scale_mode(&mut self, mode: ScaleMode);
    /// Draw and present one frame.
    fn render(&mut self) -> Result<(), RenderError>;
}

/// A renderer error the app layer can handle without depending on wgpu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderError {
    /// The GPU is out of memory — fatal.
    OutOfMemory,
}
