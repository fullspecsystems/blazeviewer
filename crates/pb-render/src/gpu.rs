//! wgpu presenter (DX12 backend on Windows) + a headless render for golden tests.
//!
//! One image is drawn as a textured quad, letterboxed to the screen via
//! `fit_rect` (no crop), over a dark clear color. `WgpuRenderer` presents to a
//! window surface; `render_offscreen` renders to a buffer for tests.

use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::upload::{StagingUpload, UploadStrategy};
use crate::{ColorTransform, RenderError, Renderer, ViewTransform};

/// Background (letterbox) color, straight RGBA8.
pub const LETTERBOX: [u8; 4] = [10, 10, 12, 255];

/// The HDR-capable intermediate the scene renders into: linear light, BT.709
/// primaries, extended range (scRGB). Wide-gamut / HDR values survive here as
/// numbers outside [0,1]; the tone-map pass (SDR) or native scRGB swapchain (HDR,
/// later) turns it into final pixels.
const INTERMEDIATE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Scene pass: photo texel → scene-linear scRGB, written to the fp16 intermediate.
/// `mode` (cx.p1.w): 0 = sRGB-encoded (linearize sRGB, identity primaries),
/// 1 = profiled (linearize the TRC, apply the source→BT.709 matrix), 2 = already
/// scene-linear (HDR fp16) → passthrough. No clamping — wide-gamut colors keep
/// their out-of-[0,1] values for the HDR path.
const SCENE_WGSL: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@location(0) position: vec2<f32>, @location(1) uv: vec2<f32>) -> VsOut {
    var o: VsOut;
    o.pos = vec4<f32>(position, 0.0, 1.0);
    o.uv = uv;
    return o;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct ColorXf {
    r0: vec4<f32>,
    r1: vec4<f32>,
    r2: vec4<f32>,
    p0: vec4<f32>,
    p1: vec4<f32>,
    scale: vec4<f32>,   // .x = output scale (SDR→HDR-surface white level, else 1.0)
};
@group(0) @binding(2) var<uniform> cx: ColorXf;

// Source EOTF (encoded -> linear), evaluated exactly as moxcms::ParametricCurve.
fn eotf(x: f32) -> f32 {
    if (x < cx.p1.x) {
        return cx.p0.w * x + cx.p1.z;                          // c*x + f
    }
    return pow(cx.p0.y * x + cx.p0.z, cx.p0.x) + cx.p1.y;      // (a*x + b)^g + e
}

fn srgb_to_linear(x: f32) -> f32 {
    if (x <= 0.04045) {
        return x / 12.92;
    }
    return pow((x + 0.055) / 1.055, 2.4);
}

@fragment
fn fs_scene(in: VsOut) -> @location(0) vec4<f32> {
    let s = textureSample(tex, samp, in.uv);
    var lin: vec3<f32>;
    if (cx.p1.w > 1.5) {
        lin = s.rgb;                                           // already scene-linear
    } else if (cx.p1.w > 0.5) {
        let e = vec3<f32>(eotf(s.r), eotf(s.g), eotf(s.b));
        lin = vec3<f32>(dot(cx.r0.xyz, e), dot(cx.r1.xyz, e), dot(cx.r2.xyz, e));
    } else {
        lin = vec3<f32>(srgb_to_linear(s.r), srgb_to_linear(s.g), srgb_to_linear(s.b));
    }
    // cx.r3.w carries the output scale: SDR content on an HDR surface is scaled to
    // the SDR white level; HDR content (and the SDR-display path) uses 1.0.
    return vec4<f32>(lin * cx.scale.x, s.a);
}
"#;

/// Present pass: the scene-linear intermediate → the surface. A fullscreen triangle
/// samples the intermediate, then branches on the surface type (`params.z`):
///   - **SDR 8-bit surface** (0): per-channel extended Reinhard with the image's
///     peak as the white point (peak = 1 ⇒ identity, faithful SDR), then sRGB-encode.
///   - **HDR fp16 scRGB surface** (1): output scene-linear scaled to the SDR white
///     level (`params.y`). Negatives (wide gamut) and values > 1 (HDR) are kept —
///     scRGB carries them to the panel.
const PRESENT_WGSL: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> VsOut {
    var o: VsOut;
    let uv = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    o.uv = uv;
    o.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct Present { params: vec4<f32> };   // x = peak (SDR white pt), y = EDR roll-off headroom (0 = straight), z = hdr flag
@group(0) @binding(2) var<uniform> pr: Present;

fn srgb_oetf(c: f32) -> f32 {
    let x = clamp(c, 0.0, 1.0);
    if (x <= 0.0031308) {
        return 12.92 * x;
    }
    return 1.055 * pow(x, 1.0 / 2.4) - 0.055;
}

fn reinhard(v: f32, lw: f32) -> f32 {
    let x = max(v, 0.0);
    return x * (1.0 + x / (lw * lw)) / (1.0 + x);
}

// Highlight roll-off for the macOS EDR surface: identity at/below SDR white (1.0),
// smoothly compressing values above 1 so they asymptote to `headroom` rather than
// hard-clipping at the panel's EDR limit. SDR/diffuse and wide-gamut (negative)
// values pass through unchanged. `headroom` <= 1 clamps highlights to SDR white.
fn rolloff(v: f32, headroom: f32) -> f32 {
    if (v <= 1.0) { return v; }
    let t = v - 1.0;
    let m = max(headroom - 1.0, 0.0);
    return 1.0 + m * t / (t + m + 1e-6);
}

@fragment
fn fs_present(in: VsOut) -> @location(0) vec4<f32> {
    let s = textureSample(tex, samp, in.uv);
    if (pr.params.z > 0.5) {
        // HDR/wide-gamut scRGB surface: the intermediate is already final scene-linear
        // scRGB. On Windows (headroom 0) the DWM compositor tone-maps, so pass straight
        // through. On macOS, EDR hard-clips above the panel headroom, so roll highlights
        // off toward it — keeping SDR/diffuse and wide-gamut (negative) values intact.
        let hr = pr.params.y;
        if (hr > 0.0) {
            return vec4<f32>(rolloff(s.r, hr), rolloff(s.g, hr), rolloff(s.b, hr), 1.0);
        }
        return vec4<f32>(s.rgb, 1.0);
    }
    let lw = pr.params.x;
    let o = vec3<f32>(
        srgb_oetf(reinhard(s.r, lw)),
        srgb_oetf(reinhard(s.g, lw)),
        srgb_oetf(reinhard(s.b, lw)),
    );
    return vec4<f32>(o, 1.0);
}
"#;

/// Overlay pass: an sRGB UI bitmap (info panel / help) composited into the
/// scene-linear intermediate (before present), so it works for both the SDR and
/// HDR output paths uniformly. The bitmap is sRGB-encoded, so it is linearized
/// here; the present pass re-encodes (SDR) or scales it to SDR white (HDR). Uses
/// the scene bind-group layout (the color uniform at binding 2 is unused here).
const OVERLAY_WGSL: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@location(0) position: vec2<f32>, @location(1) uv: vec2<f32>) -> VsOut {
    var o: VsOut;
    o.pos = vec4<f32>(position, 0.0, 1.0);
    o.uv = uv;
    return o;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct ColorXf {
    r0: vec4<f32>, r1: vec4<f32>, r2: vec4<f32>,
    p0: vec4<f32>, p1: vec4<f32>, scale: vec4<f32>,
};
@group(0) @binding(2) var<uniform> cx: ColorXf;

fn srgb_to_linear(x: f32) -> f32 {
    if (x <= 0.04045) {
        return x / 12.92;
    }
    return pow((x + 0.055) / 1.055, 2.4);
}

@fragment
fn fs_overlay(in: VsOut) -> @location(0) vec4<f32> {
    let s = textureSample(tex, samp, in.uv);
    let lin = vec3<f32>(srgb_to_linear(s.r), srgb_to_linear(s.g), srgb_to_linear(s.b));
    // Same output scale as the scene, so UI sits at the SDR white level on an HDR
    // surface (and is unchanged on the SDR path).
    return vec4<f32>(lin * cx.scale.x, s.a);
}
"#;

/// egui-overlay pass: the shell renders the rich panels (Inspector / Help / folder
/// tree) into an offscreen **`Rgba8UnormSrgb`** texture with `egui-wgpu`, which
/// composites them in linear light with *premultiplied* alpha and stores sRGB. We
/// sample that texture through an sRGB view (so the sampler decodes back to
/// premultiplied **linear**) with a bufferless fullscreen triangle, lift it to the
/// SDR-white level (`cx.scale.x`, matching the CPU overlays on an HDR surface), and
/// blend it into the fp16 intermediate with a *premultiplied* blend state — so no
/// double sRGB conversion happens and the panels sit at the right brightness on both
/// SDR and HDR output paths. Reuses the scene bind-group layout (only `scale.x` of
/// the color uniform is read).
const EGUI_WGSL: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_egui(@builtin(vertex_index) vi: u32) -> VsOut {
    var o: VsOut;
    let uv = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    o.uv = uv;
    o.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct ColorXf {
    r0: vec4<f32>, r1: vec4<f32>, r2: vec4<f32>,
    p0: vec4<f32>, p1: vec4<f32>, scale: vec4<f32>,
};
@group(0) @binding(2) var<uniform> cx: ColorXf;

@fragment
fn fs_egui(in: VsOut) -> @location(0) vec4<f32> {
    // The sRGB view decodes the store back to premultiplied linear; keep the alpha
    // straight for the premultiplied-over blend. Scale only the (premultiplied) RGB.
    let s = textureSample(tex, samp, in.uv);
    return vec4<f32>(s.rgb * cx.scale.x, s.a);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
}

const ATTRS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2];

const INDICES: [u16; 6] = [0, 1, 2, 0, 2, 3];

/// GPU-side mirror of [`ColorTransform`], laid out as six `vec4`s to match the
/// shader's `ColorXf` uniform (each field 16-byte aligned). `r0..r2` are the
/// matrix rows; `p0 = (g,a,b,c)`, `p1 = (d,e,f,mode)`, `scale.x` = output scale.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ColorUniform {
    r0: [f32; 4],
    r1: [f32; 4],
    r2: [f32; 4],
    p0: [f32; 4],
    p1: [f32; 4],
    scale: [f32; 4],
}

impl ColorUniform {
    /// `mode`: 0 = sRGB-encoded, 1 = convert (matrix+TRC), 2 = scene-linear (HDR).
    /// `scale`: output multiplier (SDR content → HDR-surface white level, else 1.0).
    fn new(c: &ColorTransform, mode: f32, scale: f32) -> Self {
        let m = &c.matrix;
        let t = &c.trc;
        Self {
            r0: [m[0][0], m[0][1], m[0][2], 0.0],
            r1: [m[1][0], m[1][1], m[1][2], 0.0],
            r2: [m[2][0], m[2][1], m[2][2], 0.0],
            p0: [t[0], t[1], t[2], t[3]],
            p1: [t[4], t[5], t[6], mode],
            scale: [scale, 0.0, 0.0, 0.0],
        }
    }
}

/// Present-pass uniform (one `vec4` for std140 alignment): `x` = SDR tone-map white
/// point (image peak), `y` = HDR scRGB scale (SDR-white in 80-nit units), `z` = HDR
/// output flag (1.0 = fp16 scRGB surface, 0.0 = SDR 8-bit).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PresentUniform {
    params: [f32; 4],
}

