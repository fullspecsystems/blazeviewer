//! Copy the current photo to the OS clipboard (tasks.json #27).
//!
//! We put the photo on the clipboard in **two formats at once** so a paste does the
//! right thing wherever it lands:
//! - **CF_DIBV5** — the (rotation-baked) pixels, for pasting into image editors,
//!   documents, and chat. (Windows synthesizes CF_DIB / CF_BITMAP from it for older
//!   consumers.)
//! - **CF_HDROP** — a file-drop *reference* to the original file, for pasting into
//!   Explorer / another folder or attaching in email — preserving the original bytes
//!   (EXIF / ICC / quality), and instant (no decode).
//!
//! The pure pixel transforms — [`to_clipboard_rgba8`] (format convert) and
//! [`rotate_rgba8`] (bake the in-RAM rotation) — are unit-tested here; the Win32
//! clipboard write is the platform shell.
//!
//! Note: a *pending* in-RAM rotation (rotated with `r`, not yet saved to the file via
//! Save Rotation) shows in the bitmap but not in the file the CF_HDROP points at —
//! save first if you need them to match (expected; rotation is RAM-only until saved).
//!
//! Privacy (tasks.json #2): the clipboard is an explicit, user-initiated command —
//! not a passive viewing trace — so it's outside the no-trace guarantee.

use pb_decode::{DecodedImage, PixelFormat};
use pb_render::Rotation;

