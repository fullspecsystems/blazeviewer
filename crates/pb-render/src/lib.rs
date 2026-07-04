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

pub mod display;
pub mod fit;
pub mod gpu;
pub mod upload;
pub mod view;

pub use fit::{cover_rect, fit_rect, original_rect, FitRect};
pub use gpu::{render_offscreen, render_offscreen_color, test_pattern, WgpuRenderer, LETTERBOX};
pub use upload::{StagingUpload, UploadStrategy};
pub use view::{Placement, Rotation, ViewTransform, MAX_ZOOM, MIN_ZOOM};

/// A source→sRGB color conversion the fragment shader applies per texel: a 3×3
/// matrix (source-linear RGB → sRGB-linear RGB, row-major) plus the source EOTF as
/// moxcms's 7-param parametric curve `(g, a, b, c, d, e, f)`. `enabled == false`
/// means "sRGB or unknown" — the shader passes the texel through unchanged, so the
/// common case stays bit-exact and free. Built in `pb-decode` from the image's ICC
/// profile and handed in via [`Renderer::set_image`] / [`Renderer::upload_slot`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorTransform {
    pub matrix: [[f32; 3]; 3],
    pub trc: [f32; 7],
    pub enabled: bool,
}

impl ColorTransform {
    /// The sRGB passthrough (disabled) transform.
    pub const fn srgb() -> Self {
        Self {
            matrix: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            trc: [1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            enabled: false,
        }
    }
}

impl Default for ColorTransform {
    fn default() -> Self {
        Self::srgb()
    }
}

/// How the image is sized to the viewport (the base scale of a [`ViewTransform`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScaleMode {
    /// Scale to fit the viewport, preserving aspect, no crop (the default).
    #[default]
    Fit,
    /// Cover the viewport, preserving aspect, cropping the overflow.
    Fill,
    /// Native pixel-for-pixel size, centered (may overflow the viewport).
    Original,
}

/// The swappable rendering seam (A/B backends slot in here; see ADR-002).
pub trait Renderer {
    /// React to a surface/window resize.
    fn resize(&mut self, width: u32, height: u32);
    /// Replace the displayed image. `rgba` is `width*height*4` RGBA8 (sRGB-encoded)
    /// unless `hdr`, in which case it is `width*height*8` `Rgba16Float` scene-linear
    /// scRGB. `color` is the source→sRGB transform (SDR); `peak` is the scene-linear
    /// peak (HDR tone-map white point on an SDR display).
    fn set_image(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        color: ColorTransform,
        hdr: bool,
        peak: f32,
    );
    /// Drop the displayed image and show a blank background (the letterbox fill)
    /// instead — the bare-launch / no-images / last-photo-deleted empty state. The
    /// next [`set_image`](Renderer::set_image) / [`present_slot`](Renderer::present_slot)
    /// restores a photo.
    fn clear_image(&mut self);
    /// Set the per-photo view transform (scaling mode + rotation + zoom + pan).
    fn set_view(&mut self, view: ViewTransform);
    /// Set or clear the corner info-panel overlay: an RGBA8 bitmap (`w*h*4`)
    /// drawn alpha-blended `margin` px in from the bottom-right. `None` hides it.
    fn set_overlay(&mut self, panel: Option<(&[u8], u32, u32)>, margin: u32);

    /// Allocate a resident texture ring of `capacity` slots (Phase 3). `slot_w`/
    /// `slot_h` are the intended slot size for the fixed-size variant; the v1
    /// image-sized implementation ignores them. Resets any existing ring.
    fn reserve_ring(&mut self, capacity: usize, slot_w: u32, slot_h: u32);
    /// Upload a decoded image into ring slot `slot`, baking its `color`/`hdr`/`peak`
    /// into the slot's bind group (see [`Renderer::set_image`] for the buffer layout).
    /// Runs during prefetch, off the keypress frame — so the later `present_slot`
    /// rebind carries everything for free.
    #[allow(clippy::too_many_arguments)]
    fn upload_slot(
        &mut self,
        slot: usize,
        rgba: &[u8],
        w: u32,
        h: u32,
        color: ColorTransform,
        hdr: bool,
        peak: f32,
    );
    /// Select ring slot `slot` as the displayed image (the keypress fast path: a
    /// rebind, no decode or upload). A no-op if the slot isn't uploaded yet.
    fn present_slot(&mut self, slot: usize);

