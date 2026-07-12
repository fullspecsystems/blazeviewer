//! Analytic (signed-distance-field) rounded rectangles for the egui chrome.
//!
//! egui tessellates rounded rects on the CPU and anti-aliases them with a ~1px
//! feathering pass, which reads soft/rough on HiDPI — a fractionally-positioned pill
//! feathers its corner arcs unevenly. This draws the same shape from a fragment shader
//! instead: a rounded-box SDF with `fwidth`-based analytic AA, so corners are crisp and
//! resolution-independent, matching the macOS SwiftUI `.continuous` look.
//!
//! It runs as an `egui_wgpu` paint callback, so it composites in the same offscreen pass
//! and z-order as the rest of the panel (text/icons draw on top). Off the photo hot path:
//! the overlay re-renders only when a panel changes, and the shader is a few ops over a
//! small quad.

use std::sync::OnceLock;

use egui::epaint::PaintCallbackInfo;
use egui::{Color32, Rect};
use egui_wgpu::wgpu;
use egui_wgpu::{Callback, CallbackResources, CallbackTrait, ScreenDescriptor};

/// The offscreen overlay texture's format — must match `egui_overlay::TARGET_FORMAT`.
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Add a crisp rounded-rect (fill + optional border) at `rect` to `ui`'s paint list,
/// drawn analytically in a shader instead of via egui's tessellator. `radius`/`border_w`
/// are egui points; `fill`/`border` are straight `Color32` (converted to linear-
/// premultiplied for the shader). A zero-alpha `border` (or `border_w == 0`) is borderless.
pub fn round_rect(
    ui: &egui::Ui,
    rect: Rect,
    radius: f32,
    fill: Color32,
    border_w: f32,
    border: Color32,
) {
    // Honor the Ui's opacity factor (`Ui::set_opacity` — the info-line fade):
    // paint callbacks bypass egui's shape-color multiplication, so without this
    // the SDF backgrounds stayed opaque while the text faded (owner-reported).
    let o = ui.opacity();
    let (fill, border) = if o < 1.0 {
        (fill.gamma_multiply(o), border.gamma_multiply(o))
    } else {
        (fill, border)
    };
    ui.painter()
        .add(round_rect_shape(rect, radius, fill, border_w, border));
}

/// Like [`round_rect`] but returns the `egui::Shape` instead of adding it, so a caller can
/// reserve a slot with `Painter::add(Shape::Noop)` and backfill it with `Painter::set` once
/// the rect is known (the pattern for drawing a panel background *behind* its content).
pub fn round_rect_shape(
    rect: Rect,
    radius: f32,
    fill: Color32,
    border_w: f32,
    border: Color32,
) -> egui::Shape {
    let cb = RoundRect {
        rect,
        radius,
        border_w,
        fill: egui::Rgba::from(fill).to_array(),
        border: egui::Rgba::from(border).to_array(),
        slot: OnceLock::new(),
    };
    // The callback rect (→ the GPU viewport) is the shape grown a touch so the border's
    // outer anti-aliasing isn't clipped right at the edge.
    egui::Shape::Callback(Callback::new_paint_callback(rect.expand(2.0), cb))
}

/// One rounded-rect draw. Geometry is in egui points; [`prepare`](CallbackTrait::prepare)
/// scales to physical pixels with the frame's `pixels_per_point`. Colors are already
/// linear-premultiplied rgba.
struct RoundRect {
    rect: Rect,
    radius: f32,
    border_w: f32,
    fill: [f32; 4],
    border: [f32; 4],
    /// This rect's own uniform buffer + bind group, built in `prepare` and read in
    /// `paint`. A fresh `RoundRect` is created per overlay frame, and `prepare` runs
    /// once before any `paint`, so this is written exactly once per instance — hence a
    /// `OnceLock` (Sync, no locking on the read). Per-instance so many rects can be
    /// drawn in one frame (the shared pipeline lives in [`SdfResources`]).
    slot: OnceLock<(wgpu::Buffer, wgpu::BindGroup)>,
}

