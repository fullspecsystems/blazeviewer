//! `UploadStrategy` — the swappable CPU→GPU texture-upload seam (plan §3.1).
//!
//! v1 stages pixels through a mapped buffer and `copy_buffer_to_texture` — the
//! path the upload spike measured at ~48 GB/s (3.4× the 120 Hz budget). We
//! deliberately do **not** use `queue.write_texture` (the documented trap:
//! 60–75 fps on large frames). A recycled staging-buffer ring and a CUDA
//! zero-copy backend are future alternatives behind this same trait.

use std::iter;

/// How decoded RGBA8 pixels reach a GPU texture. Implementors copy `rgba`
/// (`w*h*4`, tightly packed) into `tex` at origin (0,0). `tex` must be created
/// with `COPY_DST` usage and an `Rgba8*` format.
pub trait UploadStrategy {
    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        tex: &wgpu::Texture,
        rgba: &[u8],
        w: u32,
        h: u32,
    );
    /// Stable label for logs / A/B benchmarks.
    fn name(&self) -> &'static str;
}

/// v1: one mapped staging buffer per upload + `copy_buffer_to_texture`.
///
/// Correct and on the measured fast path; it allocates a staging buffer per
/// call. Uploads happen during prefetch (never on the keypress frame), so that
/// allocation is off the hot path. The zero-per-upload recycled-ring variant is
/// a drop-in replacement behind [`UploadStrategy`] (plan §3.1).
#[derive(Default)]
pub struct StagingUpload;

impl StagingUpload {
    pub fn new() -> Self {
        Self
    }
}

impl UploadStrategy for StagingUpload {
    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        tex: &wgpu::Texture,
        rgba: &[u8],
        w: u32,
        h: u32,
    ) {
        // `copy_buffer_to_texture` requires each row offset to be 256-aligned, so
        // the staging buffer holds padded rows even though `rgba` is tight.
        let unpadded = w * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded = unpadded.div_ceil(align) * align;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("staging-upload"),
            size: padded as u64 * h as u64,
            usage: wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: true,
        });
        {
            let mut view = buffer.slice(..).get_mapped_range_mut();
            if padded == unpadded {
                view[..rgba.len()].copy_from_slice(rgba);
            } else {
                let (u, p) = (unpadded as usize, padded as usize);
                for row in 0..h as usize {
                    view[row * p..row * p + u].copy_from_slice(&rgba[row * u..row * u + u]);
                }
            }
        }
        buffer.unmap();

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("staging-copy"),
        });
        encoder.copy_buffer_to_texture(
            wgpu::ImageCopyBuffer {
                buffer: &buffer,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            wgpu::ImageCopyTexture {
                texture: tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        // The submission keeps `buffer` alive until the copy completes, after
        // which our handle drops and the GPU buffer is freed.
        queue.submit(iter::once(encoder.finish()));
    }

    fn name(&self) -> &'static str {
        "staging-ring"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Upload a known pattern through the staging path and read it back; it must
    /// survive byte-for-byte. Uses a width whose row stride (100*4 = 400) is NOT
    /// 256-aligned, so the padding path is exercised — the bug the simpler render
    /// test can't catch (its 1600-wide rows are already aligned).
    #[test]
    fn staging_upload_round_trips_unaligned_width() {
        pollster::block_on(round_trip());
    }

    async fn round_trip() {
        let (w, h) = (100u32, 10u32);
        let mut src = vec![0u8; (w * h * 4) as usize];
        for (i, px) in src.chunks_exact_mut(4).enumerate() {
            px.copy_from_slice(&[
                (i % 256) as u8,
                (i / 7 % 256) as u8,
                (i / 13 % 256) as u8,
                255,
            ]);
        }

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12 | wgpu::Backends::VULKAN,
            ..Default::default()
        });
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
            })
            .await
            .expect("no GPU adapter");
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default(), None)
            .await
            .expect("request device");

        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rt-tex"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        StagingUpload::new().upload(&device, &queue, &tex, &src, w, h);

        // Read the texture back (padded rows) and reconstruct it tightly packed.
        let unpadded = w * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded = unpadded.div_ceil(align) * align;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rt-readback"),
            size: padded as u64 * h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("rt-copy"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &readback,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(iter::once(encoder.finish()));

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.poll(wgpu::Maintain::Wait);
        rx.recv().expect("map channel").expect("map readback");
        let mapped = slice.get_mapped_range();
        let mut got = Vec::with_capacity(src.len());
        for row in 0..h {
            let start = (row * padded) as usize;
            got.extend_from_slice(&mapped[start..start + unpadded as usize]);
        }
        drop(mapped);
        readback.unmap();

        assert_eq!(got, src, "staging upload corrupted pixels");
    }
}