impl PresentUniform {
    /// `peak` = SDR tone-map white point (the displayed image's peak; 1.0 for SDR).
    /// `hdr` = true on an fp16 scRGB surface. `edr_headroom` = the macOS EDR roll-off
    /// target (>1 rolls highlights off toward it; 0 = pass straight through, the
    /// Windows/DWM path) — see `display::DisplayHdr::edr_headroom`.
    fn new(peak: f32, hdr: bool, edr_headroom: f32) -> Self {
        Self {
            params: [
                peak.max(1.0),
                edr_headroom,
                if hdr { 1.0 } else { 0.0 },
                0.0,
            ],
        }
    }
}

/// The four corners of the image quad in clip space, placed and UV-mapped by the
/// per-photo `ViewTransform` (scaling mode + rotation + zoom + pan).
fn quad_vertices(
    view: &ViewTransform,
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
    top_inset: u32,
) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    // Fit/center against the content region *below* a translucent top bar, then slide the
    // whole placement down by the inset. The surface is still the full `screen_h`, so a
    // zoomed/cropped photo's overflow rides up under the bar (task #59 spike). `top_inset == 0`
    // reduces to the classic full-surface fit.
    let content_h = screen_h.saturating_sub(top_inset).max(1);
    let mut p = view.placement(img_w, img_h, screen_w, content_h);
    p.y += top_inset as f32;
    let x0 = (p.x / sw) * 2.0 - 1.0;
    let x1 = ((p.x + p.w) / sw) * 2.0 - 1.0;
    let y_top = 1.0 - (p.y / sh) * 2.0;
    let y_bot = 1.0 - ((p.y + p.h) / sh) * 2.0;
    [
        Vertex {
            pos: [x0, y_top],
            uv: p.uvs[0],
        },
        Vertex {
            pos: [x1, y_top],
            uv: p.uvs[1],
        },
        Vertex {
            pos: [x1, y_bot],
            uv: p.uvs[2],
        },
        Vertex {
            pos: [x0, y_bot],
            uv: p.uvs[3],
        },
    ]
}

/// A bind-group layout entry: a filterable float texture, a filtering sampler, and
/// a fragment uniform buffer — the shape both the image (color uniform) and
/// tone-map (peak uniform) passes use.
fn tex_sampler_uniform_bgl(device: &wgpu::Device, label: &str) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    })
}

/// The render pipelines and their bind-group layouts.
struct Pipelines {
    /// Scene: photo quad → fp16 scRGB-linear intermediate (alpha-blended over the
    /// letterbox so transparent images composite cleanly).
    scene: wgpu::RenderPipeline,
    /// Tone-map: fullscreen intermediate → SDR `surface_format`.
    tonemap: wgpu::RenderPipeline,
    /// Overlay: sRGB UI bitmap → surface, alpha-blended on top.
    overlay: wgpu::RenderPipeline,
    /// egui rich-panel overlay: an offscreen premultiplied-sRGB texture → the fp16
    /// intermediate, premultiplied-blended (bufferless fullscreen).
    egui: wgpu::RenderPipeline,
    /// Layout for the image (and overlay) bind groups: tex + sampler + color uniform.
    scene_bgl: wgpu::BindGroupLayout,
    /// Layout for the tone-map bind group: intermediate tex + sampler + peak uniform.
    tonemap_bgl: wgpu::BindGroupLayout,
}

fn build_pipelines(device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> Pipelines {
    let scene_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("pb-scene"),
        source: wgpu::ShaderSource::Wgsl(SCENE_WGSL.into()),
    });
    let tonemap_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("pb-present"),
        source: wgpu::ShaderSource::Wgsl(PRESENT_WGSL.into()),
    });
    let overlay_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("pb-overlay"),
        source: wgpu::ShaderSource::Wgsl(OVERLAY_WGSL.into()),
    });
    let egui_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("pb-egui"),
        source: wgpu::ShaderSource::Wgsl(EGUI_WGSL.into()),
    });

    let scene_bgl = tex_sampler_uniform_bgl(device, "img-bgl");
    let tonemap_bgl = tex_sampler_uniform_bgl(device, "tonemap-bgl");

    let scene_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("pb-scene-layout"),
        bind_group_layouts: &[&scene_bgl],
        push_constant_ranges: &[],
    });
    let tonemap_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("pb-tonemap-layout"),
        bind_group_layouts: &[&tonemap_bgl],
        push_constant_ranges: &[],
    });

    let quad_buffers = [wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &ATTRS,
    }];

    // Scene → fp16 intermediate.
    let scene = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("pb-scene-pipeline"),
        layout: Some(&scene_layout),
        vertex: wgpu::VertexState {
            module: &scene_mod,
            entry_point: "vs_main",
            compilation_options: Default::default(),
            buffers: &quad_buffers,
        },
        fragment: Some(wgpu::FragmentState {
            module: &scene_mod,
            entry_point: "fs_scene",
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: INTERMEDIATE_FORMAT,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });

    // Fullscreen present → surface (no vertex buffer; overwrites the whole frame).
    let tonemap = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("pb-present-pipeline"),
        layout: Some(&tonemap_layout),
        vertex: wgpu::VertexState {
            module: &tonemap_mod,
            entry_point: "vs_fullscreen",
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &tonemap_mod,
            entry_point: "fs_present",
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: surface_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });

    // Overlay → the fp16 intermediate (alpha-blended in linear), so one present
    // pass serves both SDR and HDR. Reuses the scene bind-group layout.
    let overlay = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("pb-overlay-pipeline"),
        layout: Some(&scene_layout),
        vertex: wgpu::VertexState {
            module: &overlay_mod,
            entry_point: "vs_main",
            compilation_options: Default::default(),
            buffers: &quad_buffers,
        },
        fragment: Some(wgpu::FragmentState {
            module: &overlay_mod,
            entry_point: "fs_overlay",
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: INTERMEDIATE_FORMAT,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });

    // egui rich panels → the fp16 intermediate. Bufferless fullscreen triangle; the
    // egui texture is premultiplied (linear after the sRGB-view decode), so the blend
    // is premultiplied-over (src One), not the straight `ALPHA_BLENDING` the CPU
    // overlays use. Reuses the scene bind-group layout.
    let egui = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("pb-egui-pipeline"),
        layout: Some(&scene_layout),
        vertex: wgpu::VertexState {
            module: &egui_mod,
            entry_point: "vs_egui",
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &egui_mod,
            entry_point: "fs_egui",
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: INTERMEDIATE_FORMAT,
                blend: Some(wgpu::BlendState {
                    color: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                    alpha: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
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

    Pipelines {
        scene,
        tonemap,
        overlay,
        egui,
        scene_bgl,
        tonemap_bgl,
    }
}

/// sRGB EOTF (encoded → linear) for one channel — to express the letterbox color
/// in the intermediate's linear space.
fn srgb_to_linear(u: u8) -> f64 {
    let x = u as f64 / 255.0;
    if x <= 0.04045 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

/// The letterbox clear color (sRGB `rgb`) in the intermediate's linear space (so it
/// round-trips back to `rgb` through the tone-map pass's sRGB encode). The background
/// is always opaque (alpha 1).
fn letterbox_linear(rgb: [u8; 3]) -> wgpu::Color {
    wgpu::Color {
        r: srgb_to_linear(rgb[0]),
        g: srgb_to_linear(rgb[1]),
        b: srgb_to_linear(rgb[2]),
        a: 1.0,
    }
}

/// Create the fp16 scRGB-linear intermediate render target sized to the surface,
/// plus the tone-map bind group that samples it. Rebuilt on resize.
fn make_intermediate(
    device: &wgpu::Device,
    tonemap_bgl: &wgpu::BindGroupLayout,
    peak_buf: &wgpu::Buffer,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::BindGroup) {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("scene-intermediate"),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: INTERMEDIATE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("tonemap-bg"),
        layout: tonemap_bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: peak_buf.as_entire_binding(),
            },
        ],
    });
    (tex, bind_group)
}

