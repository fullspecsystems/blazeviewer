//! wgpu presenter (DX12 backend on Windows) + a headless render for golden tests.
//!
//! One image is drawn as a textured quad, letterboxed to the screen via
//! `fit_rect` (no crop), over a dark clear color. `WgpuRenderer` presents to a
//! window surface; `render_offscreen` renders to a buffer for tests.

use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::upload::{StagingUpload, UploadStrategy};
use crate::{RenderError, Renderer, ViewTransform};

/// Background (letterbox) color, straight RGBA8.
pub const LETTERBOX: [u8; 4] = [10, 10, 12, 255];

const SHADER: &str = r#"
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

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
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

/// The four corners of the image quad in clip space, placed and UV-mapped by the
/// per-photo `ViewTransform` (scaling mode + rotation + zoom + pan).
fn quad_vertices(
    view: &ViewTransform,
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    let p = view.placement(img_w, img_h, screen_w, screen_h);
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

fn clear_color(rgba: [u8; 4]) -> wgpu::Color {
    wgpu::Color {
        r: rgba[0] as f64 / 255.0,
        g: rgba[1] as f64 / 255.0,
        b: rgba[2] as f64 / 255.0,
        a: rgba[3] as f64 / 255.0,
    }
}

fn build_pipelines(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
) -> (
    wgpu::RenderPipeline,
    wgpu::RenderPipeline,
    wgpu::BindGroupLayout,
) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("pb-shader"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("img-bgl"),
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
        ],
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("pb-layout"),
        bind_group_layouts: &[&bgl],
        push_constant_ranges: &[],
    });
    // Two pipelines, same shader: the photo is opaque (REPLACE); the corner
    // info overlay is alpha-blended over it.
    let make = |blend: wgpu::BlendState, label: &str| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &ATTRS,
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(blend),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        })
    };
    let photo = make(wgpu::BlendState::REPLACE, "pb-photo-pipeline");
    let overlay = make(wgpu::BlendState::ALPHA_BLENDING, "pb-overlay-pipeline");
    (photo, overlay, bgl)
}

/// The four corners of the overlay panel quad: `panel_w`×`panel_h` pixels, placed
/// `margin` px in from the bottom-right of the screen.
fn overlay_quad_vertices(
    panel_w: u32,
    panel_h: u32,
    screen_w: u32,
    screen_h: u32,
    margin: u32,
) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    let m = margin as f32;
    let x1 = sw - m;
    let x0 = x1 - panel_w as f32;
    let y1 = sh - m;
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

fn upload_image(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    bgl: &wgpu::BindGroupLayout,
    uploader: &mut dyn UploadStrategy,
    image: &[u8],
    img_w: u32,
    img_h: u32,
) -> wgpu::BindGroup {
    // Downscale anything beyond the GPU's max texture dimension so huge images
    // (e.g. panoramas) upload instead of failing device validation.
    let max = device.limits().max_texture_dimension_2d;
    let (image, img_w, img_h) = clamp_to_max(image, img_w, img_h, max);
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
        format: wgpu::TextureFormat::Rgba8Unorm,
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
        contents: bytemuck::cast_slice(&quad_vertices(view, img_w, img_h, screen_w, screen_h)),
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

fn draw(
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
    vbuf: &wgpu::Buffer,
    ibuf: &wgpu::Buffer,
) {
    let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("scene"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(clear_color(LETTERBOX)),
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

fn instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12 | wgpu::Backends::VULKAN,
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
}

/// On-screen presenter for a window surface.
pub struct WgpuRenderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    overlay_pipeline: wgpu::RenderPipeline,
    bgl: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    vbuf: wgpu::Buffer,
    ibuf: wgpu::Buffer,
    img_w: u32,
    img_h: u32,
    /// Per-photo view transform (scaling mode + rotation + zoom + pan).
    view: ViewTransform,
    overlay: Option<OverlayDraw>,
    upload: Box<dyn UploadStrategy>,
    /// Resident texture ring (Phase 3). Empty until `reserve_ring`; each `Some`
    /// slot holds a pre-uploaded photo. `present_slot` selects which one draws.
    ring: Vec<Option<RingSlot>>,
    /// When `Some(i)`, `render` draws ring slot `i` instead of `bind_group`.
    present_idx: Option<usize>,
}

impl WgpuRenderer {
    /// Create a presenter for `target` (e.g. an `Arc<Window>`) and upload `image`.
    pub fn new(
        target: impl Into<wgpu::SurfaceTarget<'static>>,
        width: u32,
        height: u32,
        image: &[u8],
        img_w: u32,
        img_h: u32,
    ) -> Self {
        pollster::block_on(Self::new_async(target, width, height, image, img_w, img_h))
    }

    async fn new_async(
        target: impl Into<wgpu::SurfaceTarget<'static>>,
        width: u32,
        height: u32,
        image: &[u8],
        img_w: u32,
        img_h: u32,
    ) -> Self {
        let instance = instance();
        let surface = instance.create_surface(target).expect("create surface");
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
        // Pick a NON-sRGB surface: the JPEG's bytes are already sRGB-encoded, so
        // they should pass straight through to the monitor. An sRGB surface would
        // re-encode the shader output and wash the image out (the faded look).
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        // Mailbox = low latency, no tearing; fall back to Fifo if unsupported.
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
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

        let view = ViewTransform::default();
        let (pipeline, overlay_pipeline, bgl) = build_pipelines(&device, format);
        let mut upload: Box<dyn UploadStrategy> = Box::new(StagingUpload::new());
        let bind_group = upload_image(&device, &queue, &bgl, upload.as_mut(), image, img_w, img_h);
        let vbuf = vertex_buffer(&device, &view, img_w, img_h, config.width, config.height);
        let ibuf = index_buffer(&device);

        Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            overlay_pipeline,
            bgl,
            bind_group,
            vbuf,
            ibuf,
            img_w,
            img_h,
            view,
            overlay: None,
            upload,
            ring: Vec::new(),
            present_idx: None,
        }
    }

    /// The display refresh cap helper will live here in Phase 3; for now the app
    /// reads it from winit directly.
    pub fn present_mode(&self) -> wgpu::PresentMode {
        self.config.present_mode
    }

    /// Process completed GPU work and free dropped resources (the previous
    /// image's texture). Call once per frame so rapid navigation doesn't let GPU
    /// memory pile up. (Phase 3 replaces per-image textures with a reused ring.)
    pub fn poll(&self) {
        self.device.poll(wgpu::Maintain::Poll);
    }
}

