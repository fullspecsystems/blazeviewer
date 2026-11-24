//! The live 3D view's draw (task #153): a lit triangle soup with a depth buffer, drawn into
//! the fp16 scRGB intermediate **instead of** the photo quad, so the overlays, the tone-map and
//! the HDR/EDR present path all apply unchanged.
//!
//! The geometry and camera come from `pb-mesh` (by way of the core); this module only knows
//! vertices and a per-frame [`MeshFrame`]. The shading is the CPU still's (`pb-mesh::raster`),
//! moved to the GPU: flat normals, flipped towards the camera, under a key, a fill and a sky
//! light in linear light. That is what makes swapping the still for the live view invisible.
//!
//! Flat normals come from screen-space derivatives of the world position, so a vertex needs
//! no normal: 16 bytes per corner (position + sRGB colour).
//!
//! # Antialiasing: 4× MSAA, sized to fit
//!
//! Two passes per frame:
//!
//! 1. **The mesh**, into a 4× multisampled target with a transparent clear, resolved into a
//!    single-sample texture. The resolve averages the samples, so an edge pixel comes out
//!    premultiplied by its coverage: true geometric antialiasing, not a post-process blur.
//! 2. **The composite**: clear the intermediate to the letterbox, then blend the resolved
//!    texture over it (premultiplied), lifting the colour by the scene's SDR scale exactly as
//!    the photo path does. The letterbox itself is never lifted, matching the photo path.
//!
//! The MSAA attachments are kept compact: **8-bit sRGB colour** (the CPU still's own precision;
//! the hardware encodes the shader's linear output, and the resolve averages in linear light)
//! and **16-bit depth** (the depth range only spans the model's bounding sphere, so 16 bits
//! resolve a few hundred-thousandths of its radius). That is 24 bytes per pixel at 4×: ~354 MB
//! for a 5K window. Over [`MSAA_BUDGET_BYTES`] the view renders at 1× instead (refuse before
//! reserve). 4× is the only multisample count WebGPU guarantees on every backend for these
//! formats, so there is no 2× rung without adapter-specific features. `PB_MESH_MSAA=1` or `=4`
//! forces either, for A/B.

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::gpu::INTERMEDIATE_FORMAT;

/// The mesh's colour target: 8-bit sRGB, the CPU still's precision.
const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
/// The mesh's depth: ample over a bounding sphere.
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth16Unorm;
/// The antialiased sample count; WebGPU guarantees 4× for both formats.
pub(crate) const MSAA_SAMPLES: u32 = 4;
/// Bytes per pixel per sample: colour + depth.
const BYTES_PER_SAMPLE: u64 = 4 + 2;
/// The most the multisampled attachments may take before the view falls back to 1×.
pub(crate) const MSAA_BUDGET_BYTES: u64 = 512 << 20;

/// The sample count for a `width`×`height` target under `budget` bytes: 4× when it fits, else 1×.
pub(crate) fn samples_within_budget(width: u32, height: u32, budget: u64) -> u32 {
    let bytes = width as u64 * height as u64 * MSAA_SAMPLES as u64 * BYTES_PER_SAMPLE;
    if bytes <= budget {
        MSAA_SAMPLES
    } else {
        1
    }
}

/// One corner of the live view's triangle soup: a world-space position (Z-up) and the
/// triangle's sRGB colour (alpha ignored).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct MeshVertex {
    pub pos: [f32; 3],
    pub color: [u8; 4],
}

/// One frame of the live view, computed for the **content area** (the surface below the
/// top inset). The renderer maps it onto the surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshFrame {
    /// World → clip, column-major; depth in `[0, 1]`.
    pub view_proj: [[f32; 4]; 4],
    /// Camera position, world space.
    pub eye: [f32; 3],
    /// Unit directions towards the key and fill lights, world space.
    pub key_dir: [f32; 3],
    pub fill_dir: [f32; 3],
    /// `[ambient, sky, key, fill]` strengths.
    pub lighting: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    view_proj: [[f32; 4]; 4],
    eye: [f32; 4],
    key: [f32; 4],
    fill: [f32; 4],
    light: [f32; 4],
    /// `.x` = the scene's output scale (SDR white on an HDR surface; else 1).
    scale: [f32; 4],
    /// Content-area NDC → surface NDC: `(sx, sy, ox, oy)`.
    ndc: [f32; 4],
}

