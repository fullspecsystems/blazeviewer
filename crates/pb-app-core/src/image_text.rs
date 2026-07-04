//! "Text in image" analysis (task #45): on-device OCR of the displayed photo plus
//! QR-code payload decode, run together on a worker thread so the event loop never
//! blocks. `T` shows the result in a HUD panel; "Copy Text from Image" puts it on
//! the clipboard.
//!
//! Engines are OS-built-in and fully on-device — Windows.Media.Ocr here, Apple
//! Vision on macOS (a follow-up; the non-Windows path reports unavailable for now)
//! — and QR decode is pure Rust (`rqrr`) on every platform, because Windows has no
//! OS barcode API. Nothing is downloaded, nothing leaves the machine.
//!
//! Privacy (task #2 / ADR-018): results are RAM-only — cached per item beside the
//! other index-keyed caches, dropped on playlist rebuild and exit, never written to
//! disk. Analysis runs only on an explicit user command (`T` / Copy) or while the
//! user holds the text panel open — never as a passive byproduct of viewing.
//!
//! ⚠ QR decode reads the **full-resolution** pixels (pixels-per-module — a downscale
//! destroys small codes); only the OCR input is capped to the engine's max dimension.

use std::sync::mpsc::Receiver;

use pb_render::Rotation;
use pb_source::PhotoSource;

use crate::engine::{decode_item, rotate_rgba8, to_clipboard_rgba8};

/// Everything found in one photo: QR payloads (full-res decode) + recognized text
/// lines (OCR), plus the OCR backend's error when it couldn't run (missing language
/// pack, unsupported platform). QR results are independent of the OCR outcome.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageText {
    /// Decoded QR payloads, listed above the text in the panel and the clipboard.
    pub qr: Vec<String>,
    /// Recognized text lines, top-to-bottom (trimmed, consecutive duplicates collapsed).
    pub lines: Vec<String>,
    /// Why OCR produced nothing, when it failed outright (`None` = it ran fine).
    pub ocr_error: Option<String>,
}

impl ImageText {
    /// Nothing found at all (no QR payloads and no text).
    pub fn is_empty(&self) -> bool {
        self.qr.is_empty() && self.lines.is_empty()
    }

    /// The clipboard payload: QR payloads first (raw, so a URL pastes usable), then
    /// the recognized lines, newline-joined.
    pub fn clipboard_text(&self) -> String {
        let mut out: Vec<&str> = self.qr.iter().map(String::as_str).collect();
        out.extend(self.lines.iter().map(String::as_str));
        out.join("\n")
    }

    /// The copy-confirmation toast ("Copied 214 characters" / "Copied text + 1 QR
    /// code" / "Copied 2 QR codes"). Only meaningful when `!is_empty()`.
    pub fn copy_toast(&self) -> String {
        let codes = |n: usize| {
            if n == 1 {
                "1 QR code".to_string()
            } else {
                format!("{n} QR codes")
            }
        };
        match (self.lines.is_empty(), self.qr.len()) {
            (false, 0) => {
                let n: usize = self.lines.iter().map(|l| l.chars().count()).sum();
                format!("Copied {n} characters")
            }
            (false, n) => format!("Copied text + {}", codes(n)),
            (true, n) => format!("Copied {}", codes(n)),
        }
    }
}

/// An in-flight off-thread text scan for `item` — the `anim_decode` shape: kicked on
/// `T` / Copy (or on settling with the panel open), polled by the tick, superseded by
/// dropping the receiver. `gen` is the deck generation at kick time; a rebuild in
/// between reassigned the indices, so a late result is dropped.
pub struct TextScan {
    pub gen: u64,
    pub item: usize,
    /// Copy the result to the clipboard when it lands (the Copy command ran while
    /// the scan was still in flight).
    pub copy_when_done: bool,
    pub rx: Receiver<ImageText>,
}

/// The whole worker-thread job: decode `item` at full resolution, bake the in-RAM
/// rotation override (OCR needs the pixels upright as displayed), then QR + OCR.
/// Never returns an Err — failures fold into [`ImageText::ocr_error`] so the panel
/// and toast have one shape to render.
pub fn scan_job(source: &dyn PhotoSource, item: usize, rot: Rotation) -> ImageText {
    let img = match decode_item(source, item, None, false) {
        Ok(img) => img,
        Err(e) => {
            return ImageText {
                ocr_error: Some(format!("Can't read this image ({e})")),
                ..ImageText::default()
            }
        }
    };
    // fp16 HDR tone-maps to sRGB8 exactly like the clipboard copy — both engines
    // want plain 8-bit.
    let rgba = to_clipboard_rgba8(&img);
    let (rgba, w, h) = rotate_rgba8(&rgba, img.width, img.height, rot);
    analyze_rgba(rgba, w, h)
}