    /// Override the letterbox / background fill color (sRGB), shown around a photo
    /// that doesn't cover the screen. Takes effect on the next `render`. Off the
    /// photo hot path — set from user settings, not per frame.
    fn set_letterbox(&mut self, rgb: [u8; 3]);
    /// Set or clear the transient bottom-center status toast. Its own overlay layer,
    /// so it composites *over* the info panel rather than replacing it; the caller
    /// fades it by re-uploading with scaled alpha. `bottom_margin` is the gap from
    /// the bottom edge.
    fn set_toast(&mut self, panel: Option<(&[u8], u32, u32)>, bottom_margin: u32);
    /// Set or clear the top-right "loading" pie (shown while the next photo isn't
    /// ready). Its own overlay layer, composited above the photo and the panels;
    /// the caller animates the fill / fade by re-uploading. `margin` is the gap from
    /// the top and right edges.
    fn set_pie(&mut self, panel: Option<(&[u8], u32, u32)>, margin: u32);
    /// Set or clear the top-right **scan-count chip** ("12 / 1234…"). `right_margin`
    /// aligns its right edge with the pie; `top_margin` is its top inset. Its own
    /// overlay layer, drawn like the pie.
    fn set_chip(&mut self, panel: Option<(&[u8], u32, u32)>, right_margin: u32, top_margin: u32);
    /// Set or clear the centered message panel (the empty-state "Press O to open…"
    /// hint). Its own overlay layer, centered on both axes; persists until a photo is
    /// shown (`set_image` / `present_slot` clear it).
    fn set_message(&mut self, panel: Option<(&[u8], u32, u32)>);
    /// Set or clear the **folder-tree panel** (`Shift+F`): an RGBA8 bitmap drawn
    /// alpha-blended `margin` px in from the **top-left corner** — the info
    /// panel's bottom-right inset, mirrored, so the two panels frame the photo
    /// concentrically. Its own overlay layer. `None` hides it.
    fn set_tree(&mut self, panel: Option<(&[u8], u32, u32)>, margin: u32);

    /// The currently displayed image's texture dimensions (for pan-clamp math).
    fn image_size(&self) -> (u32, u32);
    /// Set the EDR highlight roll-off target (macOS) — the headroom of the screen the
    /// **window** is on. `1.0` = clamp HDR to SDR white. Re-writes the present uniform
    /// so it takes effect immediately.
    fn set_edr_headroom(&mut self, headroom: f32);
    /// macOS: how to configure the window's `CAMetalLayer` for this surface. `Some`
    /// when the surface is fp16 scRGB (wide-gamut/HDR); the bool is whether to also
    /// request EDR headroom. `None` for a plain SDR 8-bit surface.
    fn hdr_surface_wants_edr(&self) -> Option<bool>;
    /// Process completed GPU work and free dropped resources (the previous image's
    /// texture). Call once per frame so rapid navigation doesn't let GPU memory pile up.
    fn poll(&self);

    /// Draw and present one frame. `Ok(true)` = a frame was presented. `Ok(false)` =
    /// the frame was **dropped** (surface Lost/Outdated/Timeout — routine during
    /// window-resize/fullscreen-transition churn): the surface was reconfigured or
    /// skipped and nothing reached the screen, so the caller must schedule a retry —
    /// otherwise the compositor keeps showing the previous frame indefinitely (the
    /// Mac host's "unfilled background after a fullscreen toggle" bug, 2026-07-04).
    fn render(&mut self) -> Result<bool, RenderError>;
}

/// A renderer error the app layer can handle without depending on wgpu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderError {
    /// The GPU is out of memory — fatal.
    OutOfMemory,
}