/// Shared by both shaders (one uniform buffer feeds both passes).
const UNIFORMS_WGSL: &str = r#"
struct U {
    view_proj: mat4x4<f32>,
    eye: vec4<f32>,
    key: vec4<f32>,
    fill: vec4<f32>,
    light: vec4<f32>,
    scale: vec4<f32>,
    ndc: vec4<f32>,
};
@group(0) @binding(0) var<uniform> u: U;
"#;

const MESH_WGSL: &str = r#"
struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) @interpolate(flat) color: vec3<f32>,
};

@vertex
fn vs_mesh(@location(0) p: vec3<f32>, @location(1) c: vec4<f32>) -> VOut {
    var clip = u.view_proj * vec4<f32>(p, 1.0);
    // The frame was computed for the content area; slide it into place on the surface.
    clip.x = clip.x * u.ndc.x + u.ndc.z * clip.w;
    clip.y = clip.y * u.ndc.y + u.ndc.w * clip.w;
    var o: VOut;
    o.pos = clip;
    o.world = p;
    o.color = c.rgb;
    return o;
}

fn srgb_to_linear(x: f32) -> f32 {
    if (x <= 0.04045) {
        return x / 12.92;
    }
    return pow((x + 0.055) / 1.055, 2.4);
}

@fragment
fn fs_mesh(i: VOut) -> @location(0) vec4<f32> {
    // Flat normal from the triangle's own screen-space derivatives (pb-mesh::raster recomputes
    // normals from the geometry too), then flipped to face the camera: two-sided lighting.
    let to_eye = u.eye.xyz - i.world;
    var n = cross(dpdx(i.world), dpdy(i.world));
    let len = length(n);
    if (len > 0.0) {
        n = n / len;
    } else {
        n = normalize(to_eye);
    }
    if (dot(n, to_eye) < 0.0) {
        n = -n;
    }
    let sky = 0.5 + 0.5 * n.z;
    let light = u.light.x + u.light.y * sky
        + u.light.z * max(dot(n, u.key.xyz), 0.0)
        + u.light.w * max(dot(n, u.fill.xyz), 0.0);
    let albedo = vec3<f32>(srgb_to_linear(i.color.r), srgb_to_linear(i.color.g), srgb_to_linear(i.color.b));
    // Linear, clamped exactly like the still's 8-bit encode; the sRGB target encodes it, and
    // the composite applies the scene scale.
    return vec4<f32>(min(albedo * light, vec3<f32>(1.0)), 1.0);
}
"#;

const COMPOSITE_WGSL: &str = r#"
@group(0) @binding(1) var src: texture_2d<f32>;

@vertex
fn vs_full(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

@fragment
fn fs_composite(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    // Same size as the intermediate: one texel per pixel. Premultiplied by coverage.
    let c = textureLoad(src, vec2<i32>(p.xy), 0);
    return vec4<f32>(c.rgb * u.scale.x, c.a);
}
"#;

const ATTRS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Float32x3, 1 => Unorm8x4];

/// The size-dependent attachments, rebuilt when the intermediate's size changes.
struct Targets {
    width: u32,
    height: u32,
    samples: u32,
    /// The multisampled colour attachment; `None` at 1× (the mesh draws into `resolved`).
    msaa: Option<wgpu::TextureView>,
    depth: wgpu::TextureView,
    /// The single-sample, coverage-premultiplied result the composite reads.
    resolved: wgpu::TextureView,
    composite_bg: wgpu::BindGroup,
}

/// The GPU side of the live view: pipelines, the uploaded soup (chunked to the device's buffer
/// limit), the antialiasing attachments, and the frame to draw (if showing).
pub(crate) struct MeshPass {
    /// Indexed by "is multisampled": `[1×, 4×]`.
    mesh_pipelines: [wgpu::RenderPipeline; 2],
    composite_pipeline: wgpu::RenderPipeline,
    mesh_bg: wgpu::BindGroup,
    composite_bgl: wgpu::BindGroupLayout,
    uniforms: wgpu::Buffer,
    chunks: Vec<(wgpu::Buffer, u32)>,
    targets: Option<Targets>,
    /// `PB_MESH_MSAA=1|4`, read once: forces the sample count past the budget, for A/B.
    pub sample_override: Option<u32>,
    /// `Some` = draw the mesh instead of the photo this frame.
    pub frame: Option<MeshFrame>,
}

fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

