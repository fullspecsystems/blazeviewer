//! The **egui rich-panel overlay** for the winit shell (task #54 Phase 4).
//!
//! The viewer's ephemeral HUD (toasts, the `i` line, hints, scan chip) stays a CPU
//! quad, but the *rich* panels — Help, the Inspector (Details/Text/Describe), and the
//! folder tree — move to a real retained-mode UI so they can scroll, select, copy, and
//! render Markdown, matching the native SwiftUI panels on macOS. On Windows/winit there
//! is no native toolkit, so egui is that presenter (it already powers the dialog chrome
//! via [`pb_ui`]).
//!
//! Unlike the dialog window ([`crate::dialog`]) — which owns its own wgpu
//! device/surface — this overlay **shares the main renderer's device/queue** and draws
//! into an offscreen `Rgba8UnormSrgb` texture. `pb-render` composites that texture into
//! the fp16 scRGB intermediate (premultiplied, before the tone-map pass) via
//! [`Renderer::set_egui_overlay`](pb_render::Renderer::set_egui_overlay), so the panels
//! are color-correct on both the SDR and HDR output paths and cost nothing per nav frame
//! (the texture is retained; egui only re-renders on a real change).

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_wgpu::wgpu;
use winit::window::Window;

/// The offscreen texture format egui renders into. sRGB-aware, so `egui-wgpu` uses its
/// linear-framebuffer path (premultiplied **linear** output, stored sRGB); `pb-render`'s
/// egui pass then samples it through this same sRGB format — one decode, no double
/// conversion — and composites premultiplied into the linear intermediate.
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// egui state + renderer for the main window's rich-panel overlay. Holds the shared
/// context, the winit input translator, an `egui-wgpu` renderer targeting the offscreen
/// texture, and that texture. All GPU work uses the *main renderer's* device/queue,
/// passed in per call.
pub struct EguiOverlay {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
    target: wgpu::Texture,
    target_view: wgpu::TextureView,
    size: (u32, u32),
    dark: bool,
    /// When egui next wants a *timed* repaint (an animation in flight) — folded into the
    /// event loop's wake `min`, exactly like the dialog's `repaint_at`.
    next_repaint: Option<Instant>,
}

impl EguiOverlay {
    /// Build the overlay for `window`, sharing the main renderer's `device`. `w`/`h` are
    /// the surface's physical pixel size; the offscreen texture tracks it.
    pub fn new(window: &Arc<Window>, device: &wgpu::Device, dark: bool, w: u32, h: u32) -> Self {
        let ctx = egui::Context::default();
        // Lock egui to the OS-resolved theme (explicit, not `System`) so `pb_ui`'s
        // palette isn't re-clobbered by egui's per-frame light/dark resolution.
        ctx.set_theme(if dark {
            egui::ThemePreference::Dark
        } else {
            egui::ThemePreference::Light
        });
        pb_ui::install_fonts(&ctx);
        pb_ui::apply_style(&ctx, dark);
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        let renderer = egui_wgpu::Renderer::new(device, TARGET_FORMAT, None, 1, false);
        let (target, target_view) = make_target(device, w.max(1), h.max(1));
        Self {
            ctx,
            state,
            renderer,
            target,
            target_view,
            size: (w.max(1), h.max(1)),
            dark,
            next_repaint: None,
        }
    }

    /// Feed a winit event to egui. Returns the response so the caller can gate
    /// fall-through (`consumed`) and honor `repaint` — **but the caller must still let
    /// key *releases* reach the core held-key tracker even when egui consumes them, or a
    /// swallowed KeyUp on a held nav key strands a fly** (see the plan's input-routing).
    pub fn on_window_event(
        &mut self,
        window: &Window,
        event: &winit::event::WindowEvent,
    ) -> egui_winit::EventResponse {
        self.state.on_window_event(window, event)
    }

    /// The offscreen texture the panels were rendered into — handed to
    /// `Renderer::set_egui_overlay` for compositing.
    pub fn target(&self) -> &wgpu::Texture {
        &self.target
    }

    /// The pending timed-repaint deadline (an in-flight egui animation), for the wake calc.
    pub fn repaint_at(&self) -> Option<Instant> {
        self.next_repaint
    }

    /// Re-resolve the theme (OS light/dark change). Cheap; the next `run` re-applies it.
    pub fn set_theme(&mut self, dark: bool) {
        if dark != self.dark {
            self.dark = dark;
            self.ctx.set_theme(if dark {
                egui::ThemePreference::Dark
            } else {
                egui::ThemePreference::Light
            });
        }
    }

    /// Recreate the offscreen texture on a surface resize. No-op if unchanged.
    pub fn resize(&mut self, device: &wgpu::Device, w: u32, h: u32) {
        let (w, h) = (w.max(1), h.max(1));
        if (w, h) == self.size {
            return;
        }
        let (target, target_view) = make_target(device, w, h);
        self.target = target;
        self.target_view = target_view;
        self.size = (w, h);
    }

    /// Run one egui frame — `build` lays out the panels into the context — and render the
    /// result into the offscreen texture (cleared transparent, so only the panels show).
    /// Updates the timed-repaint deadline. The caller then composites [`target`](Self::target).
    pub fn run(
        &mut self,
        window: &Window,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        build: impl FnMut(&egui::Context),
    ) {
        pb_ui::apply_style(&self.ctx, self.dark);
        let raw_input = self.state.take_egui_input(window);
        let full_output = self.ctx.run(raw_input, build);
        self.state
            .handle_platform_output(window, full_output.platform_output);

        let repaint_delay = full_output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay)
            .unwrap_or(Duration::MAX);
        self.next_repaint = if repaint_delay.is_zero() {
            Some(Instant::now())
        } else if repaint_delay < Duration::MAX {
            Some(Instant::now() + repaint_delay)
        } else {
            None
        };

        let paint_jobs = self
            .ctx
            .tessellate(full_output.shapes, full_output.pixels_per_point);
        let desc = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.size.0, self.size.1],
            pixels_per_point: full_output.pixels_per_point,
        };
        for (id, delta) in &full_output.textures_delta.set {
            self.renderer.update_texture(device, queue, *id, delta);
        }
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("egui-overlay"),
        });
        let cmd_bufs =
            self.renderer
                .update_buffers(device, queue, &mut encoder, &paint_jobs, &desc);
        {
            let mut rpass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui-overlay-to-texture"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.target_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            // Transparent so only the panels composite over the photo.
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .forget_lifetime();
            self.renderer.render(&mut rpass, &paint_jobs, &desc);
        }
        queue.submit(
            cmd_bufs
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        for id in &full_output.textures_delta.free {
            self.renderer.free_texture(id);
        }
    }
}

/// Create the offscreen render target (+ a default view) egui paints into.
fn make_target(device: &wgpu::Device, w: u32, h: u32) -> (wgpu::Texture, wgpu::TextureView) {
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("egui-overlay-target"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TARGET_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    (target, view)
}