/// The four corners of an overlay panel quad: `panel_w`×`panel_h` pixels, placed
/// `margin` px in from the bottom-right of the screen (equal right + bottom insets).
fn overlay_quad_vertices(
    panel_w: u32,
    panel_h: u32,
    screen_w: u32,
    screen_h: u32,
    margin: u32,
) -> [Vertex; 4] {
    bottom_right_quad_xy(panel_w, panel_h, screen_w, screen_h, margin, margin)
}

/// Bottom-right anchored quad with **independent** right + bottom insets — lets the
/// rich panel lift above the info-line strip (`bottom_margin`) while keeping its right
/// edge fixed (`right_margin`), the mirror of [`top_right_quad_xy`].
fn bottom_right_quad_xy(
    panel_w: u32,
    panel_h: u32,
    screen_w: u32,
    screen_h: u32,
    right_margin: u32,
    bottom_margin: u32,
) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    let x1 = sw - right_margin as f32;
    let x0 = x1 - panel_w as f32;
    let y1 = sh - bottom_margin as f32;
    let y0 = y1 - panel_h as f32;
    let x0n = (x0 / sw) * 2.0 - 1.0;
    let x1n = (x1 / sw) * 2.0 - 1.0;
    let y_top = 1.0 - (y0 / sh) * 2.0;
    let y_bot = 1.0 - (y1 / sh) * 2.0;
    [
        Vertex {
            pos: [x0n, y_top],
            uv: [0.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_top],
            uv: [1.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_bot],
            uv: [1.0, 1.0],
        },
        Vertex {
            pos: [x0n, y_bot],
            uv: [0.0, 1.0],
        },
    ]
}

/// The four corners of a bottom-anchored quad placed `margin` px up from the bottom,
/// horizontally left / center / right by `align` — the info line's three positions.
fn bottom_aligned_quad(
    panel_w: u32,
    panel_h: u32,
    screen_w: u32,
    screen_h: u32,
    margin: u32,
    align: crate::HAlign,
) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    let pw = panel_w as f32;
    let x0 = match align {
        crate::HAlign::Left => margin as f32,
        crate::HAlign::Center => ((sw - pw) * 0.5).max(0.0),
        crate::HAlign::Right => sw - margin as f32 - pw,
    };
    let x1 = x0 + pw;
    let y1 = sh - margin as f32;
    let y0 = y1 - panel_h as f32;
    let x0n = (x0 / sw) * 2.0 - 1.0;
    let x1n = (x1 / sw) * 2.0 - 1.0;
    let y_top = 1.0 - (y0 / sh) * 2.0;
    let y_bot = 1.0 - (y1 / sh) * 2.0;
    [
        Vertex {
            pos: [x0n, y_top],
            uv: [0.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_top],
            uv: [1.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_bot],
            uv: [1.0, 1.0],
        },
        Vertex {
            pos: [x0n, y_bot],
            uv: [0.0, 1.0],
        },
    ]
}

/// The four corners of the bottom-center toast quad: `panel_w`×`panel_h` px,
/// horizontally centered and `bottom_margin` px up from the bottom edge.
fn toast_quad_vertices(
    panel_w: u32,
    panel_h: u32,
    screen_w: u32,
    screen_h: u32,
    bottom_margin: u32,
) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    let x0 = ((sw - panel_w as f32) * 0.5).max(0.0);
    let x1 = x0 + panel_w as f32;
    let y1 = sh - bottom_margin as f32;
    let y0 = y1 - panel_h as f32;
    let x0n = (x0 / sw) * 2.0 - 1.0;
    let x1n = (x1 / sw) * 2.0 - 1.0;
    let y_top = 1.0 - (y0 / sh) * 2.0;
    let y_bot = 1.0 - (y1 / sh) * 2.0;
    [
        Vertex {
            pos: [x0n, y_top],
            uv: [0.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_top],
            uv: [1.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_bot],
            uv: [1.0, 1.0],
        },
        Vertex {
            pos: [x0n, y_bot],
            uv: [0.0, 1.0],
        },
    ]
}

/// The four corners of a panel quad centered on both axes of the screen — the
/// empty-state message placement.
fn center_quad_vertices(panel_w: u32, panel_h: u32, screen_w: u32, screen_h: u32) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    let x0 = ((sw - panel_w as f32) * 0.5).max(0.0);
    let y0 = ((sh - panel_h as f32) * 0.5).max(0.0);
    let x1 = x0 + panel_w as f32;
    let y1 = y0 + panel_h as f32;
    let x0n = (x0 / sw) * 2.0 - 1.0;
    let x1n = (x1 / sw) * 2.0 - 1.0;
    let y_top = 1.0 - (y0 / sh) * 2.0;
    let y_bot = 1.0 - (y1 / sh) * 2.0;
    [
        Vertex {
            pos: [x0n, y_top],
            uv: [0.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_top],
            uv: [1.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_bot],
            uv: [1.0, 1.0],
        },
        Vertex {
            pos: [x0n, y_bot],
            uv: [0.0, 1.0],
        },
    ]
}

/// The four corners of the top-left tree quad: `panel_w`×`panel_h` px, placed
/// `margin` px in from the top and left edges — the folder-tree panel's corner,
/// mirroring the info panel's bottom-right inset.
fn top_left_quad_vertices(
    panel_w: u32,
    panel_h: u32,
    screen_w: u32,
    screen_h: u32,
    margin: u32,
) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    let x0 = margin as f32;
    let x1 = x0 + panel_w as f32;
    let y0 = margin as f32;
    let y1 = y0 + panel_h as f32;
    let x0n = (x0 / sw) * 2.0 - 1.0;
    let x1n = (x1 / sw) * 2.0 - 1.0;
    let y_top = 1.0 - (y0 / sh) * 2.0;
    let y_bot = 1.0 - (y1 / sh) * 2.0;
    [
        Vertex {
            pos: [x0n, y_top],
            uv: [0.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_top],
            uv: [1.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_bot],
            uv: [1.0, 1.0],
        },
        Vertex {
            pos: [x0n, y_bot],
            uv: [0.0, 1.0],
        },
    ]
}

/// The four corners of the top-right pie quad: `panel_w`×`panel_h` px, placed
/// `margin` px in from the top and right edges (the "loading" affordance corner).
fn top_right_quad_vertices(
    panel_w: u32,
    panel_h: u32,
    screen_w: u32,
    screen_h: u32,
    margin: u32,
) -> [Vertex; 4] {
    // Same inset on both axes (the pie).
    top_right_quad_xy(panel_w, panel_h, screen_w, screen_h, margin, margin)
}

/// Top-right anchored quad with **independent** right + top insets — lets the scan-count
/// chip align its right edge with the pie (`right_margin`) while sitting below it
/// (`top_margin`).
fn top_right_quad_xy(
    panel_w: u32,
    panel_h: u32,
    screen_w: u32,
    screen_h: u32,
    right_margin: u32,
    top_margin: u32,
) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    let x1 = sw - right_margin as f32;
    let x0 = x1 - panel_w as f32;
    let y0 = top_margin as f32;
    let y1 = y0 + panel_h as f32;
    let x0n = (x0 / sw) * 2.0 - 1.0;
    let x1n = (x1 / sw) * 2.0 - 1.0;
    let y_top = 1.0 - (y0 / sh) * 2.0;
    let y_bot = 1.0 - (y1 / sh) * 2.0;
    [
        Vertex {
            pos: [x0n, y_top],
            uv: [0.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_top],
            uv: [1.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_bot],
            uv: [1.0, 1.0],
        },
        Vertex {
            pos: [x0n, y_bot],
            uv: [0.0, 1.0],
        },
    ]
}

/// Downscale `image` (RGBA8) so neither dimension exceeds `max`, preserving
/// aspect with nearest-neighbor sampling. Returns the input borrowed unchanged
/// when it already fits.
fn clamp_to_max(image: &[u8], w: u32, h: u32, max: u32) -> (Cow<'_, [u8]>, u32, u32) {
    if w <= max && h <= max {
        return (Cow::Borrowed(image), w, h);
    }
    let scale = max as f64 / w.max(h) as f64;
    let tw = ((w as f64 * scale) as u32).clamp(1, max);
    let th = ((h as f64 * scale) as u32).clamp(1, max);
    let mut out = vec![0u8; (tw as usize) * (th as usize) * 4];
    for y in 0..th {
        let sy = ((y as u64 * h as u64) / th as u64) as u32;
        for x in 0..tw {
            let sx = ((x as u64 * w as u64) / tw as u64) as u32;
            let s = ((sy * w + sx) * 4) as usize;
            let d = ((y * tw + x) * 4) as usize;
            out[d..d + 4].copy_from_slice(&image[s..s + 4]);
        }
    }
    (Cow::Owned(out), tw, th)
}

#[allow(clippy::too_many_arguments)]
fn upload_image(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    bgl: &wgpu::BindGroupLayout,
    uploader: &mut dyn UploadStrategy,
    image: &[u8],
    img_w: u32,
    img_h: u32,
    color: &ColorTransform,
    hdr: bool,
    scale: f32,
) -> wgpu::BindGroup {
    // HDR images are scene-linear fp16 (Rgba16Float, mode 2); everything else is
    // sRGB/source-encoded RGBA8 (mode 1 if a profile applies, else 0).
    let (tex_format, mode) = if hdr {
        (wgpu::TextureFormat::Rgba16Float, 2.0)
    } else {
        (
            wgpu::TextureFormat::Rgba8Unorm,
            if color.enabled { 1.0 } else { 0.0 },
        )
    };
    // Downscale anything beyond the GPU's max texture dimension so huge images
    // (e.g. panoramas) upload instead of failing device validation. RGBA8 only —
    // HDR sources are already fit-sized in the decoder.
    let (image, img_w, img_h) = if hdr {
        (Cow::Borrowed(image), img_w, img_h)
    } else {
        clamp_to_max(
            image,
            img_w,
            img_h,
            device.limits().max_texture_dimension_2d,
        )
    };
    let image: &[u8] = &image;

    let size = wgpu::Extent3d {
        width: img_w,
        height: img_h,
        depth_or_array_layers: 1,
    };
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("image"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: tex_format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    // The staging-ring upload (`copy_buffer_to_texture`), not `write_texture`.
    uploader.upload(device, queue, &tex, image, img_w, img_h);
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    // Linear so large photos scaled down to the screen are smooth, not grainy/
    // aliased. (Crisp high-ratio downscaling via mipmaps/Lanczos is a later
    // quality pass; bilinear is the big first step up from nearest.)
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    // Per-image color-transform uniform (matrix + TRC + mode + scale). Tiny and
    // created off the keypress frame, so it's baked into the slot's bind group.
    let color_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("color-uniform"),
        contents: bytemuck::bytes_of(&ColorUniform::new(color, mode, scale)),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("img-bg"),
        layout: bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: color_buf.as_entire_binding(),
            },
        ],
    })
}