/// QR + OCR over a straight-alpha RGBA8 buffer. Split from [`scan_job`] so tests can
/// feed synthetic bitmaps without a `PhotoSource`.
pub fn analyze_rgba(rgba: Vec<u8>, w: u32, h: u32) -> ImageText {
    let gray = grayscale(&rgba);
    let qr = qr_payloads(&gray, w, h);
    // The OCR engine caps its input dimension (~2600 px on Windows); Lanczos-downscale
    // to fit. QR already ran on the full-res pixels above.
    let cap = platform::max_dimension();
    let (rgba, w, h) = if w > cap || h > cap {
        let fit = pb_decode::FitBox {
            max_width: cap,
            max_height: cap,
        };
        pb_decode::downscale_rgba8(rgba, w, h, fit).unwrap_or_else(|_| (Vec::new(), 0, 0))
    } else {
        (rgba, w, h)
    };
    let (lines, ocr_error) = if rgba.is_empty() {
        (Vec::new(), Some("Text recognition failed".to_string()))
    } else {
        match platform::ocr_lines(&rgba, w, h) {
            Ok(lines) => (clean_lines(lines), None),
            Err(e) => (Vec::new(), Some(e)),
        }
    };
    ImageText {
        qr,
        lines,
        ocr_error,
    }
}

/// RGBA8 → 8-bit luma (integer Rec.601 weights). QR decode wants grayscale.
fn grayscale(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4)
        .map(|px| {
            ((px[0] as u32 * 77 + px[1] as u32 * 150 + px[2] as u32 * 29) >> 8).min(255) as u8
        })
        .collect()
}

/// Decode every QR code found in a grayscale bitmap (pure Rust — `rqrr`). Undecodable
/// grids (damaged / not actually QR) are skipped silently; payload order follows the
/// detector's grid order.
fn qr_payloads(gray: &[u8], w: u32, h: u32) -> Vec<String> {
    if gray.is_empty() || w == 0 || h == 0 {
        return Vec::new();
    }
    let (wu, hu) = (w as usize, h as usize);
    let mut img = rqrr::PreparedImage::prepare_from_greyscale(wu, hu, |x, y| gray[y * wu + x]);
    img.detect_grids()
        .into_iter()
        .filter_map(|grid| grid.decode().ok().map(|(_, content)| content))
        .filter(|c| !c.is_empty())
        .collect()
}

/// Tidy the OCR engine's lines: trim whitespace, drop empties, collapse consecutive
/// duplicates (engines occasionally emit a line twice). No cleverness beyond that —
/// v1 keeps the raw reading order.
fn clean_lines(lines: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if out.last().map(String::as_str) != Some(t) {
            out.push(t.to_string());
        }
    }
    out
}

/// The Windows on-device engine: `Windows.Media.Ocr` on the user's installed
/// language profile. Runs on the scan worker thread — the first blocking
/// `IAsyncOperation::join()` in the tree, which is exactly why it must never run
/// on the event loop.
#[cfg(windows)]
mod platform {
    use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
    use windows::Media::Ocr::OcrEngine;
    use windows::Security::Cryptography::CryptographicBuffer;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    /// The engine's hard input cap (`OcrEngine.MaxImageDimension`, ~2600 px).
    pub fn max_dimension() -> u32 {
        OcrEngine::MaxImageDimension().unwrap_or(2600)
    }

    pub fn ocr_lines(rgba: &[u8], w: u32, h: u32) -> Result<Vec<String>, String> {
        // Same tolerant per-call init as the WIC decoder: S_FALSE / RPC_E_CHANGED_MODE
        // both mean "usable apartment", so the HRESULT is deliberately ignored.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        // Fails (or returns no engine) only when no installed language has an OCR
        // pack — English ships with Windows, so this is rare and actionable.
        let engine = OcrEngine::TryCreateFromUserProfileLanguages().map_err(|_| {
            "No OCR language is installed (Windows Settings > Time & Language)".to_string()
        })?;
        let err = |e: windows::core::Error| format!("Text recognition failed ({e})");
        // SoftwareBitmap wants BGRA8; swap in place on a copy.
        let mut bgra = rgba.to_vec();
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        let buf = CryptographicBuffer::CreateFromByteArray(&bgra).map_err(err)?;
        let bmp = SoftwareBitmap::CreateCopyFromBuffer(
            &buf,
            BitmapPixelFormat::Bgra8,
            w as i32,
            h as i32,
        )
        .map_err(err)?;
        let result = engine
            .RecognizeAsync(&bmp)
            .map_err(err)?
            .join()
            .map_err(err)?;
        let mut out = Vec::new();
        for line in result.Lines().map_err(err)? {
            out.push(line.Text().map_err(err)?.to_string_lossy());
        }
        Ok(out)
    }
}

