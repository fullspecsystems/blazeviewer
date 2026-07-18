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
pub mod yuv;

pub use fit::{cover_rect, fit_rect, original_rect, FitRect};
pub use gpu::{
    render_offscreen, render_offscreen_color, render_offscreen_nv12, test_pattern, WgpuRenderer,
    LETTERBOX,
};
pub use upload::{StagingUpload, UploadStrategy};
pub use view::{Placement, Rotation, ViewTransform, MAX_ZOOM, MIN_ZOOM};
pub use yuv::{PlanarFormat, PlanarTransfer, YuvMatrix, YuvParams};

/// Everything [`Renderer::set_video_planar`] needs to display one planar video
/// frame (task #91 Phase 2): the storage precision, the transfer to invert, the
/// YUV matrix + range, the primaries/parametric `color` transform, and the HDR
/// tone-map `peak`. A typed argument rather than a long parameter list (Codex).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanarPresentation {
    /// Storage precision of the two planes (NV12 8-bit / P010 16-bit).
    pub format: PlanarFormat,
    /// Transfer function to invert (SDR sRGB/parametric, or HDR PQ/HLG).
    pub transfer: PlanarTransfer,
    /// YUV matrix family + full/limited range (applied in-shader exactly once).
    pub yuv: YuvParams,
    /// Primaries matrix (source-linear → sRGB-linear, for the non-sRGB transfers)
    /// + the parametric TRC. Passthrough for sRGB-like BT.709 SDR.
    pub color: ColorTransform,
    /// Scene-linear peak (HDR tone-map white point on an SDR display); 1.0 for SDR.
    pub peak: f32,
}

/// Serializes GPU-device creation across this crate's tests. The libtest harness runs
/// tests in parallel by default, and some virtual GPU drivers — notably **VMware SVGA 3D**
/// on a Windows-on-ARM VM — fault (`STATUS_ACCESS_VIOLATION`) under *concurrent* wgpu
/// device creation, even though each device works fine on its own. The headless
/// golden-image and staging tests hold this lock for the span of their GPU work so only
/// one device is ever being created/used at a time. On a healthy driver it's a negligible
/// serialization; on a fragile one it's the difference between green and a crash (letting
/// `cargo test` pass without the `--test-threads=1` workaround). Held only in the sync test
/// bodies, never across an `.await`, so it doesn't trip `clippy::await_holding_lock`.
#[cfg(test)]
pub(crate) fn gpu_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A poisoned lock just means a prior GPU test panicked; the guard is still usable and
    // the next test should still run, so recover the inner guard rather than propagate.
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

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