fn vertex_buffer(
    device: &wgpu::Device,
    view: &ViewTransform,
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("vbuf"),
        contents: bytemuck::cast_slice(&quad_vertices(view, img_w, img_h, screen_w, screen_h, 0)),
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
    })
}

fn index_buffer(device: &wgpu::Device) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("ibuf"),
        contents: bytemuck::cast_slice(&INDICES),
        usage: wgpu::BufferUsages::INDEX,
    })
}

/// Scene pass: clear the (linear) intermediate to the letterbox and draw the photo
/// quad alpha-blended over it.
fn draw_scene(
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
    vbuf: &wgpu::Buffer,
    ibuf: &wgpu::Buffer,
    clear: wgpu::Color,
) {
    let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("scene"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(clear),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
    rp.set_pipeline(pipeline);
    rp.set_bind_group(0, bind_group, &[]);
    rp.set_vertex_buffer(0, vbuf.slice(..));
    rp.set_index_buffer(ibuf.slice(..), wgpu::IndexFormat::Uint16);
    rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
}

/// Clear-only scene pass: fill the intermediate with `clear` (the letterbox
/// background) and draw nothing. Used for the blank, image-free state.
fn clear_scene(encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView, clear: wgpu::Color) {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("scene-blank"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(clear),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
}

/// Tone-map pass: fullscreen-sample the intermediate into `view` (the surface).
fn draw_tonemap(
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
) {
    let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("tonemap"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
    rp.set_pipeline(pipeline);
    rp.set_bind_group(0, bind_group, &[]);
    rp.draw(0..3, 0..1);
}

fn instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        // `PRIMARY` = the per-OS primary backend: DX12 on Windows, Metal on macOS,
        // Vulkan on Linux (ADR-002). Excludes the GL secondary backend, so the
        // "no GL fallback" posture is unchanged — this just adds Metal for the port.
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    })
}

/// Per-adapter limits: conservative defaults except two raised to what *this* GPU
/// supports — `max_texture_dimension_2d` (16384 on a 5090, often 8192 elsewhere)
/// and `max_buffer_size`. The latter matters because the staging-ring upload
/// stages a whole frame in one buffer; a large Original-mode/panorama image can
/// exceed wgpu's 256 MiB default even while fitting the texture-dimension limit.
/// Keeping the rest at defaults stays portable, and `clamp_to_max` downscales
/// images that exceed whatever limit we end up with.
fn device_limits(adapter: &wgpu::Adapter) -> wgpu::Limits {
    let adapter_limits = adapter.limits();
    wgpu::Limits {
        max_texture_dimension_2d: adapter_limits.max_texture_dimension_2d,
        max_buffer_size: adapter_limits.max_buffer_size,
        ..wgpu::Limits::default()
    }
}

fn device_descriptor(limits: wgpu::Limits) -> wgpu::DeviceDescriptor<'static> {
    wgpu::DeviceDescriptor {
        label: Some("pb-render"),
        required_features: wgpu::Features::empty(),
        required_limits: limits,
        memory_hints: wgpu::MemoryHints::Performance,
    }
}

/// The corner info-panel overlay, when shown.
struct OverlayDraw {
    bind_group: wgpu::BindGroup,
    vbuf: wgpu::Buffer,
    panel_w: u32,
    panel_h: u32,
    margin: u32,
    /// Secondary inset, used only by the top-right **chip** (the scan count): `margin` is
    /// its right inset, `margin_top` its top inset (so it can sit *below* the pie). `0` and
    /// ignored for every other layer.
    margin_top: u32,
}

/// One resident ring slot: a pre-uploaded image texture reused across photos, so
/// a keypress is a rebind (`present_slot`) — never a decode or upload. v1 slots
/// are image-sized; the win is that the texture is uploaded during prefetch, off
/// the keypress frame. Fixed-size slots + sub-rect UVs (zero prefetch-time
/// allocation) are the documented next optimization behind `reserve_ring`'s
/// `slot_w/slot_h` params.
struct RingSlot {
    bind_group: wgpu::BindGroup,
    w: u32,
    h: u32,
    /// Scene-linear peak (SDR tone-map white point on an SDR display); 1.0 for SDR.
    peak: f32,
}

/// On-screen presenter for a window surface.
pub struct WgpuRenderer {
    surface: wgpu::Surface<'static>,
    /// Kept so the surface can be re-queried for its capabilities on every reconfigure. A
    /// lost/reset surface (WSLg / remote / software lavapipe, when the display connection
    /// hiccups) can momentarily report a *different* — even empty — present-mode set than at
    /// construction, and `configure` **panics** on a mode that's no longer supported. See
    /// [`WgpuRenderer::reconfigure_surface`].
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    /// Scene → fp16 intermediate, tone-map → surface, overlay → surface.
    scene_pipeline: wgpu::RenderPipeline,
    tonemap_pipeline: wgpu::RenderPipeline,
    overlay_pipeline: wgpu::RenderPipeline,
    egui_pipeline: wgpu::RenderPipeline,
    /// Layout for image (and overlay) bind groups: tex + sampler + color uniform.
    bgl: wgpu::BindGroupLayout,
    /// Layout for the tone-map bind group (rebuilt with the intermediate on resize).
    tonemap_bgl: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    vbuf: wgpu::Buffer,
    ibuf: wgpu::Buffer,
    img_w: u32,
    img_h: u32,
    /// The fp16 scRGB-linear intermediate the scene renders into, plus the bind
    /// group the tone-map pass samples it through. Both rebuilt on resize.
    intermediate: wgpu::Texture,
    tonemap_bind_group: wgpu::BindGroup,
    /// Present uniform (displayed image's peak + HDR-surface flag).
    peak_buf: wgpu::Buffer,
    /// True when the surface is fp16 scRGB (HDR/wide-gamut display).
    hdr_surface: bool,
    /// True when the display has EDR headroom (macOS) / desktop HDR is on (Windows) —
    /// drives `wantsExtendedDynamicRangeContent` on the macOS CAMetalLayer.
    hdr_on: bool,
    /// EDR roll-off target for the present pass (macOS); 0 = pass straight through.
    edr_headroom: f32,
    /// SDR-content output scale in scRGB units (used only on an HDR surface).
    sdr_scale: f32,
    /// Per-photo view transform (scaling mode + rotation + zoom + pan).
    view: ViewTransform,
    overlay: Option<OverlayDraw>,
    /// The basic info line (`i`), its own bottom-right layer, independent of the
    /// rich-panel `overlay` slot so the two coexist (task #54).
    info_line: Option<OverlayDraw>,
    /// The transient bottom-center status toast, drawn as its own overlay layer
    /// (independent of the info panel) — e.g. "Recursive folders: on".
    toast: Option<OverlayDraw>,
    /// The top-right "loading" pie, shown while the next photo isn't ready yet
    /// (its own overlay layer, composited above the photo + panels).
    pie: Option<OverlayDraw>,
    /// The top-right scan-count chip ("12 / 1234…"), shown while a folder scan streams in,
    /// sitting just below the pie. Its own layer; cleared when the scan ends.
    chip: Option<OverlayDraw>,
    /// A centered, persistent message panel — the empty-state "Press O to open…"
    /// hint shown over the blank background. Its own layer; cleared the moment a
    /// photo is shown (`set_image` / `present_slot`).
    message: Option<OverlayDraw>,
    /// The folder-tree panel (`Shift+F`), anchored `margin` px in from the
    /// top-left corner (the info panel's inset, mirrored). Its own layer.
    tree: Option<OverlayDraw>,
    /// The egui rich-panel overlay's bind group over a shell-owned offscreen texture
    /// (`Rgba8UnormSrgb`). Retained across nav frames — the shell only re-hands it on
    /// (re)allocation / resize, and re-renders egui *into* the same texture when a
    /// panel changes, so per-nav-frame egui cost is zero (task #54 Phase 4). Composited
    /// bufferless-fullscreen, above the CPU overlay layers.
    egui: Option<wgpu::BindGroup>,
    upload: Box<dyn UploadStrategy>,
    /// Resident texture ring (Phase 3). Empty until `reserve_ring`; each `Some`
    /// slot holds a pre-uploaded photo. `present_slot` selects which one draws.
    ring: Vec<Option<RingSlot>>,
    /// When `Some(i)`, `render` draws ring slot `i` instead of `bind_group`.
    present_idx: Option<usize>,
    /// Background (letterbox) fill, sRGB, shown around a non-covering photo.
    /// Defaults to [`LETTERBOX`]; the app overrides it from user settings.
    letterbox: [u8; 3],
    /// Spike (task #59): height in **physical px** of a translucent top bar the surface
    /// extends *under* (a glass toolbar). The photo is fit/centered against the content region
    /// below it (`surface_h - inset`) and the quad is offset down by `inset`, so a fitted photo
    /// stays fully below the bar while a zoomed/cropped one's natural overflow shows under the
    /// glass. `0` (default) = the classic opaque-bar behavior, untouched.
    content_top_inset: u32,
    /// No image loaded (bare launch with no folder, or the last photo deleted):
    /// draw just the letterbox background, no photo quad. Cleared by `set_image` /
    /// `present_slot`, set by `clear_image`.
    blank: bool,
}

