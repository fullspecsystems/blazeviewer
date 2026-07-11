//! `UploadStrategy` — the swappable CPU→GPU texture-upload seam (plan §3.1).
//!
//! v1 stages pixels through a mapped buffer and `copy_buffer_to_texture` — the
//! path the upload spike measured at ~48 GB/s (3.4× the 120 Hz budget). We
//! deliberately do **not** use `queue.write_texture` (the documented trap:
//! 60–75 fps on large frames). Staging buffers are **recycled** (task #79
//! phase 3): after each copy completes the buffer re-maps in the background and
//! returns to a small pool, so steady-state animation/video playback stops
//! allocating a fresh multi-MB mapped buffer per frame. Reclamation is purely
//! opportunistic — a pool miss allocates fresh, and nothing ever waits on the
//! GPU. A CUDA zero-copy backend remains a future alternative behind this trait.

use std::iter;
use std::sync::{Arc, Mutex};

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

/// Most staging buffers kept for reuse. Animation/video playback needs 1–2 in
/// steady rotation; anything beyond that is idle RAM (a 4K RGBA band is ~33 MB).
const MAX_STAGING_POOL: usize = 3;

/// The staging + `copy_buffer_to_texture` upload, with buffer recycling.
///
/// Each band stages through a mapped buffer. After the submit, the buffer
/// re-maps asynchronously (`map_async`; the callback fires during the device's
/// normal maintenance) and rejoins [`Self::ready`], so the next same-size upload
/// — the animation/video steady state — takes an already-mapped buffer instead
/// of allocating. A miss (first frames, size change, callback not fired yet)
/// allocates fresh: exactly the old behavior, never a wait.
#[derive(Default)]
pub struct StagingUpload {
    /// Recycled staging buffers, mapped and ready to fill (bounded by
    /// [`MAX_STAGING_POOL`]). Outer `Arc` because the map_async callbacks that
    /// refill it outlive `&mut self`; per-buffer `Arc` because wgpu 22 buffers
    /// aren't `Clone` and the callback must own a handle.
    ready: Arc<Mutex<Vec<Arc<wgpu::Buffer>>>>,
}

impl StagingUpload {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test hook: how many recycled buffers are ready right now.
    #[cfg(test)]
    fn pool_len(&self) -> usize {
        self.ready.lock().unwrap().len()
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
        // Bytes per pixel from the texture format (4 for Rgba8*, 8 for Rgba16Float).
        let bpp = tex.format().block_copy_size(None).unwrap_or(4);
        // Keep each staging buffer within the device's max buffer size by copying
        // the image in horizontal row-bands. Normal (fit-sized) images are a
        // single band; only huge Original-mode/panorama images split.
        let padded = padded_row_bytes(w, bpp);
        let max_rows = (device.limits().max_buffer_size / padded.max(1) as u64).max(1);
        let rows_per_band = max_rows.min(h.max(1) as u64) as u32;
        // Pump finished map-backs (non-blocking) so completed buffers rejoin the
        // pool before we look for one.
        let _ = device.poll(wgpu::Maintain::Poll);
        copy_via_staging(
            device,
            queue,
            tex,
            rgba,
            w,
            h,
            bpp,
            rows_per_band,
            Some(&self.ready),
        );
    }

    fn name(&self) -> &'static str {
        "staging-ring"
    }
}

/// The 256-byte-aligned row stride for a `w`-pixel row of `bpp` bytes/pixel, as
/// `copy_buffer_to_texture` requires.
fn padded_row_bytes(w: u32, bpp: u32) -> u32 {
    let unpadded = w * bpp;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    unpadded.div_ceil(align) * align
}