/// Horizontal placement of a bottom-anchored overlay layer (the info line's three
/// positions). `margin` is the inset from the chosen edge; `Center` ignores it
/// horizontally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HAlign {
    Left,
    Center,
    #[default]
    Right,
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
    /// Display one planar video frame (task 79.10 NV12; task #91 Phase 2 P010 +
    /// PQ/HLG): `y` is the `width×height` luma plane, `uv` the interleaved half-res
    /// chroma plane. The convert applies the YUV matrix + range + transfer +
    /// primaries **exactly once** in-shader — planar pixels arrive raw. This
    /// default implementation converts on CPU via [`yuv::planar_to_scene`] (the
    /// correctness/portability fallback); `WgpuRenderer` overrides it with the
    /// two-plane in-shader path.
    fn set_video_planar(
        &mut self,
        y: &[u8],
        uv: &[u8],
        width: u32,
        height: u32,
        p: PlanarPresentation,
    ) {
        let f = yuv::planar_to_scene(
            y, uv, width, height, p.format, p.yuv, p.transfer, &p.color, p.peak,
        );
        self.set_image(&f.bytes, width, height, p.color, f.hdr, f.peak);
    }

    /// Whether this renderer can display P010 (10-bit) frames on the GPU path
    /// (`TEXTURE_FORMAT_16BIT_NORM`). When false, the producer must emit an 8-bit
    /// or RGBA fallback for 10-bit sources (task #91 Phase 2, Codex Q3). The CPU
    /// default fallback handles P010 regardless, so this defaults to `true` for the
    /// trait; `WgpuRenderer` reports its real device capability.
    fn supports_p010(&self) -> bool {
        true
    }

    /// Convenience for an 8-bit NV12 SDR frame (task 79.10 call sites, e.g. the
    /// Windows MF producer) — builds a [`PlanarPresentation`] and delegates to
    /// [`set_video_planar`](Self::set_video_planar).
    fn set_video_nv12(
        &mut self,
        y: &[u8],
        uv: &[u8],
        width: u32,
        height: u32,
        yuv: YuvParams,
        color: ColorTransform,
    ) {
        // Preserve the task-79.10 behavior: an enabled color transform (BT.2020 /
        // wide-gamut SDR) uses the parametric+primaries path (shader mode 1); plain
        // BT.709 SDR is sRGB-like (mode 0).
        let transfer = if color.enabled {
            PlanarTransfer::Parametric
        } else {
            PlanarTransfer::SrgbLike
        };
        self.set_video_planar(
            y,
            uv,
            width,
            height,
            PlanarPresentation {
                format: PlanarFormat::Nv12,
                transfer,
                yuv,
                color,
                peak: 1.0,
            },
        );
    }
    /// Drop the displayed image and show a blank background (the letterbox fill)
    /// instead — the bare-launch / no-images / last-photo-deleted empty state. The
    /// next [`set_image`](Renderer::set_image) / [`present_slot`](Renderer::present_slot)
    /// restores a photo.
    fn clear_image(&mut self);
    /// Set the per-photo view transform (scaling mode + rotation + zoom + pan).
    fn set_view(&mut self, view: ViewTransform);
    /// Set or clear the corner **rich-panel** overlay (Inspector / Help): an RGBA8
    /// bitmap (`w*h*4`) drawn alpha-blended in from the bottom-right, `right_margin`
    /// px from the right edge and `bottom_margin` px from the bottom. The two insets
    /// are independent so the panel can lift above the [info-line](Renderer::set_info_line)
    /// strip when it shares the corner (task #54). `None` hides it.
    fn set_overlay(
        &mut self,
        panel: Option<(&[u8], u32, u32)>,
        right_margin: u32,
        bottom_margin: u32,
    );

    /// Set or clear the **basic info line** (`i`): the one-line filename · resolution ·
    /// format readout, its own permanent overlay layer anchored `margin` px in from the
    /// bottom edge, horizontally placed by `align` (left / center / right). Ephemeral
    /// (a CPU quad forever — it never migrates to a presenter), and independent of the
    /// rich-panel slot, so the two can coexist (task #54). `None` hides it.
    fn set_info_line(&mut self, panel: Option<(&[u8], u32, u32)>, margin: u32, align: HAlign);

    /// Allocate a resident texture ring of `capacity` slots (Phase 3). `slot_w`/
    /// `slot_h` are the intended slot size for the fixed-size variant; the v1
    /// image-sized implementation ignores them. Resets any existing ring.
    fn reserve_ring(&mut self, capacity: usize, slot_w: u32, slot_h: u32);
    /// Upload a decoded image into ring slot `slot`, baking its `color`/`hdr`/`peak`
    /// into the slot's bind group (see [`Renderer::set_image`] for the buffer layout).
    /// Runs during prefetch, off the keypress frame — so the later `present_slot`
    /// rebind carries everything for free.
    /// `mip` = build a mipmap chain for this texture (gpu-mipmap-hq-scaling). Only the full-res
    /// `Original` rep passes `true`, so trilinear fit-downscaling of it is near-Lanczos; the
    /// `Fit`/preview reps (shown ~1:1) pass `false` and stay single-level.
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
        mip: bool,
    );
    /// Select ring slot `slot` as the displayed image (the keypress fast path: a
    /// rebind, no decode or upload). Returns `true` when the slot was live and is now
    /// on screen; `false` when the slot isn't uploaded yet (the previous frame is kept),
    /// so a caller can tell "actually presented" from "held the old frame" — the core
    /// gates `mark_resolved` on this, or a door/photo would be marked on-screen while the
    /// renderer still shows the prior photo (the "card over a photo" defect).
    fn present_slot(&mut self, slot: usize) -> bool;

    /// The surface's currently-configured swapchain size (physical px). The shell compares
    /// it to the live window size to detect a stale swapchain — a size drift the renderer
    /// can't self-heal (it holds no window handle), which otherwise leaves the surface
    /// perpetually `Outdated`, dropping every frame and freezing the display until a resize.
    fn surface_size(&self) -> (u32, u32);

    /// Override the letterbox / background fill color (sRGB), shown around a photo
    /// that doesn't cover the screen. Takes effect on the next `render`. Off the
    /// photo hot path — set from user settings, not per frame.
    fn set_letterbox(&mut self, rgb: [u8; 3]);
    /// Spike (task #59): height in physical px of a translucent top bar (glass toolbar) the
    /// surface extends *under*. The photo is fit against the content region below it and its
    /// zoom/fill overflow rides up under the bar; `0` (default) keeps the classic opaque-bar
    /// layout. Off the hot path — set on resize / chrome change, not per frame.
    fn set_content_top_inset(&mut self, _px: u32) {}
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
    /// Set or clear the **folder-tree panel** (`Shift+F`): an RGBA8 bitmap drawn
    /// alpha-blended `margin` px in from the **top-left corner** — the info
    /// panel's bottom-right inset, mirrored, so the two panels frame the photo
    /// concentrically. Its own overlay layer. `None` hides it.
    fn set_tree(&mut self, panel: Option<(&[u8], u32, u32)>, margin: u32);

    /// Set or clear the **subtitle overlay** (task #90.5): the rasterized cue block from
    /// `pb_app_core::SubtitleEngine`, drawn at an absolute `x`/`y` in **physical px**
    /// (top-left origin).
    ///
    /// Unlike every other overlay here, this one is **not** an inset from an edge — a
    /// subtitle tracks the *picture*, not the window, so the core places it (see
    /// `pb_app_core::subtitle::place`) and hands the result over already resolved. `y` may
    /// be slightly negative: placement clamps the text's box on screen, not the bitmap's,
    /// so a soft shadow may bleed off the top rather than push the words down.
    ///
    /// ⚠ The bitmap is **premultiplied** RGBA8 (`pb_hud::subtitle`), not the straight alpha
    /// the other CPU overlays use — it composites through its own premultiplied-blend
    /// pipeline. Composited above the photo/video but **below** all chrome, so a toast or
    /// the info line stays readable over a cue. `None` hides it.
    fn set_subtitle_overlay(&mut self, panel: Option<(&[u8], u32, u32)>, x: f32, y: f32);

    /// The renderer's wgpu device, so the shell can build an `egui-wgpu` renderer that
    /// **shares** this device (task #54 Phase 4: the egui rich-panel overlay renders
    /// into a shell-owned offscreen texture and hands it back via
    /// [`set_egui_overlay`](Renderer::set_egui_overlay), rather than spinning up a
    /// second device like the dialog window does). Borrowed, since wgpu 22's `Device`
    /// isn't `Clone`; the shell fetches it on demand.
    fn device(&self) -> &wgpu::Device;
    /// The renderer's wgpu queue. See [`device`](Renderer::device).
    fn queue(&self) -> &wgpu::Queue;
    /// Set or clear the **egui rich-panel overlay** (Inspector / Help / folder tree on
    /// the winit shell): a full-window offscreen texture the shell rendered the panels
    /// into with `egui-wgpu`, composited premultiplied over the photo (task #54 Phase
    /// 4). The texture must be `Rgba8UnormSrgb` (premultiplied), sized to the surface.
    /// Retained — the shell re-hands it only on (re)allocation/resize and re-renders
    /// egui *into* the same texture on a panel change, so a nav frame costs nothing
    /// extra. `None` hides it. Unlike the CPU overlays this takes an already-uploaded
    /// GPU texture — no bitmap round-trip.
    fn set_egui_overlay(&mut self, texture: Option<&wgpu::Texture>);

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
