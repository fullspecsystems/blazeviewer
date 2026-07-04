//! "Text in image" analysis (task #45): on-device OCR of the displayed photo plus
//! QR-code payload decode, run together on a worker thread so the event loop never
//! blocks. `T` shows the result in a HUD panel; "Copy Text from Image" puts it on
//! the clipboard.
//!
//! Engines are OS-built-in and fully on-device — Windows.Media.Ocr on Windows, Apple
//! Vision (`VNRecognizeTextRequest`) on macOS — and QR decode is pure Rust (`rqrr`) on
//! every platform, because Windows has no OS barcode API. Nothing is downloaded, nothing
//! leaves the machine. Recognized lines are regrouped into paragraphs ([`group_paragraphs`])
//! so copied text flows as blocks rather than hard-broken lines.
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
    /// Recognized text, grouped into paragraphs in top-to-bottom reading order (each entry
    /// is one paragraph; see [`group_paragraphs`]).
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
            Ok(lines) => (group_paragraphs(lines), None),
            Err(e) => (Vec::new(), Some(e)),
        }
    };
    ImageText {
        qr,
        lines,
        ocr_error,
    }
}

/// One recognized text line plus its bounding box — the currency the OCR backends hand back
/// so [`group_paragraphs`] can reconstruct paragraphs. The box is **normalized** to the OCR
/// input (each component 0..1), **top-left origin, y increasing downward** — the backends
/// convert into this one convention (Windows word rects are pixels; Vision's box is
/// bottom-left normalized), so the grouping heuristic is resolution- and platform-independent.
pub(crate) struct OcrLineBox {
    pub text: String,
    /// `[x, y, w, h]`, normalized, top-left origin.
    pub bbox: [f32; 4],
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

/// Tidy a list of text lines: trim whitespace, drop empties, collapse consecutive
/// duplicates (engines occasionally emit a line twice). No cleverness beyond that.
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

/// A vertical gap larger than this fraction of the line height starts a new paragraph — i.e.
/// roughly a blank line between text blocks. Tuned for single-column bodies; deliberately
/// tolerant (values well under 1.0 keep tight line spacing together).
const PARAGRAPH_GAP_RATIO: f32 = 0.6;

/// Reassemble the OCR engine's per-line output into **paragraphs** so copied text reads as
/// flowing blocks instead of hard-broken lines (owner request 2026-07-04): sort the boxes
/// into reading order (top-to-bottom, then left-to-right), then break to a new paragraph
/// whenever the vertical gap to the previous line exceeds [`PARAGRAPH_GAP_RATIO`] of the line
/// height — a blank-line-sized gap. Lines inside one paragraph join with a single space.
///
/// Platform-neutral: both backends feed [`OcrLineBox`] in the same normalized, top-left space,
/// so this improves Windows and macOS identically and stays unit-testable with synthetic
/// boxes. v1 assumes a single column (multi-column layout — the job of macOS 26's
/// `RecognizeDocumentsRequest` — is out of scope); degenerate/zero boxes simply never trigger
/// a break, so the output gracefully falls back to the raw line order.
fn group_paragraphs(lines: Vec<OcrLineBox>) -> Vec<String> {
    // Trim + drop blank lines up front so an empty box can't fabricate a paragraph break.
    let mut lines: Vec<OcrLineBox> = lines
        .into_iter()
        .filter_map(|mut l| {
            let t = l.text.trim();
            if t.is_empty() {
                return None;
            }
            l.text = t.to_string();
            Some(l)
        })
        .collect();
    if lines.is_empty() {
        return Vec::new();
    }
    // Reading order: top edge, then left edge. Engines usually emit this already, but a sort
    // makes the grouping robust to ones that don't (and keeps the heuristic deterministic).
    lines.sort_by(|a, b| {
        a.bbox[1]
            .total_cmp(&b.bbox[1])
            .then(a.bbox[0].total_cmp(&b.bbox[0]))
    });

    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut prev: Option<[f32; 4]> = None;
    for line in &lines {
        let starts_paragraph = prev.is_some_and(|p| {
            let prev_bottom = p[1] + p[3];
            let gap = line.bbox[1] - prev_bottom;
            let line_h = line.bbox[3].max(p[3]).max(f32::EPSILON);
            gap > PARAGRAPH_GAP_RATIO * line_h
        });
        if starts_paragraph {
            paragraphs.push(std::mem::take(&mut current));
        } else if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(&line.text);
        prev = Some(line.bbox);
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    // Collapse an engine's occasional duplicate emission at the paragraph level.
    clean_lines(paragraphs)
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

    pub fn ocr_lines(rgba: &[u8], w: u32, h: u32) -> Result<Vec<super::OcrLineBox>, String> {
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
        // `OcrLine` carries no rect of its own — union its words' pixel `BoundingRect`s, then
        // normalize to the [0,1] top-left space `group_paragraphs` expects. A line with no
        // words (shouldn't happen) yields a zero box, which never triggers a paragraph break.
        let (fw, fh) = (w.max(1) as f32, h.max(1) as f32);
        let mut out = Vec::new();
        for line in result.Lines().map_err(err)? {
            let text = line.Text().map_err(err)?.to_string_lossy();
            let (mut min_x, mut min_y) = (f32::MAX, f32::MAX);
            let (mut max_x, mut max_y) = (f32::MIN, f32::MIN);
            for word in line.Words().map_err(err)? {
                let r = word.BoundingRect().map_err(err)?;
                min_x = min_x.min(r.X);
                min_y = min_y.min(r.Y);
                max_x = max_x.max(r.X + r.Width);
                max_y = max_y.max(r.Y + r.Height);
            }
            let bbox = if max_x >= min_x && max_y >= min_y {
                [
                    min_x / fw,
                    min_y / fh,
                    (max_x - min_x) / fw,
                    (max_y - min_y) / fh,
                ]
            } else {
                [0.0, 0.0, 0.0, 0.0]
            };
            out.push(super::OcrLineBox { text, bbox });
        }
        Ok(out)
    }
}

/// The macOS on-device engine: Apple **Vision** `VNRecognizeTextRequest` (accurate mode) —
/// the twin of the Windows.Media.Ocr backend above. Fully on-device (no model shipped, no
/// network) and, like the Windows path, it runs on the scan worker thread: Vision's
/// `performRequests:` blocks until recognition finishes, which is exactly why this must
/// never run on the event loop. Typed `objc2` bindings mirror the `windows` crate's WinRT
/// posture on the other platform.
#[cfg(target_os = "macos")]
mod platform {
    use objc2::rc::autoreleasepool;
    use objc2::AnyThread;
    use objc2_core_foundation::{CFData, CFRetained};
    use objc2_core_graphics::{
        CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage,
        CGImageAlphaInfo,
    };
    use objc2_foundation::{NSArray, NSDictionary};
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
    };

    /// Vision has no fixed input cap like Windows' `OcrEngine.MaxImageDimension` (it
    /// down-samples internally), but keeping the shared 2600 means both platforms feed the
    /// engine a bounded image and exercise the same Lanczos downscale path — QR already ran
    /// on the full-resolution pixels before this.
    pub fn max_dimension() -> u32 {
        2600
    }

    pub fn ocr_lines(rgba: &[u8], w: u32, h: u32) -> Result<Vec<super::OcrLineBox>, String> {
        if w == 0 || h == 0 || rgba.len() != (w as usize) * (h as usize) * 4 {
            return Err("Text recognition failed (bad image)".to_string());
        }
        // Vision + Foundation vend autoreleased temporaries; the scan worker thread has no
        // ambient autorelease pool, so wrap the whole call in one (deterministic, leak-free).
        autoreleasepool(|_| {
            let image = cg_image(rgba, w, h).ok_or("Text recognition failed (image build)")?;
            // Empty options dict (no camera-intrinsics hints); typed to the handler's key.
            let options = NSDictionary::new();
            // SAFETY: `image` is a valid CGImage that outlives the handler and the request
            // below (all dropped at the end of this scope, after `performRequests` returns).
            let handler = unsafe {
                VNImageRequestHandler::initWithCGImage_options(
                    VNImageRequestHandler::alloc(),
                    &image,
                    &options,
                )
            };
            let request = VNRecognizeTextRequest::new();
            request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
            request.setUsesLanguageCorrection(true);

            // `performRequests:` takes `NSArray<VNRequest>`; upcast the concrete request.
            let req_ref: &VNRequest = &request;
            let requests = NSArray::from_slice(&[req_ref]);
            handler
                .performRequests_error(&requests)
                .map_err(|e| format!("Text recognition failed ({e})"))?;

            // `VNRecognizeTextRequest::results()` is already typed as recognized-text
            // observations — no downcast — one block per line, top-to-bottom.
            let mut out = Vec::new();
            if let Some(results) = request.results() {
                for obs in results.iter() {
                    // The single most-confident candidate for this block (v1: no alternates).
                    let Some(best) = obs.topCandidates(1).iter().next() else {
                        continue;
                    };
                    // Vision's `boundingBox` is normalized with the origin at the image's
                    // LOWER-left; flip Y so the box matches `group_paragraphs`' top-left space.
                    // SAFETY: `obs` is a live observation for the duration of this pool.
                    let r = unsafe { obs.boundingBox() };
                    let bbox = [
                        r.origin.x as f32,
                        1.0 - (r.origin.y + r.size.height) as f32,
                        r.size.width as f32,
                        r.size.height as f32,
                    ];
                    out.push(super::OcrLineBox {
                        text: best.string().to_string(),
                        bbox,
                    });
                }
            }
            Ok(out)
        })
    }

    /// Build a `CGImage` from the straight-alpha RGBA8 buffer (8bpc / 32bpp / alpha-last /
    /// device-RGB — the same layout the clipboard copy uses). `CFData` copies the bytes, so
    /// the image owns its pixels and there's no borrow of `rgba` to keep alive past here.
    fn cg_image(rgba: &[u8], w: u32, h: u32) -> Option<CFRetained<CGImage>> {
        // SAFETY: `rgba` is a valid slice of `len` bytes; CFDataCreate copies from it.
        let data = unsafe { CFData::new(None, rgba.as_ptr(), rgba.len() as isize) }?;
        let provider = CGDataProvider::with_cf_data(Some(&data))?;
        let space = CGColorSpace::new_device_rgb()?;
        let bitmap = CGBitmapInfo(CGImageAlphaInfo::Last.0);
        // SAFETY: width/height/stride are consistent with `data`'s length (checked by the
        // caller); `decode` is null (identity) and every reference outlives the call.
        unsafe {
            CGImage::new(
                w as usize,
                h as usize,
                8,
                32,
                (w as usize) * 4,
                Some(&space),
                bitmap,
                Some(&provider),
                std::ptr::null(),
                false,
                CGColorRenderingIntent::RenderingIntentDefault,
            )
        }
    }
}

/// Other non-Windows, non-macOS platforms (Linux/BSD): no OS OCR engine is wired. QR decode
/// above still works everywhere; the panel reports OCR unavailable.
#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    pub fn max_dimension() -> u32 {
        2600
    }

    pub fn ocr_lines(_rgba: &[u8], _w: u32, _h: u32) -> Result<Vec<super::OcrLineBox>, String> {
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

    /// A synthetic OCR line box: `text` at normalized `(x, y)` with size `(w, h)`.
    fn lbox(text: &str, x: f32, y: f32, w: f32, h: f32) -> OcrLineBox {
        OcrLineBox {
            text: text.to_string(),
            bbox: [x, y, w, h],
        }
    }

    #[test]
    fn group_paragraphs_joins_tightly_spaced_lines_into_one_block() {
        // Two lines a fraction of a line-height apart → one flowing paragraph.
        let lines = vec![
            lbox("The quick brown", 0.1, 0.10, 0.5, 0.04),
            lbox("fox jumps.", 0.1, 0.15, 0.5, 0.04),
        ];
        assert_eq!(group_paragraphs(lines), vec!["The quick brown fox jumps."]);
    }

    #[test]
    fn group_paragraphs_breaks_on_a_blank_line_sized_gap() {
        // A title, a blank-line-sized gap, then a two-line body block.
        let lines = vec![
            lbox("Title", 0.1, 0.05, 0.3, 0.04),
            lbox("Body line one", 0.1, 0.30, 0.5, 0.04),
            lbox("body line two", 0.1, 0.35, 0.5, 0.04),
        ];
        assert_eq!(
            group_paragraphs(lines),
            vec![
                "Title".to_string(),
                "Body line one body line two".to_string()
            ]
        );
    }

    #[test]
    fn group_paragraphs_sorts_into_reading_order_and_drops_blanks() {
        // Fed out of order and with a blank line; output is top-to-bottom, blank ignored.
        let lines = vec![
            lbox("second", 0.1, 0.20, 0.4, 0.04),
            lbox("   ", 0.1, 0.14, 0.4, 0.04),
            lbox("first", 0.1, 0.10, 0.4, 0.04),
        ];
        // 'first' (y=.10) then 'second' (y=.20): gap .06 > 0.6*.04 → two paragraphs.
        assert_eq!(
            group_paragraphs(lines),
            vec!["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn group_paragraphs_empty_input_is_empty() {
        assert!(group_paragraphs(Vec::new()).is_empty());
        assert!(group_paragraphs(vec![lbox("  ", 0.0, 0.0, 0.1, 0.02)]).is_empty());
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