impl WgpuRenderer {
    /// Create a presenter for `target` (e.g. an `Arc<Window>`) and upload `image`
    /// with its `color` transform (use [`ColorTransform::srgb`] for sRGB sources).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        target: impl Into<wgpu::SurfaceTarget<'static>>,
        width: u32,
        height: u32,
        image: &[u8],
        img_w: u32,
        img_h: u32,
        color: ColorTransform,
        hdr: bool,
        peak: f32,
    ) -> Self {
        let instance = instance();
        let surface = instance.create_surface(target).expect("create surface");
        pollster::block_on(Self::new_async(
            instance, surface, width, height, image, img_w, img_h, color, hdr, peak,
        ))
    }

    /// macOS-only: create the presenter on a **host-owned, retained `CAMetalLayer`** — the
    /// AppKit/SwiftUI shell path (NS1, ADR-021), where the window/view/layer belong to the
    /// Swift host and Rust only draws into the layer it's handed. winit is absent on that
    /// target, so there is no raw-window-handle; the surface goes through wgpu's unsafe
    /// `SurfaceTargetUnsafe::CoreAnimationLayer` instead.
    ///
    /// # Safety
    /// - `layer` must point to a valid, **retained** `CAMetalLayer`, and it must outlive
    ///   the returned renderer — the host drops the renderer *before* the view/layer dies.
    /// - Creation, `resize`, `render`, and drop must all happen on the **main thread**
    ///   (AppKit layers are not safe to reconfigure off it); the FFI effect-drain rule
    ///   already pins the callers there.
    /// - The host reports size changes (`resize`) before drawing at a new size.
    #[cfg(target_os = "macos")]
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn new_from_ca_layer(
        layer: *mut std::ffi::c_void,
        width: u32,
        height: u32,
        image: &[u8],
        img_w: u32,
        img_h: u32,
        color: ColorTransform,
        hdr: bool,
        peak: f32,
    ) -> Self {
        let instance = instance();
        // SAFETY: the caller upholds the layer contract documented above.
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(layer))
        }
        .expect("create surface from CAMetalLayer");
        pollster::block_on(Self::new_async(
            instance, surface, width, height, image, img_w, img_h, color, hdr, peak,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn new_async(
        instance: wgpu::Instance,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
        image: &[u8],
        img_w: u32,
        img_h: u32,
        color: ColorTransform,
        hdr: bool,
        peak: f32,
    ) -> Self {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
            })
            .await
            .expect("no compatible GPU adapter");
        let (device, queue) = adapter
            .request_device(&device_descriptor(device_limits(&adapter)), None)
            .await
            .expect("request device");

        let caps = surface.get_capabilities(&adapter);
        // HDR/wide-gamut output: when the desktop is in HDR mode and the surface can
        // be `Rgba16Float`, present that — a float flip-model swapchain is always
        // **scRGB** (linear, BT.709, extended range), so wide-gamut/HDR values reach
        // the panel. Otherwise pick an **8-bit non-sRGB** surface (the present pass
        // sRGB-encodes; an sRGB surface would double-encode and washes out).
        let disp = crate::display::primary_hdr();
        // Use the fp16 scRGB surface whenever the panel benefits: HDR-on (extended
        // range), OR wide-gamut (P3+). On macOS `wide_gamut` is set even for an SDR P3
        // panel, so P3 photos light up there without needing EDR (the surface's
        // CAMetalLayer is configured to extended-linear-sRGB in pb-app); on Windows
        // `wide_gamut` tracks `hdr_on`, so behavior is unchanged.
        let want_fp16 = (disp.hdr_on || disp.wide_gamut)
            && caps.formats.contains(&wgpu::TextureFormat::Rgba16Float);
        let format = if want_fp16 {
            wgpu::TextureFormat::Rgba16Float
        } else {
            caps.formats
                .iter()
                .copied()
                .find(|f| {
                    matches!(
                        f,
                        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm
                    )
                })
                .or_else(|| caps.formats.iter().copied().find(|f| !f.is_srgb()))
                .unwrap_or(caps.formats[0])
        };
        // Mailbox = low latency, no tearing; fall back to Fifo if unsupported. On a software
        // (Cpu) adapter — lavapipe under WSLg — prefer Fifo even when Mailbox is advertised:
        // its triple-buffering is unstable there (drawable timeouts / surface loss / the
        // present-mode reconfigure panic) while Fifo (plain vsync) is lighter and steadier.
        // Real GPUs (DX12 / Metal / Vulkan) keep Mailbox — the low-latency path is the point.
        let want_mailbox = caps.present_modes.contains(&wgpu::PresentMode::Mailbox)
            && adapter.get_info().device_type != wgpu::DeviceType::Cpu;
        let present_mode = if want_mailbox {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let sdr_scale = disp.sdr_scale();
        let scene_scale = if want_fp16 && !hdr { sdr_scale } else { 1.0 };
        let view = ViewTransform::default();
        let pipelines = build_pipelines(&device, format);
        let mut upload: Box<dyn UploadStrategy> = Box::new(StagingUpload::new());
        let bind_group = upload_image(
            &device,
            &queue,
            &pipelines.scene_bgl,
            upload.as_mut(),
            image,
            img_w,
            img_h,
            &color,
            hdr,
            scene_scale,
        );
        let vbuf = vertex_buffer(&device, &view, img_w, img_h, config.width, config.height);
        let ibuf = index_buffer(&device);

        // Present uniform (peak for SDR tone-map; HDR-surface flag) + the fp16
        // intermediate the scene renders into.
        let peak_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("present-uniform"),
            contents: bytemuck::bytes_of(&PresentUniform::new(peak, want_fp16, disp.edr_headroom)),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let (intermediate, tonemap_bind_group) = make_intermediate(
            &device,
            &pipelines.tonemap_bgl,
            &peak_buf,
            config.width,
            config.height,
        );

        Self {
            surface,
            adapter,
            device,
            queue,
            config,
            scene_pipeline: pipelines.scene,
            tonemap_pipeline: pipelines.tonemap,
            overlay_pipeline: pipelines.overlay,
            egui_pipeline: pipelines.egui,
            bgl: pipelines.scene_bgl,
            tonemap_bgl: pipelines.tonemap_bgl,
            bind_group,
            vbuf,
            ibuf,
            img_w,
            img_h,
            intermediate,
            tonemap_bind_group,
            peak_buf,
            hdr_surface: want_fp16,
            hdr_on: disp.hdr_on,
            edr_headroom: disp.edr_headroom,
            sdr_scale,
            view,
            overlay: None,
            info_line: None,
            toast: None,
            pie: None,
            chip: None,
            upload,
            ring: Vec::new(),
            present_idx: None,
            letterbox: [LETTERBOX[0], LETTERBOX[1], LETTERBOX[2]],
            content_top_inset: 0,
            blank: false,
            message: None,
            tree: None,
            egui: None,
        }
    }

    /// The per-image output scale: SDR content on an HDR surface is lifted to the
    /// SDR white level; HDR content (and any SDR-surface content) uses 1.0.
    fn scene_scale(&self, hdr: bool) -> f32 {
        if self.hdr_surface && !hdr {
            self.sdr_scale
        } else {
            1.0
        }
    }

    /// Update the present uniform's tone-map white point to the displayed image's
    /// peak (only consequential on an SDR surface; harmless on an HDR surface).
    fn set_present_peak(&self, peak: f32) {
        self.queue.write_buffer(
            &self.peak_buf,
            0,
            bytemuck::bytes_of(&PresentUniform::new(
                peak,
                self.hdr_surface,
                self.edr_headroom,
            )),
        );
    }

    /// The display refresh cap helper will live here in Phase 3; for now the app
    /// reads it from winit directly.
    pub fn present_mode(&self) -> wgpu::PresentMode {
        self.config.present_mode
    }

    /// Reconfigure the surface, **re-querying its capabilities first**. A lost/reset surface —
    /// common on WSLg / remote / software (lavapipe) backends when the display connection
    /// hiccups ("Connection reset by peer") — can report a shrunken or **empty** present-mode
    /// set, and `wgpu`'s `configure` *panics* on a present mode that's no longer in the list
    /// (the crash: "Requested present mode Mailbox is not in the list of supported present
    /// modes: []"). So skip entirely while the surface reports no usable modes — the frame is
    /// dropped and the next one retries once the connection recovers — and otherwise clamp the
    /// present mode (and format / alpha) to something actually supported (Mailbox if available,
    /// else the spec-guaranteed Fifo). Returns whether the surface was (re)configured.
    ///
    /// A no-op change on healthy surfaces (Windows DX12 / macOS Metal): the caps are stable, so
    /// the config is re-applied unchanged — this only adds a safety net where the surface can
    /// vanish out from under us.
    fn reconfigure_surface(&mut self) -> bool {
        let caps = self.surface.get_capabilities(&self.adapter);
        if caps.present_modes.is_empty() || caps.formats.is_empty() {
            // Surface not currently usable (lost / reset). Configuring now would panic — drop
            // the frame and retry next time; the surface comes back when the display does.
            return false;
        }
        if !caps.present_modes.contains(&self.config.present_mode) {
            self.config.present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox)
            {
                wgpu::PresentMode::Mailbox
            } else {
                wgpu::PresentMode::Fifo
            };
        }
        if !caps.formats.contains(&self.config.format) {
            self.config.format = caps.formats[0];
        }
        if !caps.alpha_modes.contains(&self.config.alpha_mode) {
            self.config.alpha_mode = caps.alpha_modes[0];
        }
        self.surface.configure(&self.device, &self.config);
        true
    }
}