/// sRGB OETF (scene-linear → sRGB-encoded), matching the present shader's
/// `srgb_oetf` in `pb-render/src/gpu.rs`.
fn srgb_oetf(c: f32) -> f32 {
    let x = c.clamp(0.0, 1.0);
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// Extended-Reinhard tone-map with white point `lw`, matching the present shader's
/// `reinhard`. `lw = 1` is the identity (faithful SDR); a larger `lw` rolls HDR
/// highlights into the displayable range.
fn reinhard(v: f32, lw: f32) -> f32 {
    let x = v.max(0.0);
    x * (1.0 + x / (lw * lw)) / (1.0 + x)
}

/// Convert a decoded image to a straight-alpha RGBA8 buffer for the clipboard.
///
/// - `Rgba8` is taken as-is (source-encoded sRGB). The DIB clipboard format carries
///   no ICC profile, so a wide-gamut source pastes interpreted as sRGB — a
///   documented v1 limitation.
/// - `Rgba16F` (HDR scene-linear scRGB) is tone-mapped to SDR sRGB8 exactly as the
///   SDR present pass does: extended-Reinhard at the image `peak`, then sRGB-encode.
pub fn to_clipboard_rgba8(img: &DecodedImage) -> Vec<u8> {
    match img.format {
        PixelFormat::Rgba8 => img.pixels.clone(),
        PixelFormat::Rgba16F => {
            let lw = img.peak.max(1.0);
            let px_count = (img.width as usize) * (img.height as usize);
            let mut out = Vec::with_capacity(px_count * 4);
            // 4 half-floats (8 bytes) per pixel, little-endian.
            for px in img.pixels.chunks_exact(8) {
                for ch in 0..3 {
                    let h = half::f16::from_le_bytes([px[ch * 2], px[ch * 2 + 1]]);
                    let v = srgb_oetf(reinhard(h.to_f32(), lw));
                    out.push((v * 255.0 + 0.5) as u8);
                }
                out.push(255); // opaque; HDR sources have no meaningful alpha here
            }
            out
        }
    }
}

/// Rotate a tightly-packed RGBA8 buffer by a 90° quadrant (clockwise), returning the
/// rotated buffer and its new dimensions. `R0` clones unchanged. Used to bake the
/// in-RAM rotation override (the `r` / `Shift+R` overlay transform, which is a GPU
/// transform — not baked into the decoded pixels) into the copied image so the
/// clipboard is WYSIWYG.
pub fn rotate_rgba8(pixels: &[u8], w: u32, h: u32, rot: Rotation) -> (Vec<u8>, u32, u32) {
    if rot == Rotation::R0 {
        return (pixels.to_vec(), w, h);
    }
    let (wu, hu) = (w as usize, h as usize);
    let (new_w, new_h) = if rot.swaps_axes() { (h, w) } else { (w, h) };
    let nwu = new_w as usize;
    let mut out = vec![0u8; wu * hu * 4];
    for sy in 0..hu {
        for sx in 0..wu {
            // Destination pixel coordinates for this source pixel after the turn.
            let (dx, dy) = match rot {
                Rotation::R90 => (hu - 1 - sy, sx),
                Rotation::R180 => (wu - 1 - sx, hu - 1 - sy),
                Rotation::R270 => (sy, wu - 1 - sx),
                Rotation::R0 => unreachable!(),
            };
            let src = (sy * wu + sx) * 4;
            let dst = (dy * nwu + dx) * 4;
            out[dst..dst + 4].copy_from_slice(&pixels[src..src + 4]);
        }
    }
    (out, new_w, new_h)
}

/// Write the image (pixels, CF_DIBV5) **and** a reference to its file (CF_HDROP) to
/// the OS clipboard. `rgba` is straight-alpha `width×height`; `path` is the source
/// file. See the module docs for the two-format rationale.
#[cfg(windows)]
pub fn set_image_and_file(
    width: u32,
    height: u32,
    rgba: &[u8],
    path: &std::path::Path,
) -> Result<(), String> {
    win::set(width, height, rgba, Some(path))
}

/// Write only the image (CF_DIBV5), with no CF_HDROP file reference — for sources
/// that have no standalone file on disk (an image inside an archive). A paste still
/// lands the pixels; there's just no file to drag into a folder.
#[cfg(windows)]
pub fn set_image(width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    win::set(width, height, rgba, None)
}

/// Non-Windows stub (macOS port is deferred; it would mirror this with `NSPasteboard`
/// writing both an image and a file URL).
#[cfg(not(windows))]
pub fn set_image_and_file(
    _width: u32,
    _height: u32,
    _rgba: &[u8],
    _path: &std::path::Path,
) -> Result<(), String> {
    Err("clipboard copy is only implemented on Windows".to_string())
}

/// Non-Windows stub for the image-only write.
#[cfg(not(windows))]
pub fn set_image(_width: u32, _height: u32, _rgba: &[u8]) -> Result<(), String> {
    Err("clipboard copy is only implemented on Windows".to_string())
}

#[cfg(windows)]
mod win {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows::core::BOOL;
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, POINT};
    use windows::Win32::Graphics::Gdi::{BITMAPV5HEADER, BI_RGB};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::{CF_DIBV5, CF_HDROP};
    use windows::Win32::UI::Shell::DROPFILES;

    /// `'sRGB'` little-endian — `LCS_sRGB` for `BITMAPV5HEADER::bV5CSType`.
    const LCS_SRGB: u32 = 0x7352_4742;

    pub fn set(width: u32, height: u32, rgba: &[u8], path: Option<&Path>) -> Result<(), String> {
        let dib = dibv5_blob(width, height, rgba);
        // Archive entries have no file on disk, so there's no CF_HDROP to offer.
        let hdrop = path.map(hdrop_blob);
        // SAFETY: standard clipboard sequence; every HGLOBAL handed to a successful
        // SetClipboardData is owned by the system thereafter (see `put`).
        unsafe {
            OpenClipboard(None).map_err(|e| format!("OpenClipboard: {e}"))?;
            let r = set_formats(&dib, hdrop.as_deref());
            let _ = CloseClipboard();
            r
        }
    }

    /// Inside an open clipboard: empty it, then set the image (and the file ref, if
    /// there is one).
    unsafe fn set_formats(dib: &[u8], hdrop: Option<&[u8]>) -> Result<(), String> {
        EmptyClipboard().map_err(|e| format!("EmptyClipboard: {e}"))?;
        put(CF_DIBV5.0 as u32, dib).map_err(|e| format!("CF_DIBV5: {e}"))?;
        if let Some(hdrop) = hdrop {
            put(CF_HDROP.0 as u32, hdrop).map_err(|e| format!("CF_HDROP: {e}"))?;
        }
        Ok(())
    }

    /// Move `bytes` into a moveable HGLOBAL and hand it to `SetClipboardData`. On
    /// success the system owns the memory; on failure we free it ourselves.
    unsafe fn put(format: u32, bytes: &[u8]) -> Result<(), String> {
        let hmem = hglobal_from(bytes)?;
        match SetClipboardData(format, Some(HANDLE(hmem.0))) {
            Ok(_) => Ok(()),
            Err(e) => {
                let _ = GlobalFree(Some(hmem));
                Err(e.to_string())
            }
        }
    }

    /// Allocate a `GMEM_MOVEABLE` HGLOBAL and copy `bytes` into it.
    unsafe fn hglobal_from(bytes: &[u8]) -> Result<HGLOBAL, String> {
        let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).map_err(|e| e.to_string())?;
        let p = GlobalLock(hmem);
        if p.is_null() {
            let _ = GlobalFree(Some(hmem));
            return Err("GlobalLock failed".to_string());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
        let _ = GlobalUnlock(hmem);
        Ok(hmem)
    }

    /// `BITMAPV5HEADER` + bottom-up, opaque BGRA pixels. Forced opaque (alpha = 255)
    /// for broad paste compatibility — clipboard transparency is unreliable across
    /// apps, and photos are opaque anyway.
    fn dibv5_blob(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let (w, h) = (width as usize, height as usize);
        let hdr = BITMAPV5HEADER {
            bV5Size: std::mem::size_of::<BITMAPV5HEADER>() as u32,
            bV5Width: width as i32,
            bV5Height: height as i32, // positive ⇒ bottom-up rows
            bV5Planes: 1,
            bV5BitCount: 32,
            bV5Compression: BI_RGB,
            bV5SizeImage: (w * h * 4) as u32,
            bV5CSType: LCS_SRGB,
            ..Default::default()
        };
        let mut out = Vec::with_capacity(std::mem::size_of::<BITMAPV5HEADER>() + w * h * 4);
        // SAFETY: BITMAPV5HEADER is repr(C), POD; read its bytes verbatim.
        let hdr_bytes = unsafe {
            std::slice::from_raw_parts(
                std::ptr::addr_of!(hdr) as *const u8,
                std::mem::size_of::<BITMAPV5HEADER>(),
            )
        };
        out.extend_from_slice(hdr_bytes);
        // Bottom-up: the DIB's first row is the image's last row. RGBA → BGRA, opaque.
        for sy in (0..h).rev() {
            let row = sy * w * 4;
            for x in 0..w {
                let i = row + x * 4;
                out.push(rgba[i + 2]);
                out.push(rgba[i + 1]);
                out.push(rgba[i]);
                out.push(255);
            }
        }
        out
    }

    /// `DROPFILES` + one double-null-terminated wide (UTF-16) absolute path.
    fn hdrop_blob(path: &Path) -> Vec<u8> {
        let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let wide: Vec<u16> = abs.as_os_str().encode_wide().collect();
        let df = DROPFILES {
            pFiles: std::mem::size_of::<DROPFILES>() as u32, // file list follows the header
            pt: POINT { x: 0, y: 0 },
            fNC: BOOL(0),
            fWide: BOOL(1), // wide (UTF-16) paths
        };
        let mut out = Vec::new();
        // SAFETY: DROPFILES is repr(C), POD; read its bytes verbatim.
        let df_bytes = unsafe {
            std::slice::from_raw_parts(
                std::ptr::addr_of!(df) as *const u8,
                std::mem::size_of::<DROPFILES>(),
            )
        };
        out.extend_from_slice(df_bytes);
        for u in wide {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes()); // terminate the path
        out.extend_from_slice(&0u16.to_le_bytes()); // terminate the list (double null)
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pb_decode::ColorTransform;

    fn img_rgba8(width: u32, height: u32, pixels: Vec<u8>) -> DecodedImage {
        DecodedImage {
            width,
            height,
            orig_width: width,
            orig_height: height,
            codec: "TEST",
            format: PixelFormat::Rgba8,
            pixels,
            is_preview: false,
            color: ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        }
    }

    #[test]
    fn rgba8_is_copied_verbatim() {
        let px = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let img = img_rgba8(2, 1, px.clone());
        assert_eq!(to_clipboard_rgba8(&img), px);
    }

    #[test]
    fn rgba16f_tonemaps_to_srgb8_opaque() {
        // One scene-linear pixel: R=1.0, G=0.0, B=0.5, A=1.0 packed as f16 LE.
        let mut bytes = Vec::new();
        for v in [1.0f32, 0.0, 0.5, 1.0] {
            bytes.extend_from_slice(&half::f16::from_f32(v).to_le_bytes());
        }
        let mut img = img_rgba8(1, 1, Vec::new());
        img.format = PixelFormat::Rgba16F;
        img.pixels = bytes;
        let out = to_clipboard_rgba8(&img);
        assert_eq!(out.len(), 4);
        // peak=1 ⇒ reinhard is identity; sRGB-encode 1.0/0.0/0.5.
        assert_eq!(out[0], 255); // 1.0 → white
        assert_eq!(out[1], 0); //   0.0 → black
        assert_eq!(out[3], 255); // forced opaque
        let mid = (srgb_oetf(0.5) * 255.0 + 0.5) as u8;
        assert_eq!(out[2], mid); // ~188
    }

    /// A 2×2 image with four distinct, identifiable pixels (R, G, B, W).
    fn quad() -> (Vec<u8>, u32, u32) {
        #[rustfmt::skip]
        let px = vec![
            255,0,0,255,   0,255,0,255,   // (0,0)=R (1,0)=G
            0,0,255,255,   255,255,255,255 // (0,1)=B (1,1)=W
        ];
        (px, 2, 2)
    }

    fn pixel_at(buf: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn rotate_r0_is_identity() {
        let (px, w, h) = quad();
        let (out, nw, nh) = rotate_rgba8(&px, w, h, Rotation::R0);
        assert_eq!((nw, nh), (2, 2));
        assert_eq!(out, px);
    }

    #[test]
    fn rotate_r90_cw_moves_topleft_to_topright() {
        let (px, w, h) = quad();
        let (out, nw, nh) = rotate_rgba8(&px, w, h, Rotation::R90);
        assert_eq!((nw, nh), (2, 2));
        // 90° CW: top-left R → top-right; top-right G → bottom-right;
        // bottom-right W → bottom-left; bottom-left B → top-left.
        assert_eq!(pixel_at(&out, nw, 1, 0), [255, 0, 0, 255]); // R
        assert_eq!(pixel_at(&out, nw, 0, 0), [0, 0, 255, 255]); // B
        assert_eq!(pixel_at(&out, nw, 1, 1), [0, 255, 0, 255]); // G
        assert_eq!(pixel_at(&out, nw, 0, 1), [255, 255, 255, 255]); // W
    }

    #[test]
    fn rotate_dims_swap_on_quarter_turns() {
        // A 3×2 image becomes 2×3 after a quarter turn, unchanged after a half turn.
        let px = vec![0u8; 3 * 2 * 4];
        let (_, w90, h90) = rotate_rgba8(&px, 3, 2, Rotation::R90);
        assert_eq!((w90, h90), (2, 3));
        let (_, w270, h270) = rotate_rgba8(&px, 3, 2, Rotation::R270);
        assert_eq!((w270, h270), (2, 3));
        let (_, w180, h180) = rotate_rgba8(&px, 3, 2, Rotation::R180);
        assert_eq!((w180, h180), (3, 2));
    }

    #[test]
    fn four_cw_rotations_return_to_original() {
        let (px, w, h) = quad();
        let (mut cur, mut cw, mut ch) = (px.clone(), w, h);
        for _ in 0..4 {
            let (n, nw, nh) = rotate_rgba8(&cur, cw, ch, Rotation::R90);
            cur = n;
            cw = nw;
            ch = nh;
        }
        assert_eq!((cw, ch), (w, h));
        assert_eq!(cur, px);
    }
}
