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

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::gpu::INTERMEDIATE_FORMAT;

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

const SHADER: &str = r#"
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
    // Clamped exactly like the still's 8-bit encode, then lifted like any SDR scene content.
    return vec4<f32>(min(albedo * light, vec3<f32>(1.0)) * u.scale.x, 1.0);
}
"#;

const ATTRS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Float32x3, 1 => Unorm8x4];

/// The GPU side of the live view: pipeline, the uploaded soup (chunked to the device's buffer
/// limit), a depth buffer sized to the intermediate, and the frame to draw (if showing).
pub(crate) struct MeshPass {
    pipeline: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    chunks: Vec<(wgpu::Buffer, u32)>,
    depth: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// `Some` = draw the mesh instead of the photo this frame.
    pub frame: Option<MeshFrame>,
}

impl MeshPass {
    pub fn new(device: &wgpu::Device) -> MeshPass {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pb-mesh-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pb-mesh-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pb-mesh-layout"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pb-mesh-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: "vs_mesh",
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<MeshVertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &ATTRS,
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: "fs_mesh",
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: INTERMEDIATE_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            // Two-sided: no culling (open or inside-out meshes still read, as in the still).
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::Less,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
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
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pb-mesh-bg"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            }],
        });
        MeshPass {
            pipeline,
            uniforms,
            bind_group,
            chunks: Vec::new(),
            depth: None,
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

    /// Drop the uploaded soup and stop showing it.
    pub fn clear(&mut self) {
        self.chunks.clear();
        self.frame = None;
        self.depth = None;
    }

    /// Draw the shown frame into `target` (the `width`×`height` intermediate), clearing it to
    /// `clear` first. Returns false (drawing nothing) when there is no frame or no geometry,
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
        let stale = self
            .depth
            .as_ref()
            .is_none_or(|(t, _)| t.width() != width || t.height() != height);
        if stale {
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("pb-mesh-depth"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Depth32Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            self.depth = Some((tex, view));
        }
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
        let depth_view = &self.depth.as_ref().expect("created above").1;
        let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("scene-mesh"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Discard,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        rp.set_pipeline(&self.pipeline);
        rp.set_bind_group(0, &self.bind_group, &[]);
        for (buf, count) in &self.chunks {
            rp.set_vertex_buffer(0, buf.slice(..));
            rp.draw(0..*count, 0..1);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 64;
    const H: u32 = 64;

    /// Draw `vertices` with `frame` into a fresh fp16 intermediate and read it back as linear
    /// RGBA floats.
    fn draw(vertices: &[MeshVertex], frame: MeshFrame, top_inset: u32) -> Vec<[f32; 4]> {
        let _gpu = crate::gpu_test_lock();
        let (device, queue) = crate::gpu::test_device();
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test-intermediate"),
            size: wgpu::Extent3d {
                width: W,
                height: H,
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
            (W, H),
            top_inset,
            clear,
            1.0
        ));
        let row = W * 8; // Rgba16Float; 512 is already 256-aligned
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (row * H) as u64,
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

    /// A screen-filling quad facing the camera at depth 0.5, with an identity projection.
    fn quad(color: [u8; 4], z: f32) -> Vec<MeshVertex> {
        let v = |x: f32, y: f32| MeshVertex {
            pos: [x, y, z],
            color,
        };
        vec![
            v(-0.5, -0.5),
            v(0.5, -0.5),
            v(0.5, 0.5),
            v(-0.5, -0.5),
            v(0.5, 0.5),
            v(-0.5, 0.5),
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
        // `ambient + sky + key + fill`, clamped to 1.
        let light: f32 = 0.13 + 0.10 + 0.58 + 0.22;
        for (c, base) in [200u8, 204, 210].into_iter().enumerate() {
            let want = (srgb_to_linear(base) * light).min(1.0);
            assert!(
                (centre[c] - want).abs() < 0.01,
                "channel {c}: {} vs {want}",
                centre[c]
            );
        }
        assert_eq!(centre[3], 1.0);
        assert_eq!(
            px[0],
            [0.0, 0.0, 0.0, 1.0],
            "outside the quad is the clear colour"
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