impl Renderer for WgpuRenderer {
    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if width == self.config.width && height == self.config.height {
            return; // already correctly sized — avoid a redundant reconfigure
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        // Re-place the quad for the new viewport.
        self.queue.write_buffer(
            &self.vbuf,
            0,
            bytemuck::cast_slice(&quad_vertices(
                &self.view, self.img_w, self.img_h, width, height,
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
    }

    fn set_image(&mut self, rgba: &[u8], width: u32, height: u32) {
        self.bind_group = upload_image(
            &self.device,
            &self.queue,
            &self.bgl,
            self.upload.as_mut(),
            rgba,
            width,
            height,
        );
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
            )),
        );
    }

    fn set_overlay(&mut self, panel: Option<(&[u8], u32, u32)>, margin: u32) {
        self.overlay = match panel {
            Some((rgba, w, h)) => {
                let bind_group = upload_image(
                    &self.device,
                    &self.queue,
                    &self.bgl,
                    self.upload.as_mut(),
                    rgba,
                    w,
                    h,
                );
                let vbuf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("overlay-vbuf"),
                        contents: bytemuck::cast_slice(&overlay_quad_vertices(
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

    fn upload_slot(&mut self, slot: usize, rgba: &[u8], w: u32, h: u32) {
        if slot >= self.ring.len() {
            return;
        }
        let bind_group = upload_image(
            &self.device,
            &self.queue,
            &self.bgl,
            self.upload.as_mut(),
            rgba,
            w,
            h,
        );
        self.ring[slot] = Some(RingSlot { bind_group, w, h });
    }

    fn present_slot(&mut self, slot: usize) {
        let Some((w, h)) = self
            .ring
            .get(slot)
            .and_then(|s| s.as_ref())
            .map(|s| (s.w, s.h))
        else {
            return; // unknown / not-yet-uploaded slot: keep the current frame
        };
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
            )),
        );
    }

    fn render(&mut self) -> Result<(), RenderError> {
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            Err(wgpu::SurfaceError::Timeout) => return Ok(()),
            Err(wgpu::SurfaceError::OutOfMemory) => return Err(RenderError::OutOfMemory),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("present"),
            });
        // Draw the selected resident-ring slot if one is presented, else the
        // single image. A keypress rebinds via `present_idx` — no upload here.
        let bind_group = match self.present_idx {
            Some(i) => self
                .ring
                .get(i)
                .and_then(|s| s.as_ref())
                .map(|s| &s.bind_group)
                .unwrap_or(&self.bind_group),
            None => &self.bind_group,
        };
        draw(
            &mut encoder,
            &view,
            &self.pipeline,
            bind_group,
            &self.vbuf,
            &self.ibuf,
        );
        // A second pass loads the photo and alpha-blends the info panel on top.
        if let Some(ov) = &self.overlay {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
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
        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        Ok(())
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
    pollster::block_on(render_offscreen_async(
        image, img_w, img_h, screen_w, screen_h,
    ))
}

async fn render_offscreen_async(
    image: &[u8],
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
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
    let (pipeline, _overlay_pipeline, bgl) = build_pipelines(&device, format);
    let mut upload = StagingUpload::new();
    let bind_group = upload_image(&device, &queue, &bgl, &mut upload, image, img_w, img_h);
    let vbuf = vertex_buffer(
        &device,
        &ViewTransform::default(),
        img_w,
        img_h,
        screen_w,
        screen_h,
    );
    let ibuf = index_buffer(&device);

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
    draw(&mut encoder, &view, &pipeline, &bind_group, &vbuf, &ibuf);
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
        let v = quad_vertices(&view, 100, 100, 100, 100);
        assert!((v[0].pos[0] + 1.0).abs() < 1e-5, "x0 = {}", v[0].pos[0]);
        assert!((v[0].pos[1] - 1.0).abs() < 1e-5, "y_top = {}", v[0].pos[1]);

        // Image half the screen -> centered, top-left at -0.5,+0.5.
        let v = quad_vertices(&view, 50, 50, 100, 100);
        assert!((v[0].pos[0] + 0.5).abs() < 1e-5, "x0 = {}", v[0].pos[0]);
        assert!((v[0].pos[1] - 0.5).abs() < 1e-5, "y_top = {}", v[0].pos[1]);
    }
}