/// The shared pipeline + bind-group layout, stashed in the egui renderer's
/// `CallbackResources` so they're built once and reused by every rect.
struct SdfResources {
    pipeline: wgpu::RenderPipeline,
    bgl: wgpu::BindGroupLayout,
}

impl CallbackTrait for RoundRect {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen: &ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if resources.get::<SdfResources>().is_none() {
            resources.insert(SdfResources::new(device));
        }
        let res = resources.get::<SdfResources>().unwrap();
        let ppp = screen.pixels_per_point;
        // Uniform layout mirrors `Uni` in the shader (64 bytes, std140-friendly).
        let d: [f32; 16] = [
            self.rect.min.x * ppp,
            self.rect.min.y * ppp,
            self.rect.width() * ppp,
            self.rect.height() * ppp,
            self.radius * ppp,
            self.border_w * ppp,
            0.0,
            0.0,
            self.fill[0],
            self.fill[1],
            self.fill[2],
            self.fill[3],
            self.border[0],
            self.border[1],
            self.border[2],
            self.border[3],
        ];
        let mut bytes = Vec::with_capacity(64);
        for f in d {
            bytes.extend_from_slice(&f.to_ne_bytes());
        }
        // A per-rect uniform buffer + bind group (a handful per overlay frame, off the hot
        // path) so multiple rects coexist in one frame — a single shared buffer would clobber
        // since every `prepare` runs before any `paint`.
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pb-sdf-rect-uniform"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&buf, 0, &bytes);
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pb-sdf-rect-bg"),
            layout: &res.bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: buf.as_entire_binding(),
            }],
        });
        let _ = self.slot.set((buf, bind_group));
        Vec::new()
    }

    fn paint(
        &self,
        _info: PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &CallbackResources,
    ) {
        let Some(res) = resources.get::<SdfResources>() else {
            return;
        };
        let Some((_, bind_group)) = self.slot.get() else {
            return;
        };
        pass.set_pipeline(&res.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

impl SdfResources {
    fn new(device: &wgpu::Device) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pb-sdf-rect"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pb-sdf-rect-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pb-sdf-rect-pl"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pb-sdf-rect-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs",
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs",
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: TARGET_FORMAT,
                    // Match egui-wgpu's premultiplied-over blend so the SDF shape accumulates
                    // into the offscreen texture exactly like egui's own shapes do.
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::OneMinusDstAlpha,
                            dst_factor: wgpu::BlendFactor::One,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        Self { pipeline, bgl }
    }
}

/// Rounded-box SDF with analytic (`fwidth`) AA. Fragment coords + the uniform rect are in
/// physical pixels; colors are linear-premultiplied so scaling by coverage stays valid.
const SHADER: &str = r#"
struct Uni {
    rect_min: vec2<f32>,
    rect_size: vec2<f32>,
    radius: f32,
    border_w: f32,
    pad0: vec2<f32>,
    fill: vec4<f32>,
    border: vec4<f32>,
};
@group(0) @binding(0) var<uniform> u: Uni;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    // Fullscreen triangle; egui-wgpu sets the viewport to this callback's rect.
    let x = f32((i << 1u) & 2u) * 2.0 - 1.0;
    let y = f32(i & 2u) * 2.0 - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

fn sd_round_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r, r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0, 0.0))) - r;
}

@fragment
fn fs(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let center = u.rect_min + u.rect_size * 0.5;
    let d = sd_round_box(frag.xy - center, u.rect_size * 0.5, u.radius);
    let g = max(fwidth(d), 1e-4);
    let fill_cov = clamp(0.5 - d / g, 0.0, 1.0);
    let inner = clamp(0.5 - (d + u.border_w) / g, 0.0, 1.0);
    let border_cov = clamp(fill_cov - inner, 0.0, 1.0);
    // Premultiplied inputs: scaling by coverage stays premultiplied.
    let fe = u.fill * fill_cov;
    let be = u.border * border_cov;
    // Border over fill (premultiplied "over").
    return be + fe * (1.0 - be.a);
}
"#;