impl MeshPass {
    pub fn new(device: &wgpu::Device) -> MeshPass {
        let mesh_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pb-mesh-shader"),
            source: wgpu::ShaderSource::Wgsl(format!("{UNIFORMS_WGSL}{MESH_WGSL}").into()),
        });
        let composite_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pb-mesh-composite-shader"),
            source: wgpu::ShaderSource::Wgsl(format!("{UNIFORMS_WGSL}{COMPOSITE_WGSL}").into()),
        });
        let mesh_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pb-mesh-bgl"),
            entries: &[uniform_entry(0)],
        });
        let composite_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pb-mesh-composite-bgl"),
            entries: &[
                uniform_entry(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let mesh_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pb-mesh-layout"),
            bind_group_layouts: &[&mesh_bgl],
            push_constant_ranges: &[],
        });
        let mesh_pipeline = |samples: u32| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("pb-mesh-pipeline"),
                layout: Some(&mesh_layout),
                vertex: wgpu::VertexState {
                    module: &mesh_module,
                    entry_point: "vs_mesh",
                    compilation_options: Default::default(),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<MeshVertex>() as wgpu::BufferAddress,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &ATTRS,
                    }],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &mesh_module,
                    entry_point: "fs_mesh",
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: COLOR_FORMAT,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                // Two-sided: no culling (open or inside-out meshes still read, as in the still).
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: true,
                    depth_compare: wgpu::CompareFunction::Less,
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState {
                    count: samples,
                    ..Default::default()
                },
                multiview: None,
                cache: None,
            })
        };
        let mesh_pipelines = [mesh_pipeline(1), mesh_pipeline(MSAA_SAMPLES)];
        let composite_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pb-mesh-composite-layout"),
            bind_group_layouts: &[&composite_bgl],
            push_constant_ranges: &[],
        });
        let composite_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pb-mesh-composite-pipeline"),
            layout: Some(&composite_layout),
            vertex: wgpu::VertexState {
                module: &composite_module,
                entry_point: "vs_full",
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &composite_module,
                entry_point: "fs_composite",
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: INTERMEDIATE_FORMAT,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pb-mesh-uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mesh_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pb-mesh-bg"),
            layout: &mesh_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            }],
        });
        let sample_override = match std::env::var("PB_MESH_MSAA").ok().as_deref() {
            Some("1") => Some(1),
            Some("4") => Some(MSAA_SAMPLES),
            _ => None,
        };
        MeshPass {
            mesh_pipelines,
            composite_pipeline,
            mesh_bg,
            composite_bgl,
            uniforms,
            chunks: Vec::new(),
            targets: None,
            sample_override,
            frame: None,
        }
    }

    /// Replace the uploaded soup. Split into buffers no larger than the device allows (whole
    /// triangles per buffer). Returns false, uploading nothing, if `vertices` isn't whole
    /// triangles.
    pub fn upload(&mut self, device: &wgpu::Device, vertices: &[MeshVertex]) -> bool {
        self.chunks.clear();
        self.frame = None;
        if !vertices.len().is_multiple_of(3) {
            return false;
        }
        let stride = std::mem::size_of::<MeshVertex>() as u64;
        let max = device.limits().max_buffer_size.max(stride * 3);
        let per_chunk = ((max / stride) / 3 * 3).max(3) as usize;
        for chunk in vertices.chunks(per_chunk) {
            let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("pb-mesh-vertices"),
                contents: bytemuck::cast_slice(chunk),
                usage: wgpu::BufferUsages::VERTEX,
            });
            self.chunks.push((buf, chunk.len() as u32));
        }
        true
    }

    pub fn has_geometry(&self) -> bool {
        !self.chunks.is_empty()
    }

    /// Drop the uploaded soup and the attachments, and stop showing.
    pub fn clear(&mut self) {
        self.chunks.clear();
        self.frame = None;
        self.targets = None;
    }

    /// The sample count the attachments are (or would be) built at, for this size.
    pub fn samples_for(&self, width: u32, height: u32) -> u32 {
        self.sample_override
            .unwrap_or_else(|| samples_within_budget(width, height, MSAA_BUDGET_BYTES))
    }

    /// (Re)build the attachments for a `width`×`height` intermediate when anything changed.
    fn ensure_targets(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        let samples = self.samples_for(width, height);
        if self
            .targets
            .as_ref()
            .is_some_and(|t| (t.width, t.height, t.samples) == (width, height, samples))
        {
            return;
        }
        // Free the old attachments before allocating the new ones (a resize must not briefly
        // hold both).
        self.targets = None;
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let make = |label, format, sample_count, usage| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size,
                    mip_level_count: 1,
                    sample_count,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor::default())
        };
        let attach = wgpu::TextureUsages::RENDER_ATTACHMENT;
        let msaa = (samples > 1).then(|| make("pb-mesh-msaa", COLOR_FORMAT, samples, attach));
        let depth = make("pb-mesh-depth", DEPTH_FORMAT, samples, attach);
        let resolved = make(
            "pb-mesh-resolved",
            COLOR_FORMAT,
            1,
            attach | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let composite_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pb-mesh-composite-bg"),
            layout: &self.composite_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&resolved),
                },
            ],
        });
        self.targets = Some(Targets {
            width,
            height,
            samples,
            msaa,
            depth,
            resolved,
            composite_bg,
        });
    }

    /// Draw the shown frame into `target` (the `width`×`height` intermediate), which is cleared
    /// to `clear` first. Returns false (drawing nothing) when there is no frame or no geometry,
    /// so the caller draws the photo instead.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        (width, height): (u32, u32),
        top_inset: u32,
        clear: wgpu::Color,
        scale: f32,
    ) -> bool {
        let Some(frame) = self.frame else {
            return false;
        };
        if self.chunks.is_empty() || width == 0 || height == 0 {
            return false;
        }
        self.ensure_targets(device, width, height);
        let (sh, top) = (
            height as f32,
            top_inset.min(height.saturating_sub(1)) as f32,
        );
        let u = Uniforms {
            view_proj: frame.view_proj,
            eye: [frame.eye[0], frame.eye[1], frame.eye[2], 0.0],
            key: [frame.key_dir[0], frame.key_dir[1], frame.key_dir[2], 0.0],
            fill: [frame.fill_dir[0], frame.fill_dir[1], frame.fill_dir[2], 0.0],
            light: frame.lighting,
            scale: [scale, 0.0, 0.0, 0.0],
            // Content y ∈ [−1, 1] lands on the surface between the inset and the bottom.
            ndc: [1.0, (sh - top) / sh, 0.0, -top / sh],
        };
        queue.write_buffer(&self.uniforms, 0, bytemuck::bytes_of(&u));
        let t = self.targets.as_ref().expect("built above");

        // Pass 1: the mesh, on a transparent clear, multisampled and resolved (or straight into
        // `resolved` at 1×).
        {
            let (view, resolve_target, store) = match &t.msaa {
                Some(msaa) => (msaa, Some(&t.resolved), wgpu::StoreOp::Discard),
                None => (&t.resolved, None, wgpu::StoreOp::Store),
            };
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene-mesh"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &t.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.mesh_pipelines[usize::from(t.msaa.is_some())]);
            rp.set_bind_group(0, &self.mesh_bg, &[]);
            for (buf, count) in &self.chunks {
                rp.set_vertex_buffer(0, buf.slice(..));
                rp.draw(0..*count, 0..1);
            }
        }

        // Pass 2: the letterbox, then the antialiased mesh over it.
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene-mesh-composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
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
            rp.set_pipeline(&self.composite_pipeline);
            rp.set_bind_group(0, &t.composite_bg, &[]);
            rp.draw(0..3, 0..1);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 64;
    const H: u32 = 64;

    /// Draw `vertices` with `frame` into a fresh fp16 intermediate at `samples` (forced) and read
    /// it back as linear RGBA floats.
    fn draw_at(
        vertices: &[MeshVertex],
        frame: MeshFrame,
        top_inset: u32,
        samples: u32,
    ) -> Vec<[f32; 4]> {
        draw_sized(vertices, frame, top_inset, samples, (W, H))
    }

    /// [`draw_at`] for any size (`width` a multiple of 32, so the readback rows stay 256-aligned).
    fn draw_sized(
        vertices: &[MeshVertex],
        frame: MeshFrame,
        top_inset: u32,
        samples: u32,
        (w, h): (u32, u32),
    ) -> Vec<[f32; 4]> {
        let _gpu = crate::gpu_test_lock();
        let (device, queue) = crate::gpu::test_device();
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test-intermediate"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: INTERMEDIATE_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let mut pass = MeshPass::new(&device);
        pass.sample_override = Some(samples);
        assert!(pass.upload(&device, vertices));
        pass.frame = Some(frame);
        let mut enc = device.create_command_encoder(&Default::default());
        let clear = wgpu::Color {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        };
        assert!(pass.draw(
            &device,
            &queue,
            &mut enc,
            &view,
            (w, h),
            top_inset,
            clear,
            1.0
        ));
        let row = w * 8; // Rgba16Float; 512 is already 256-aligned
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        enc.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::ImageCopyBuffer {
                buffer: &buf,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: None,
                },
            },
            target.size(),
        );
        queue.submit([enc.finish()]);
        buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device.poll(wgpu::Maintain::Wait);
        let data = buf.slice(..).get_mapped_range();
        data.chunks_exact(8)
            .map(|px| {
                std::array::from_fn(|c| {
                    half::f16::from_le_bytes([px[c * 2], px[c * 2 + 1]]).to_f32()
                })
            })
            .collect()
    }

    fn draw(vertices: &[MeshVertex], frame: MeshFrame, top_inset: u32) -> Vec<[f32; 4]> {
        draw_at(vertices, frame, top_inset, MSAA_SAMPLES)
    }

    fn corner(x: f32, y: f32, z: f32, color: [u8; 4]) -> MeshVertex {
        MeshVertex {
            pos: [x, y, z],
            color,
        }
    }

    /// A quad facing the camera at depth `z`, with an identity projection.
    fn quad(color: [u8; 4], z: f32) -> Vec<MeshVertex> {
        let v = |x, y| corner(x, y, z, color);
        vec![
            v(-0.5, -0.5),
            v(0.5, -0.5),
            v(0.5, 0.5),
            v(-0.5, -0.5),
            v(0.5, 0.5),
            v(-0.5, 0.5),
        ]
    }

    /// A white triangle whose hypotenuse runs diagonally across the frame, so its edge cuts
    /// through many pixels at every fraction of coverage.
    fn diagonal() -> Vec<MeshVertex> {
        let w = [255, 255, 255, 255];
        vec![
            corner(-0.9, -0.9, 0.5, w),
            corner(0.9, -0.9, 0.5, w),
            corner(0.9, 0.7, 0.5, w),
        ]
    }

    fn frame() -> MeshFrame {
        MeshFrame {
            view_proj: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
            eye: [0.0, 0.0, 10.0],
            key_dir: [0.0, 0.0, 1.0],
            fill_dir: [0.0, 0.0, 1.0],
            lighting: [0.13, 0.10, 0.58, 0.22],
        }
    }

    fn srgb_to_linear(c: u8) -> f32 {
        let x = c as f32 / 255.0;
        if x <= 0.04045 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        }
    }

    #[test]
    fn a_lit_face_matches_the_cpu_stills_formula() {
        let px = draw(&quad([200, 204, 210, 255], 0.5), frame(), 0);
        let centre = px[(H / 2 * W + W / 2) as usize];
        // Facing the camera, both lights head-on, sky fully up (n = +Z): the CPU still's
        // `ambient + sky + key + fill`, clamped to 1 (then through the 8-bit sRGB target).
        let light: f32 = 0.13 + 0.10 + 0.58 + 0.22;
        for (c, base) in [200u8, 204, 210].into_iter().enumerate() {
            let want = (srgb_to_linear(base) * light).min(1.0);
            assert!(
                (centre[c] - want).abs() < 0.01,
                "channel {c}: {} vs {want}",
                centre[c]
            );
        }
        assert!((centre[3] - 1.0).abs() < 1e-3);
        assert_eq!(
            px[0],
            [0.0, 0.0, 0.0, 1.0],
            "outside the model is the clear colour"
        );
    }

    #[test]
    fn nearer_geometry_wins_regardless_of_draw_order() {
        // Red at depth 0.3 drawn first, blue at 0.7 drawn second: the depth test keeps red.
        let mut v = quad([255, 0, 0, 255], 0.3);
        v.extend(quad([0, 0, 255, 255], 0.7));
        let px = draw(&v, frame(), 0);
        let c = px[(H / 2 * W + W / 2) as usize];
        assert!(c[0] > 0.5 && c[2] < 0.01, "{c:?}");
    }

    #[test]
    fn the_frame_lands_below_the_top_inset() {
        // With a 32 px inset the content area is the bottom half: the quad (centred in content)
        // must sit around y = 48, and the band under the inset stays clear.
        let px = draw(&quad([255, 255, 255, 255], 0.5), frame(), 32);
        assert!(
            px[(48 * W + W / 2) as usize][0] > 0.5,
            "content centre is drawn"
        );
        assert_eq!(
            px[(4 * W + W / 2) as usize][0],
            0.0,
            "under the bar stays clear"
        );
    }

    /// The number of pixels strictly between the background and the fully covered value.
    fn partial_pixels(px: &[[f32; 4]]) -> usize {
        let full = px.iter().map(|p| p[0]).fold(0.0f32, f32::max);
        px.iter()
            .filter(|p| p[0] > full * 0.05 && p[0] < full * 0.95)
            .count()
    }

    #[test]
    fn edges_are_antialiased_at_4x() {
        let px = draw_at(&diagonal(), frame(), 0, MSAA_SAMPLES);
        // A 64-px diagonal edge crosses ~60 pixels; with 4 samples most of them land partway.
        assert!(partial_pixels(&px) >= 30, "{} partial", partial_pixels(&px));
    }

    #[test]
    fn the_1x_fallback_has_hard_edges() {
        let px = draw_at(&diagonal(), frame(), 0, 1);
        assert_eq!(partial_pixels(&px), 0);
        assert!(px.iter().any(|p| p[0] > 0.5), "still draws the triangle");
    }

    #[test]
    fn the_budget_keeps_4x_through_5k_and_drops_to_1x_beyond() {
        assert_eq!(samples_within_budget(2560, 1682, MSAA_BUDGET_BYTES), 4);
        assert_eq!(samples_within_budget(5120, 2880, MSAA_BUDGET_BYTES), 4);
        assert_eq!(samples_within_budget(7680, 4320, MSAA_BUDGET_BYTES), 1);
        assert_eq!(samples_within_budget(64, 64, 0), 1);
    }

    /// Dev A/B, not run by default: render a real model, turned off its home pose, at 1× and 4×
    /// and write both as PPMs at native resolution, to eyeball the antialiasing.
    ///
    /// `PB_MESH_MODEL=part.stl PB_MESH_OUT=dir cargo test -p pb-render aa_report -- --ignored`
    #[test]
    #[ignore]
    fn aa_report() {
        let (Some(model), Some(out)) = (
            std::env::var_os("PB_MESH_MODEL"),
            std::env::var_os("PB_MESH_OUT"),
        ) else {
            eprintln!("set PB_MESH_MODEL and PB_MESH_OUT");
            return;
        };
        let (w, h) = (1024u32, 768u32);
        let bytes = std::fs::read(&model).expect("read model");
        let ext = std::path::Path::new(&model)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let format = pb_mesh::ModelFormat::from_extension(ext).expect("a model extension");
        let m = pb_mesh::parse(format, &bytes, &pb_mesh::Limits::default()).expect("parse");
        let mut view = pb_mesh::LiveView::new(&m).expect("drawable");
        view.orbit_by(180.0, -40.0, [w as f32, h as f32]);
        let f = view.frame([w as f32, h as f32]);
        let frame = MeshFrame {
            view_proj: f.view_proj,
            eye: f.eye,
            key_dir: f.key_dir,
            fill_dir: f.fill_dir,
            lighting: f.lighting,
        };
        let vertices: Vec<MeshVertex> = pb_mesh::live::corners(&m)
            .map(|(pos, [r, g, b])| MeshVertex {
                pos,
                color: [r, g, b, 255],
            })
            .collect();
        let encode = |l: f32| {
            let l = l.clamp(0.0, 1.0);
            let s = if l <= 0.003_130_8 {
                l * 12.92
            } else {
                1.055 * l.powf(1.0 / 2.4) - 0.055
            };
            (s * 255.0).round() as u8
        };
        for samples in [1, MSAA_SAMPLES] {
            let px = draw_sized(&vertices, frame, 0, samples, (w, h));
            let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
            ppm.extend(
                px.iter()
                    .flat_map(|p| [encode(p[0]), encode(p[1]), encode(p[2])]),
            );
            let path = std::path::Path::new(&out).join(format!("aa-{samples}x.ppm"));
            std::fs::write(&path, ppm).expect("write ppm");
            eprintln!("wrote {}", path.display());
        }
    }

    #[test]
    fn upload_refuses_a_partial_triangle() {
        let _gpu = crate::gpu_test_lock();
        let (device, _queue) = crate::gpu::test_device();
        let mut pass = MeshPass::new(&device);
        let mut v = quad([0, 0, 0, 255], 0.5);
        v.pop();
        assert!(!pass.upload(&device, &v));
        assert!(!pass.has_geometry());
    }
}