/// Stage `rgba` (`w*h*4`, tight) into `tex` in horizontal bands of at most
/// `rows_per_band` rows — each through a mapped buffer + a
/// `copy_buffer_to_texture` at the band's `y` origin. One encoder, one submit.
/// Banding bounds each staging buffer by the device's buffer-size limit so an
/// arbitrarily tall image uploads without exceeding it.
///
/// With `pool` set, band buffers come from the recycle pool when an exact-size,
/// already-remapped one exists (the steady state for constant-size animation /
/// video frames), and every band buffer re-maps in the background after the
/// submit to rejoin the pool. Pool buffers carry `MAP_WRITE` so they *can*
/// re-map; that keeps them host-visible, which is where a staging source lives
/// anyway.
#[allow(clippy::too_many_arguments)]
fn copy_via_staging(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    tex: &wgpu::Texture,
    rgba: &[u8],
    w: u32,
    h: u32,
    bpp: u32,
    rows_per_band: u32,
    pool: Option<&Arc<Mutex<Vec<Arc<wgpu::Buffer>>>>>,
) {
    let unpadded = w as usize * bpp as usize;
    let padded = padded_row_bytes(w, bpp);
    let rows_per_band = rows_per_band.max(1);
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("staging-copy"),
    });
    // The band buffers must outlive `encoder.finish()` / the submit.
    let mut buffers = Vec::new();
    let mut y = 0u32;
    while y < h {
        let band_h = rows_per_band.min(h - y);
        let size = padded as u64 * band_h as u64;
        // Take an exact-size recycled buffer (already mapped) if one is ready.
        let recycled = pool.and_then(|p| {
            let mut g = p.lock().unwrap();
            g.iter()
                .position(|b| b.size() == size)
                .map(|i| g.swap_remove(i))
        });
        let buffer = recycled.unwrap_or_else(|| {
            Arc::new(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("staging-upload"),
                size,
                // MAP_WRITE lets the buffer re-map after its copy so it can be
                // recycled; without a pool the old COPY_SRC-only shape suffices,
                // but one shape keeps the paths identical.
                usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::MAP_WRITE,
                mapped_at_creation: true,
            }))
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
    // Send every band buffer back toward the pool: the re-map completes after the
    // copy retires, the callback fires during normal device maintenance (our
    // next-upload `poll` or the frame loop's submits), and the buffer becomes an
    // instant hit for the next same-size upload. Beyond the cap, drop instead —
    // bounded RAM under any usage pattern. Nothing here blocks.
    if let Some(pool) = pool {
        for buffer in buffers {
            let pool = Arc::clone(pool);
            let handle = Arc::clone(&buffer);
            buffer
                .slice(..)
                .map_async(wgpu::MapMode::Write, move |res| {
                    if res.is_ok() {
                        let mut g = pool.lock().unwrap();
                        if g.len() < MAX_STAGING_POOL {
                            g.push(handle);
                        }
                    }
                });
        }
    }
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
        // Serialize GPU-device creation vs the other GPU tests (see `crate::gpu_test_lock`).
        let _gpu = crate::gpu_test_lock();
        // rows_per_band > height -> a single band (the normal fit-sized case).
        pollster::block_on(round_trip(1000));
    }

    #[test]
    fn staging_round_trips_multiple_bands() {
        let _gpu = crate::gpu_test_lock();
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
            // Per-OS primary backend (DX12/Metal/Vulkan); see `gpu::instance`.
            backends: wgpu::Backends::PRIMARY,
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

        copy_via_staging(&device, &queue, &tex, &src, w, h, 4, rows_per_band, None);

        let got = read_texture(&device, &queue, &tex, w, h);
        assert_eq!(got, src, "staging upload corrupted pixels");
    }

    /// Read an `Rgba8` texture back to tightly packed bytes (test helper).
    fn read_texture(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        tex: &wgpu::Texture,
        w: u32,
        h: u32,
    ) -> Vec<u8> {
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
                texture: tex,
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
        let mut got = Vec::with_capacity((w * h * 4) as usize);
        for row in 0..h {
            let start = (row * padded) as usize;
            got.extend_from_slice(&mapped[start..start + unpadded as usize]);
        }
        drop(mapped);
        readback.unmap();
        got
    }

    /// Task #79 phase 3: a second same-size upload must take the recycled buffer
    /// (zero staging allocation in steady state) and still deliver correct pixels.
    #[test]
    fn recycling_reuses_the_staging_buffer_between_same_size_uploads() {
        let _gpu = crate::gpu_test_lock();
        pollster::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
                backends: wgpu::Backends::PRIMARY,
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

            let (w, h) = (64u32, 64u32);
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

            let mut up = StagingUpload::new();
            let frame_a = vec![0xAAu8; (w * h * 4) as usize];
            up.upload(&device, &queue, &tex, &frame_a, w, h);
            // Test-only wait: let the copy retire and the re-map callback fire.
            device.poll(wgpu::Maintain::Wait);
            assert_eq!(up.pool_len(), 1, "buffer must rejoin the pool");

            let frame_b: Vec<u8> = (0..(w * h * 4) as usize).map(|i| (i % 251) as u8).collect();
            up.upload(&device, &queue, &tex, &frame_b, w, h);
            let got = read_texture(&device, &queue, &tex, w, h);
            assert_eq!(got, frame_b, "recycled buffer must carry the new frame");
            device.poll(wgpu::Maintain::Wait);
            assert_eq!(up.pool_len(), 1, "the same buffer keeps cycling");
        });
    }
}
