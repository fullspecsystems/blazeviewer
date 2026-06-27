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
        // Keep each staging buffer within the device's max buffer size by copying
        // the image in horizontal row-bands. Normal (fit-sized) images are a
        // single band; only huge Original-mode/panorama images split.
        let padded = padded_row_bytes(w);
        let max_rows = (device.limits().max_buffer_size / padded.max(1) as u64).max(1);
        let rows_per_band = max_rows.min(h.max(1) as u64) as u32;
        copy_via_staging(device, queue, tex, rgba, w, h, rows_per_band);
    }

    fn name(&self) -> &'static str {
        "staging-ring"
    }
}

/// The 256-byte-aligned row stride for a `w`-pixel RGBA8 row, as
/// `copy_buffer_to_texture` requires.
fn padded_row_bytes(w: u32) -> u32 {
    let unpadded = w * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    unpadded.div_ceil(align) * align
}

/// Stage `rgba` (`w*h*4`, tight) into `tex` in horizontal bands of at most
/// `rows_per_band` rows — each through its own mapped buffer + a
/// `copy_buffer_to_texture` at the band's `y` origin. One encoder, one submit.
/// Banding bounds each staging buffer by the device's buffer-size limit so an
/// arbitrarily tall image uploads without exceeding it.
fn copy_via_staging(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    tex: &wgpu::Texture,
    rgba: &[u8],
    w: u32,
    h: u32,
    rows_per_band: u32,
) {
    let unpadded = w as usize * 4;
    let padded = padded_row_bytes(w);
    let rows_per_band = rows_per_band.max(1);
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("staging-copy"),
    });
    // The band buffers must outlive `encoder.finish()` / the submit.
    let mut buffers = Vec::new();
    let mut y = 0u32;
    while y < h {
        let band_h = rows_per_band.min(h - y);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("staging-upload"),
            size: padded as u64 * band_h as u64,
            usage: wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: true,
        });
        {
            let mut view = buffer.slice(..).get_mapped_range_mut();
            let p = padded as usize;
            for row in 0..band_h as usize {
                let src = (y as usize + row) * unpadded;
                view[row * p..row * p + unpadded].copy_from_slice(&rgba[src..src + unpadded]);
            }
        }
        buffer.unmap();
        encoder.copy_buffer_to_texture(
            wgpu::ImageCopyBuffer {
                buffer: &buffer,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(band_h),
                },
            },
            wgpu::ImageCopyTexture {
                texture: tex,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: w,
                height: band_h,
                depth_or_array_layers: 1,
            },
        );
        buffers.push(buffer);
        y += band_h;
    }
    // The submission keeps the band buffers alive until their copies complete.
    queue.submit(iter::once(encoder.finish()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Upload a known pattern through the staging path and read it back; it must
    /// survive byte-for-byte. A width whose row stride (100*4 = 400) is NOT
    /// 256-aligned exercises the padding path the simpler render test can't (its
    /// 1600-wide rows are already aligned).
    #[test]
    fn staging_round_trips_single_band() {
        // rows_per_band > height -> a single band (the normal fit-sized case).
        pollster::block_on(round_trip(1000));
    }

    #[test]
    fn staging_round_trips_multiple_bands() {
        // A tiny band forces 4 bands over the 10-row image (3+3+3+1), exercising
        // the y-origin banding used for images larger than max_buffer_size.
        pollster::block_on(round_trip(3));
    }

    async fn round_trip(rows_per_band: u32) {
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

        copy_via_staging(&device, &queue, &tex, &src, w, h, rows_per_band);

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