impl Renderer for WgpuRenderer {
    /// Override the letterbox / background fill color (sRGB), shown around a photo
    /// that doesn't cover the screen. Takes effect on the next `render`. Off the
    /// photo hot path — set from user settings, not per frame.
    fn set_letterbox(&mut self, rgb: [u8; 3]) {
        self.letterbox = rgb;
    }

    fn set_content_top_inset(&mut self, px: u32) {
        self.content_top_inset = px;
    }

    /// Set or clear the transient bottom-center status toast (tasks.json #10). It
    /// is an independent overlay layer, so it composites *over* the info panel
    /// rather than replacing it; the caller fades it by re-uploading with scaled
    /// alpha. `bottom_margin` is the gap from the bottom edge.
    fn set_toast(&mut self, panel: Option<(&[u8], u32, u32)>, bottom_margin: u32) {
        self.toast = match panel {
            Some((rgba, w, h)) => {
                let scale = self.scene_scale(false);
                let bind_group = upload_image(
                    &self.device,
                    &self.queue,
                    &self.bgl,
                    self.upload.as_mut(),
                    rgba,
                    w,
                    h,
                    &ColorTransform::srgb(),
                    false,
                    scale,
                );
                let vbuf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("toast-vbuf"),
                        contents: bytemuck::cast_slice(&toast_quad_vertices(
                            w,
                            h,
                            self.config.width,
                            self.config.height,
                            bottom_margin,
                        )),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
                Some(OverlayDraw {
                    bind_group,
                    vbuf,
                    panel_w: w,
                    panel_h: h,
                    margin: bottom_margin,
                    margin_top: 0,
                })
            }
            None => None,
        };
    }

    /// Set or clear the top-right "loading" pie (shown while the next photo isn't
    /// ready). Its own overlay layer, composited above the photo and the panels;
    /// the caller animates the fill / fade by re-uploading the rasterized bitmap.
    /// `margin` is the gap from the top and right edges.
    fn set_pie(&mut self, panel: Option<(&[u8], u32, u32)>, margin: u32) {
        self.pie = match panel {
            Some((rgba, w, h)) => {
                let scale = self.scene_scale(false);
                let bind_group = upload_image(
                    &self.device,
                    &self.queue,
                    &self.bgl,
                    self.upload.as_mut(),
                    rgba,
                    w,
                    h,
                    &ColorTransform::srgb(),
                    false,
                    scale,
                );
                let vbuf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("pie-vbuf"),
                        contents: bytemuck::cast_slice(&top_right_quad_vertices(
                            w,
                            h,
                            self.config.width,
                            self.config.height,
                            margin,
                        )),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
                Some(OverlayDraw {
                    bind_group,
                    vbuf,
                    panel_w: w,
                    panel_h: h,
                    margin,
                    margin_top: 0,
                })
            }
            None => None,
        };
    }

    fn device(&self) -> &wgpu::Device {
        &self.device
    }

    fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    fn set_egui_overlay(&mut self, texture: Option<&wgpu::Texture>) {
        self.egui = texture.map(|tex| {
            // Sample through the texture's own (sRGB) format so the sampler decodes the
            // store back to premultiplied linear; `fs_egui` then composites it straight.
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });
            // Only `scale.x` is read by `fs_egui` — the SDR-white lift on an HDR surface
            // (1.0 on SDR), matching the CPU overlay layers.
            let color_buf = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("egui-color-uniform"),
                    contents: bytemuck::bytes_of(&ColorUniform::new(
                        &ColorTransform::srgb(),
                        0.0,
                        self.scene_scale(false),
                    )),
                    usage: wgpu::BufferUsages::UNIFORM,
                });
            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("egui-bg"),
                layout: &self.bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: color_buf.as_entire_binding(),
                    },
                ],
            })
        });
    }

    fn set_edr_headroom(&mut self, headroom: f32) {
        self.edr_headroom = headroom.max(1.0);
        // Peak is unused on the HDR surface (the present pass keys off the headroom),
        // so any value is fine here; the next per-image `set_present_peak` refreshes it.
        self.queue.write_buffer(
            &self.peak_buf,
            0,
            bytemuck::bytes_of(&PresentUniform::new(
                1.0,
                self.hdr_surface,
                self.edr_headroom,
            )),
        );
    }

    fn hdr_surface_wants_edr(&self) -> Option<bool> {
        self.hdr_surface.then_some(self.hdr_on)
    }

    fn image_size(&self) -> (u32, u32) {
        (self.img_w, self.img_h)
    }

    fn poll(&self) {
        self.device.poll(wgpu::Maintain::Poll);
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if width == self.config.width && height == self.config.height {
            return; // already correctly sized — avoid a redundant reconfigure
        }
        self.config.width = width;
        self.config.height = height;
        self.reconfigure_surface();
        // The fp16 intermediate must track the surface size.
        let (intermediate, tonemap_bind_group) = make_intermediate(
            &self.device,
            &self.tonemap_bgl,
            &self.peak_buf,
            width,
            height,
        );
        self.intermediate = intermediate;
        self.tonemap_bind_group = tonemap_bind_group;
        // Re-place the quad for the new viewport.
        self.queue.write_buffer(
            &self.vbuf,
            0,
            bytemuck::cast_slice(&quad_vertices(
                &self.view, self.img_w, self.img_h, width, height, self.content_top_inset,
            )),
        );
        // The overlay panel's corner position depends on the viewport.
        if let Some(ov) = &self.overlay {
            self.queue.write_buffer(
                &ov.vbuf,
                0,
                bytemuck::cast_slice(&overlay_quad_vertices(
                    ov.panel_w, ov.panel_h, width, height, ov.margin,
                )),
            );
        }
        if let Some(t) = &self.toast {
            self.queue.write_buffer(
                &t.vbuf,
                0,
                bytemuck::cast_slice(&toast_quad_vertices(
                    t.panel_w, t.panel_h, width, height, t.margin,
                )),
            );
        }
        if let Some(p) = &self.pie {
            self.queue.write_buffer(
                &p.vbuf,
                0,
                bytemuck::cast_slice(&top_right_quad_vertices(
                    p.panel_w, p.panel_h, width, height, p.margin,
                )),
            );
        }
        if let Some(c) = &self.chip {
            self.queue.write_buffer(
                &c.vbuf,
                0,
                bytemuck::cast_slice(&top_right_quad_xy(
                    c.panel_w,
                    c.panel_h,
                    width,
                    height,
                    c.margin,
                    c.margin_top,
                )),
            );
        }
        if let Some(m) = &self.message {
            self.queue.write_buffer(
                &m.vbuf,
                0,
                bytemuck::cast_slice(&center_quad_vertices(m.panel_w, m.panel_h, width, height)),
            );
        }
        if let Some(t) = &self.tree {
            self.queue.write_buffer(
                &t.vbuf,
                0,
                bytemuck::cast_slice(&top_left_quad_vertices(
                    t.panel_w, t.panel_h, width, height, t.margin,
                )),
            );
        }
    }

    fn clear_image(&mut self) {
        // Show the letterbox background only; the kept bind group/ring is simply not
        // drawn until the next set_image / present_slot (see `render`).
        self.blank = true;
        self.present_idx = None;
    }

    fn set_image(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        color: ColorTransform,
        hdr: bool,
        peak: f32,
    ) {
        let scale = self.scene_scale(hdr);
        self.bind_group = upload_image(
            &self.device,
            &self.queue,
            &self.bgl,
            self.upload.as_mut(),
            rgba,
            width,
            height,
            &color,
            hdr,
            scale,
        );
        self.blank = false; // an image is showing again
        self.message = None; // hide the empty-state hint
        self.set_present_peak(peak);
        // Revert to the single-image path; a later present_slot re-selects a slot.
        self.present_idx = None;
        self.img_w = width;
        self.img_h = height;
        // Re-place the quad for the new image.
        self.queue.write_buffer(
            &self.vbuf,
            0,
            bytemuck::cast_slice(&quad_vertices(
                &self.view,
                width,
                height,
                self.config.width,
                self.config.height,
                self.content_top_inset,
            )),
        );
    }

    fn set_view(&mut self, view: ViewTransform) {
        self.view = view;
        self.queue.write_buffer(
            &self.vbuf,
            0,
            bytemuck::cast_slice(&quad_vertices(
                &self.view,
                self.img_w,
                self.img_h,
                self.config.width,
                self.config.height,
                self.content_top_inset,
            )),
        );
    }

    fn set_overlay(
        &mut self,
        panel: Option<(&[u8], u32, u32)>,
        right_margin: u32,
        bottom_margin: u32,
    ) {
        self.overlay = match panel {
            Some((rgba, w, h)) => {
                // Overlays (Inspector / help) are sRGB UI bitmaps, composited into
                // the linear intermediate; scaled to SDR white on an HDR surface.
                let scale = self.scene_scale(false);
                let bind_group = upload_image(
                    &self.device,
                    &self.queue,
                    &self.bgl,
                    self.upload.as_mut(),
                    rgba,
                    w,
                    h,
                    &ColorTransform::srgb(),
                    false,
                    scale,
                );
                let vbuf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("overlay-vbuf"),
                        contents: bytemuck::cast_slice(&bottom_right_quad_xy(
                            w,
                            h,
                            self.config.width,
                            self.config.height,
                            right_margin,
                            bottom_margin,
                        )),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
                Some(OverlayDraw {
                    bind_group,
                    vbuf,
                    panel_w: w,
                    panel_h: h,
                    margin: right_margin,
                    margin_top: bottom_margin,
                })
            }
            None => None,
        };
    }

    /// Set or clear the basic info line — its own bottom-anchored layer (`align`:
    /// left / center / right), so it coexists with the rich-panel `overlay` slot.
    /// Drawn like the info panel: an sRGB UI bitmap composited into the intermediate.
    fn set_info_line(
        &mut self,
        panel: Option<(&[u8], u32, u32)>,
        margin: u32,
        align: crate::HAlign,
    ) {
        self.info_line = match panel {
            Some((rgba, w, h)) => {
                let scale = self.scene_scale(false);
                let bind_group = upload_image(
                    &self.device,
                    &self.queue,
                    &self.bgl,
                    self.upload.as_mut(),
                    rgba,
                    w,
                    h,
                    &ColorTransform::srgb(),
                    false,
                    scale,
                );
                let vbuf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("info-line-vbuf"),
                        contents: bytemuck::cast_slice(&bottom_aligned_quad(
                            w,
                            h,
                            self.config.width,
                            self.config.height,
                            margin,
                            align,
                        )),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
                Some(OverlayDraw {
                    bind_group,
                    vbuf,
                    panel_w: w,
                    panel_h: h,
                    margin,
                    margin_top: 0,
                })
            }
            None => None,
        };
    }

    /// Set or clear the folder-tree panel (`Shift+F`): drawn `margin` px in from the
    /// top and left edges. Its own overlay layer, drawn like the info panel.
    fn set_tree(&mut self, panel: Option<(&[u8], u32, u32)>, margin: u32) {
        self.tree = match panel {
            Some((rgba, w, h)) => {
                let scale = self.scene_scale(false);
                let bind_group = upload_image(
                    &self.device,
                    &self.queue,
                    &self.bgl,
                    self.upload.as_mut(),
                    rgba,
                    w,
                    h,
                    &ColorTransform::srgb(),
                    false,
                    scale,
                );
                let vbuf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("tree-vbuf"),
                        contents: bytemuck::cast_slice(&top_left_quad_vertices(
                            w,
                            h,
                            self.config.width,
                            self.config.height,
                            margin,
                        )),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
                Some(OverlayDraw {
                    bind_group,
                    vbuf,
                    panel_w: w,
                    panel_h: h,
                    margin,
                    margin_top: 0,
                })
            }
            None => None,
        };
    }

    fn reserve_ring(&mut self, capacity: usize, _slot_w: u32, _slot_h: u32) {
        // v1 uses image-sized slots, so slot_w/slot_h aren't needed yet (they're
        // kept for the fixed-size + sub-rect-UV variant). Allocate empty slots.
        self.ring = (0..capacity).map(|_| None).collect();
        self.present_idx = None;
    }

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
    ) {
        if slot >= self.ring.len() {
            return;
        }
        let scale = self.scene_scale(hdr);
        let bind_group = upload_image(
            &self.device,
            &self.queue,
            &self.bgl,
            self.upload.as_mut(),
            rgba,
            w,
            h,
            &color,
            hdr,
            scale,
        );
        self.ring[slot] = Some(RingSlot {
            bind_group,
            w,
            h,
            peak,
        });
    }

    fn present_slot(&mut self, slot: usize) {
        let Some((w, h, peak)) = self
            .ring
            .get(slot)
            .and_then(|s| s.as_ref())
            .map(|s| (s.w, s.h, s.peak))
        else {
            return; // unknown / not-yet-uploaded slot: keep the current frame
        };
        self.blank = false; // a photo is showing again
        self.message = None; // hide the empty-state hint
        self.set_present_peak(peak);
        self.present_idx = Some(slot);
        self.img_w = w;
        self.img_h = h;
        self.queue.write_buffer(
            &self.vbuf,
            0,
            bytemuck::cast_slice(&quad_vertices(
                &self.view,
                w,
                h,
                self.config.width,
                self.config.height,
                self.content_top_inset,
            )),
        );
    }

    fn render(&mut self) -> Result<bool, RenderError> {
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            // Dropped frames (`Ok(false)`) are NOT silent successes: nothing reached
            // the screen, and the caller must retry or the compositor keeps the stale
            // frame up indefinitely. The eprintln is deliberate — this is rare, and
            // its absence from a trace cost a debugging session (2026-07-04).
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                // Re-query caps and clamp the present mode — a lost surface (WSLg/software) can
                // report an empty/shrunken mode set, and a blind `configure` with the old mode
                // panics. `reconfigure_surface` skips safely until the surface is usable again.
                eprintln!("render: surface lost/outdated — reconfigured, frame dropped");
                self.reconfigure_surface();
                return Ok(false);
            }
            Err(wgpu::SurfaceError::Timeout) => {
                eprintln!("render: drawable timeout — frame dropped");
                return Ok(false);
            }
            Err(wgpu::SurfaceError::OutOfMemory) => return Err(RenderError::OutOfMemory),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let intermediate_view = self
            .intermediate
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("present"),
            });
        // Pass 1: scene → fp16 intermediate. The selected resident-ring slot if one
        // is presented, else the single image — a keypress rebinds via `present_idx`,
        // no upload here. When blank (no image loaded), clear to the letterbox
        // background and draw no photo quad — a plain, image-free screen.
        if self.blank {
            clear_scene(
                &mut encoder,
                &intermediate_view,
                letterbox_linear(self.letterbox),
            );
        } else {
            let bind_group = match self.present_idx {
                Some(i) => self
                    .ring
                    .get(i)
                    .and_then(|s| s.as_ref())
                    .map(|s| &s.bind_group)
                    .unwrap_or(&self.bind_group),
                None => &self.bind_group,
            };
            draw_scene(
                &mut encoder,
                &intermediate_view,
                &self.scene_pipeline,
                bind_group,
                &self.vbuf,
                &self.ibuf,
                letterbox_linear(self.letterbox),
            );
        }
        // Pass 2: alpha-blend the info panel into the intermediate (in linear), so
        // the single present pass below serves both the SDR and HDR output paths.
        if let Some(ov) = &self.overlay {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.overlay_pipeline);
            rp.set_bind_group(0, &ov.bind_group, &[]);
            rp.set_vertex_buffer(0, ov.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        // Pass 2a: the basic info line (bottom-right), its own layer so it sits
        // alongside the rich panel (which lifts above it) rather than replacing it.
        if let Some(l) = &self.info_line {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("info-line"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.overlay_pipeline);
            rp.set_bind_group(0, &l.bind_group, &[]);
            rp.set_vertex_buffer(0, l.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        // Pass 2a′: the folder-tree panel (top-left corner), its own layer beside
        // the info panel.
        if let Some(t) = &self.tree {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tree"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.overlay_pipeline);
            rp.set_bind_group(0, &t.bind_group, &[]);
            rp.set_vertex_buffer(0, t.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        // Pass 2b: the transient status toast (bottom-center), composited on top of
        // the photo and the info panel.
        if let Some(t) = &self.toast {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("toast"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.overlay_pipeline);
            rp.set_bind_group(0, &t.bind_group, &[]);
            rp.set_vertex_buffer(0, t.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        // Pass 2c: the top-right "loading" pie, composited above everything else so
        // it stays visible while the next photo decodes.
        if let Some(p) = &self.pie {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pie"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.overlay_pipeline);
            rp.set_bind_group(0, &p.bind_group, &[]);
            rp.set_vertex_buffer(0, p.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        // Pass 2c′: the top-right scan-count chip, just below the pie.
        if let Some(c) = &self.chip {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("chip"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.overlay_pipeline);
            rp.set_bind_group(0, &c.bind_group, &[]);
            rp.set_vertex_buffer(0, c.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        // Pass 2d: the centered empty-state message ("Press O to open…"), composited
        // over the blank background.
        if let Some(m) = &self.message {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("message"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.overlay_pipeline);
            rp.set_bind_group(0, &m.bind_group, &[]);
            rp.set_vertex_buffer(0, m.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        // Pass 2e: the egui rich-panel overlay (Inspector / Help / folder tree), drawn
        // last so it sits above every CPU layer. The shell-owned offscreen texture is
        // already premultiplied; the bufferless fullscreen triangle stretches it 1:1
        // over the viewport and `fs_egui` composites it with premultiplied blend.
        if let Some(bg) = &self.egui {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui-overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.egui_pipeline);
            rp.set_bind_group(0, bg, &[]);
            rp.draw(0..3, 0..1);
        }
        // Pass 3: present the intermediate onto the surface (SDR tone-map+encode, or
        // HDR scRGB scale, per the present uniform).
        draw_tonemap(
            &mut encoder,
            &view,
            &self.tonemap_pipeline,
            &self.tonemap_bind_group,
        );
        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        Ok(true)
    }
}

/// Render the scene to an RGBA8 buffer (`screen_w * screen_h * 4`) with no
/// window. Backs the golden-image tests. Uses `Rgba8Unorm` so texels are exact.
pub fn render_offscreen(
    image: &[u8],
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
) -> Vec<u8> {
    render_offscreen_color(
        image,
        img_w,
        img_h,
        screen_w,
        screen_h,
        ColorTransform::srgb(),
    )
}

/// Like [`render_offscreen`] but applies `color` (the in-shader source→sRGB
/// conversion). Backs the color-management golden tests.
pub fn render_offscreen_color(
    image: &[u8],
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
    color: ColorTransform,
) -> Vec<u8> {
    pollster::block_on(render_offscreen_async(
        image, img_w, img_h, screen_w, screen_h, color,
    ))
}

async fn render_offscreen_async(
    image: &[u8],
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
    color: ColorTransform,
) -> Vec<u8> {
    let instance = instance();
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        })
        .await
        .expect("no GPU adapter");
    let (device, queue) = adapter
        .request_device(&device_descriptor(device_limits(&adapter)), None)
        .await
        .expect("request device");

    let format = wgpu::TextureFormat::Rgba8Unorm;
    let pipelines = build_pipelines(&device, format);
    let mut upload = StagingUpload::new();
    let bind_group = upload_image(
        &device,
        &queue,
        &pipelines.scene_bgl,
        &mut upload,
        image,
        img_w,
        img_h,
        &color,
        false,
        1.0,
    );
    let vbuf = vertex_buffer(
        &device,
        &ViewTransform::default(),
        img_w,
        img_h,
        screen_w,
        screen_h,
    );
    let ibuf = index_buffer(&device);

    // Same scene → fp16 intermediate → present path as the on-screen renderer, so
    // the golden tests exercise the real pipeline. SDR peak = 1.0 (identity tone-map).
    let peak_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("present-uniform"),
        contents: bytemuck::bytes_of(&PresentUniform::new(1.0, false, 0.0)),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let (intermediate, tonemap_bind_group) = make_intermediate(
        &device,
        &pipelines.tonemap_bgl,
        &peak_buf,
        screen_w,
        screen_h,
    );
    let intermediate_view = intermediate.create_view(&wgpu::TextureViewDescriptor::default());

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen"),
        size: wgpu::Extent3d {
            width: screen_w,
            height: screen_h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());

    // Readback buffer: bytes_per_row must be 256-aligned.
    let unpadded = screen_w * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * screen_h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("offscreen"),
    });
    draw_scene(
        &mut encoder,
        &intermediate_view,
        &pipelines.scene,
        &bind_group,
        &vbuf,
        &ibuf,
        letterbox_linear([LETTERBOX[0], LETTERBOX[1], LETTERBOX[2]]),
    );
    draw_tonemap(&mut encoder, &view, &pipelines.tonemap, &tonemap_bind_group);
    encoder.copy_texture_to_buffer(
        wgpu::ImageCopyTexture {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::ImageCopyBuffer {
            buffer: &readback,
            layout: wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(screen_h),
            },
        },
        wgpu::Extent3d {
            width: screen_w,
            height: screen_h,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(std::iter::once(encoder.finish()));

    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device.poll(wgpu::Maintain::Wait);
    rx.recv().expect("map channel").expect("map readback");

    let mapped = slice.get_mapped_range();
    let mut out = Vec::with_capacity((screen_w * screen_h * 4) as usize);
    for row in 0..screen_h {
        let start = (row * padded) as usize;
        out.extend_from_slice(&mapped[start..start + unpadded as usize]);
    }
    drop(mapped);
    readback.unmap();
    out
}

/// A deterministic test/placeholder image: gray field, colored corner markers
/// (TL red, TR green, BL blue, BR yellow), and a white center block. Makes
/// letterboxing and orientation obvious on screen and exact in tests.
pub fn test_pattern(w: u32, h: u32) -> Vec<u8> {
    let mut px = vec![0u8; (w as usize) * (h as usize) * 4];
    let put = |px: &mut [u8], x: u32, y: u32, c: [u8; 4]| {
        let i = ((y * w + x) * 4) as usize;
        px[i..i + 4].copy_from_slice(&c);
    };
    let bg = [40, 40, 40, 255];
    for y in 0..h {
        for x in 0..w {
            put(&mut px, x, y, bg);
        }
    }
    let m = 120.min(w / 2).min(h / 2);
    let tl = [220, 30, 30, 255];
    let tr = [30, 200, 30, 255];
    let bl = [40, 80, 230, 255];
    let br = [230, 210, 30, 255];
    for y in 0..m {
        for x in 0..m {
            put(&mut px, x, y, tl);
            put(&mut px, w - 1 - x, y, tr);
            put(&mut px, x, h - 1 - y, bl);
            put(&mut px, w - 1 - x, h - 1 - y, br);
        }
    }
    let cw = (w / 4).max(2);
    let ch = (h / 4).max(2);
    let cx0 = w / 2 - cw / 2;
    let cy0 = h / 2 - ch / 2;
    for y in cy0..cy0 + ch {
        for x in cx0..cx0 + cw {
            put(&mut px, x, y, [255, 255, 255, 255]);
        }
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(buf: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    fn close(a: [u8; 4], b: [u8; 4], tol: i32) -> bool {
        (0..4).all(|k| (a[k] as i32 - b[k] as i32).abs() <= tol)
    }

    #[test]
    fn offscreen_letterboxes_and_draws_image() {
        let (iw, ih) = (1600u32, 1000u32);
        let img = test_pattern(iw, ih);
        let (sw, sh) = (1920u32, 1080u32);
        let out = render_offscreen(&img, iw, ih, sw, sh);
        assert_eq!(out.len(), (sw * sh * 4) as usize);

        // 1600x1000 into 1920x1080 is height-bound -> 1728x1080, pillarboxed.
        // Left/right columns are letterbox background.
        assert!(
            close(at(&out, sw, 0, sh / 2), LETTERBOX, 2),
            "left bar = {:?}",
            at(&out, sw, 0, sh / 2)
        );
        assert!(
            close(at(&out, sw, sw - 1, sh / 2), LETTERBOX, 2),
            "right bar = {:?}",
            at(&out, sw, sw - 1, sh / 2)
        );
        // Center is the white block.
        assert!(
            close(at(&out, sw, sw / 2, sh / 2), [255, 255, 255, 255], 4),
            "center = {:?}",
            at(&out, sw, sw / 2, sh / 2)
        );
        // The image fills the full height, so a top-center pixel is image, not bar.
        assert!(
            !close(at(&out, sw, sw / 2, 2), LETTERBOX, 2),
            "top-center should be image, got {:?}",
            at(&out, sw, sw / 2, 2)
        );
    }

    #[test]
    fn transparent_image_blends_over_letterbox() {
        let img = [250, 0, 0, 0];
        let out = render_offscreen(&img, 1, 1, 4, 4);
        assert!(
            close(at(&out, 4, 2, 2), LETTERBOX, 2),
            "transparent image should reveal letterbox, got {:?}",
            at(&out, 4, 2, 2)
        );
    }

    fn srgb_oetf_ref(c: f32) -> f32 {
        let x = c.clamp(0.0, 1.0);
        if x <= 0.0031308 {
            12.92 * x
        } else {
            1.055 * x.powf(1.0 / 2.4) - 0.055
        }
    }

    #[test]
    fn disabled_color_transform_is_bit_exact_passthrough() {
        // A solid opaque pixel rendered with the (disabled) sRGB transform must
        // come back unchanged — the common case stays exact and free.
        let img = [200u8, 50, 90, 255];
        let out = render_offscreen_color(&img, 1, 1, 4, 4, ColorTransform::srgb());
        assert!(
            close(at(&out, 4, 2, 2), [200, 50, 90, 255], 1),
            "passthrough altered the pixel: {:?}",
            at(&out, 4, 2, 2)
        );
    }

    #[test]
    fn enabled_color_transform_applies_curve_and_reencode() {
        // Identity primaries + a gamma-2.0 source curve, re-encoded to sRGB: the
        // shader must linearize (x^2), pass the matrix (identity), then sRGB-encode.
        let g = 2.0f32;
        let color = ColorTransform {
            matrix: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            trc: [g, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            enabled: true,
        };
        let img = [128u8, 128, 128, 255];
        let out = render_offscreen_color(&img, 1, 1, 4, 4, color);
        let got = at(&out, 4, 2, 2);

        let lin = (128.0f32 / 255.0).powf(g);
        let exp = (srgb_oetf_ref(lin) * 255.0).round() as u8;
        assert!(
            close(got, [exp, exp, exp, 255], 2),
            "got {got:?}, expected ~{exp}"
        );
        // And it must actually differ from the input — proof the path ran.
        assert!(
            (got[0] as i32 - 128).abs() > 3,
            "conversion was a no-op: {got:?}"
        );
    }

    #[test]
    fn test_pattern_is_well_formed() {
        let p = test_pattern(64, 40);
        assert_eq!(p.len(), 64 * 40 * 4);
        // top-left marker is red-ish
        assert_eq!(&p[0..4], &[220, 30, 30, 255]);
    }
    #[test]
    fn clamp_leaves_fitting_images_untouched() {
        let img = vec![5u8; 4 * 4 * 4];
        let (out, w, h) = clamp_to_max(&img, 4, 4, 8);
        assert_eq!((w, h), (4, 4));
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
        assert_eq!(out.len(), img.len());
    }

    #[test]
    fn clamp_downscales_oversized_preserving_aspect() {
        let (w, h) = (20u32, 10u32);
        let img = vec![7u8; (w * h * 4) as usize];
        let (out, ow, oh) = clamp_to_max(&img, w, h, 8);
        assert!(ow <= 8 && oh <= 8, "got {ow}x{oh}");
        assert_eq!((ow, oh), (8, 4)); // 2:1 aspect preserved
        assert_eq!(out.len(), (ow * oh * 4) as usize);
    }

    #[test]
    fn original_mode_is_one_to_one_centered() {
        let view = ViewTransform {
            mode: crate::ScaleMode::Original,
            ..Default::default()
        };
        // Image exactly the screen size -> full-screen quad (top-left at -1,+1).
        let v = quad_vertices(&view, 100, 100, 100, 100, 0);
        assert!((v[0].pos[0] + 1.0).abs() < 1e-5, "x0 = {}", v[0].pos[0]);
        assert!((v[0].pos[1] - 1.0).abs() < 1e-5, "y_top = {}", v[0].pos[1]);

        // Image half the screen -> centered, top-left at -0.5,+0.5.
        let v = quad_vertices(&view, 50, 50, 100, 100, 0);
        assert!((v[0].pos[0] + 0.5).abs() < 1e-5, "x0 = {}", v[0].pos[0]);
        assert!((v[0].pos[1] - 0.5).abs() < 1e-5, "y_top = {}", v[0].pos[1]);
    }
}