/// Non-Windows: OCR arrives with the macOS Vision backend (same seam); until then
/// the panel reports it plainly. QR decode above works everywhere already.
#[cfg(not(windows))]
mod platform {
    pub fn max_dimension() -> u32 {
        2600
    }

    pub fn ocr_lines(_rgba: &[u8], _w: u32, _h: u32) -> Result<Vec<String>, String> {
        Err("Text recognition isn't available on this platform yet".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(qr: &[&str], lines: &[&str]) -> ImageText {
        ImageText {
            qr: qr.iter().map(|s| s.to_string()).collect(),
            lines: lines.iter().map(|s| s.to_string()).collect(),
            ocr_error: None,
        }
    }

    #[test]
    fn clipboard_text_lists_qr_payloads_above_the_text() {
        let r = result(&["https://example.com"], &["Hello", "World"]);
        assert_eq!(r.clipboard_text(), "https://example.com\nHello\nWorld");
    }

    #[test]
    fn copy_toast_covers_text_qr_and_mixed() {
        assert_eq!(
            result(&[], &["Hello", "World"]).copy_toast(),
            "Copied 10 characters"
        );
        assert_eq!(
            result(&["a"], &["Hello"]).copy_toast(),
            "Copied text + 1 QR code"
        );
        assert_eq!(
            result(&["a", "b"], &["Hello"]).copy_toast(),
            "Copied text + 2 QR codes"
        );
        assert_eq!(result(&["a"], &[]).copy_toast(), "Copied 1 QR code");
        assert_eq!(result(&["a", "b"], &[]).copy_toast(), "Copied 2 QR codes");
    }

    #[test]
    fn empty_result_reports_empty() {
        assert!(result(&[], &[]).is_empty());
        assert!(!result(&["x"], &[]).is_empty());
        assert!(!result(&[], &["x"]).is_empty());
    }

    #[test]
    fn clean_lines_trims_drops_empties_and_collapses_consecutive_duplicates() {
        let raw = vec![
            "  Hello  ".to_string(),
            "".to_string(),
            "Hello".to_string(),
            "World".to_string(),
            "   ".to_string(),
            "Hello".to_string(),
        ];
        assert_eq!(clean_lines(raw), vec!["Hello", "World", "Hello"]);
    }

    /// Render a QR code to a grayscale bitmap (quiet zone + integer scale) — the
    /// synthetic input for the round-trip test.
    fn qr_bitmap(payload: &str, scale: usize) -> (Vec<u8>, u32, u32) {
        let code = qrcode::QrCode::new(payload.as_bytes()).expect("qr encode");
        let modules = code.width();
        let quiet = 4;
        let side = (modules + 2 * quiet) * scale;
        let mut gray = vec![255u8; side * side];
        let colors = code.to_colors();
        for my in 0..modules {
            for mx in 0..modules {
                if colors[my * modules + mx] == qrcode::Color::Dark {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let x = (quiet + mx) * scale + dx;
                            let y = (quiet + my) * scale + dy;
                            gray[y * side + x] = 0;
                        }
                    }
                }
            }
        }
        (gray, side as u32, side as u32)
    }

    #[test]
    fn qr_payload_round_trips_through_the_decoder() {
        let payload = "https://example.com/pb-test?x=1";
        let (gray, w, h) = qr_bitmap(payload, 4);
        assert_eq!(qr_payloads(&gray, w, h), vec![payload.to_string()]);
    }

    #[test]
    fn qr_decode_finds_nothing_in_a_flat_image() {
        let gray = vec![128u8; 64 * 64];
        assert!(qr_payloads(&gray, 64, 64).is_empty());
    }

    #[test]
    fn analyze_rgba_finds_qr_even_when_ocr_is_unavailable_or_blank() {
        // RGBA version of the QR bitmap: whatever the OCR backend does on this
        // platform, the QR payload must come through.
        let payload = "PB-ANALYZE-TEST";
        let (gray, w, h) = qr_bitmap(payload, 4);
        let rgba: Vec<u8> = gray.iter().flat_map(|&g| [g, g, g, 255]).collect();
        let r = analyze_rgba(rgba, w, h);
        assert_eq!(r.qr, vec![payload.to_string()]);
    }
}
