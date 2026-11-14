//! **HDR brightness audit** (task #146): does every HDR decoder agree that scene-linear
//! 1.0 means the 203-nit SDR reference white (BT.2408, `color::SDR_WHITE_NITS`)?
//!
//! The cards in `tests/fixtures/hdr/` (made by `generate-hdr-cards.sh`, with tools
//! independent of our decoders) are flat grey patches of known display brightness, encoded
//! with the exact PQ / HLG code values. Patch *k* must decode to `nits[k] / 203` through
//! every path: the display decode, the export decode (#89), and the video posters. A
//! backend on a different scale shows the same photo brighter or dimmer depending on which
//! decoder opened it, mis-tone-maps on SDR screens, and would bake the error into HDR
//! exports.
//!
//! `cargo test -p pb-decode hdr_audit::report -- --ignored --nocapture` prints the whole
//! table; the other tests pin each backend.

use std::path::{Path, PathBuf};

use crate::{DecodedImage, PixelFormat};

/// Keep in step with `make_card.py`.
pub(crate) const PQ_NITS: [f32; 6] = [0.0, 100.0, 203.0, 400.0, 1000.0, 4000.0];
pub(crate) const HLG_NITS: [f32; 6] = [0.0, 50.0, 100.0, 203.0, 400.0, 1000.0];
const SDR_WHITE: f32 = 203.0;

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hdr")
        .join(name)
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixture_path(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn nits_for(name: &str) -> [f32; 6] {
    if name.starts_with("hlg") {
        HLG_NITS
    } else {
        PQ_NITS
    }
}

/// The mean of R, G, B at the centre of each of the six patches of a `w`-wide card, read
/// through `px(x, y) -> [r, g, b]`.
fn patches(w: u32, h: u32, px: impl Fn(u32, u32) -> [f32; 3]) -> Vec<f32> {
    let n = PQ_NITS.len() as u32;
    (0..n)
        .map(|k| {
            let [r, g, b] = px(w * (2 * k + 1) / (2 * n), h / 2);
            (r + g + b) / 3.0
        })
        .collect()
}

/// Patch values of a display decode, or `None` when it came back SDR (8-bit).
fn display_patches(img: &DecodedImage) -> Option<Vec<f32>> {
    if img.format != PixelFormat::Rgba16F {
        return None;
    }
    let f = |i: usize| half::f16::from_le_bytes([img.pixels[i], img.pixels[i + 1]]).to_f32();
    Some(patches(img.width, img.height, |x, y| {
        let i = ((y * img.width + x) * 8) as usize;
        [f(i), f(i + 2), f(i + 4)]
    }))
}

/// Patch values of an export decode, or `None` when it isn't linear HDR.
fn export_patches(src: &pb_encode::ExportSource) -> Option<Vec<f32>> {
    let pb_encode::Samples::F32(v) = &src.samples else {
        return None;
    };
    if !src.is_hdr() {
        return None;
    }
    let n = src.channels.count();
    Some(patches(src.width, src.height, |x, y| {
        let i = (y * src.width + x) as usize * n;
        [v[i], v[i + 1], v[i + 2]]
    }))
}

/// `measured / expected` per non-black patch (1.0 = on the 203-nit scale), plus the black
/// patch's raw value.
fn ratios(nits: &[f32; 6], measured: &[f32]) -> (f32, Vec<f32>) {
    let black = measured[0];
    let r = nits
        .iter()
        .zip(measured)
        .skip(1)
        .map(|(n, m)| m / (n / SDR_WHITE))
        .collect();
    (black, r)
}

/// A decode result as a report line: an error, SDR, or the patch ratios.
fn decoded_line(
    label: &str,
    nits: &[f32; 6],
    img: Result<DecodedImage, crate::DecodeError>,
) -> String {
    match img {
        Ok(i) if i.format != PixelFormat::Rgba16F => {
            format!(
                "{label:<34} not HDR: {:?} {}x{}",
                i.format, i.width, i.height
            )
        }
        Ok(i) => line(label, nits, display_patches(&i)),
        Err(e) => format!("{label:<34} error: {e}"),
    }
}

fn line(label: &str, nits: &[f32; 6], measured: Option<Vec<f32>>) -> String {
    match measured {
        None => format!("{label:<34} SDR (not decoded as HDR)"),
        Some(m) => {
            let (black, r) = ratios(nits, &m);
            let cells: Vec<String> = nits[1..]
                .iter()
                .zip(&m[1..])
                .zip(&r)
                .map(|((n, v), r)| format!("{n:>4}:{v:>7.3} ({r:.2}x)"))
                .collect();
            format!("{label:<34} black {black:>6.3} | {}", cells.join("  "))
        }
    }
}

/// Every patch within `tol` (relative) of the 203-nit scale, black within `abs`.
fn assert_on_scale(label: &str, nits: &[f32; 6], measured: &[f32], tol: f32) {
    let (black, r) = ratios(nits, measured);
    assert!(black.abs() < 0.01, "{label}: black patch {black}");
    for (n, r) in nits[1..].iter().zip(&r) {
        assert!(
            (r - 1.0).abs() < tol,
            "{label}: the {n}-nit patch decodes {r:.3}x off the 203-nit scale \
             ({})",
            line(label, nits, Some(measured.to_vec()))
        );
    }
}

const STILLS: [&str; 6] = [
    "pq-card.avif",
    "pq-card.heic",
    "pq-card.jxl",
    "hlg-card.avif",
    "hlg-card.heic",
    "hlg-card.jxl",
];
const PNGS: [&str; 2] = ["pq-card.png", "hlg-card.png"];
const CLIPS: [&str; 2] = ["pq-card.mp4", "hlg-card.mp4"];

/// The whole table, every path this build has. Read the ratios: 1.00x is on scale.
#[test]
#[ignore = "a report: run with --ignored --nocapture"]
fn report() {
    let mut out = vec![String::from(
        "HDR audit — measured value (ratio to nits/203) per patch",
    )];
    for name in STILLS.iter().chain(&PNGS) {
        let nits = nits_for(name);
        let bytes = fixture(name);
        out.push(decoded_line(
            &format!("{name} display"),
            &nits,
            crate::decode_named_bytes(name, &bytes, None, false),
        ));
        let export = match crate::decode_named_bytes_for_export(name, &bytes) {
            Ok(s) => export_patches(&s),
            Err(e) => {
                out.push(format!(
                    "{:<34} export refused: {e}",
                    format!("{name} export")
                ));
                continue;
            }
        };
        out.push(line(&format!("{name} export"), &nits, export));
    }
    #[cfg(target_os = "macos")]
    for name in CLIPS {
        let img = crate::decode_video_poster(
            &fixture_path(name),
            None,
            &std::sync::atomic::AtomicBool::new(false),
        );
        out.push(decoded_line(
            &format!("{name} poster (AVFoundation)"),
            &nits_for(name),
            img,
        ));
    }
    #[cfg(feature = "ffvideo")]
    for name in CLIPS {
        let img = crate::ff_decode_video_poster(
            &crate::VideoInput::Path(fixture_path(name)),
            None,
            &std::sync::atomic::AtomicBool::new(false),
        );
        out.push(decoded_line(
            &format!("{name} poster (FFmpeg)"),
            &nits_for(name),
            img,
        ));
    }
    let _ = CLIPS;
    println!("{}", out.join("\n"));
}

/// The cards themselves: the reference maths (`pq_to_scrgb` / `hlg_to_scrgb`) reads the
/// generator's code values back onto the 203-nit scale, so a failing backend test below is
/// the backend, not the card.
#[test]
fn the_cards_encode_the_intended_brightness() {
    for (name, decode) in [
        ("pq-card.png", crate::color::pq_to_scrgb as fn(f32) -> f32),
        ("hlg-card.png", crate::color::hlg_to_scrgb),
    ] {
        let bytes = fixture(name);
        let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
        dec.set_transformations(png::Transformations::IDENTITY);
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        let (w, h) = (info.width, info.height);
        let measured = patches(w, h, |x, y| {
            let i = ((y * w + x) * 6) as usize;
            let v = u16::from_be_bytes([buf[i], buf[i + 1]]) as f32 / 65535.0;
            [decode(v); 3]
        });
        assert_on_scale(name, &nits_for(name), &measured, 0.01);
    }
}

/// The HDR AVIF + HEIC stills, through the display decode **and** the export decode (#89 —
/// an HDR export tone-maps from these values, so a scale error here is baked into files).
///
/// macOS (Image I/O → `kCGColorSpaceExtendedLinearSRGB`): measured exactly on scale for
/// PQ and HLG (2026-11-14) — Apple's extended-linear 1.0 *is* 203 nits, and its HLG uses
/// the same 1000-nit reference OOTF as `hlg_to_scrgb`. Windows (WIC
/// `128bppRGBAFloat`) is the open half of #146: scRGB's own convention is 1.0 = 80 nits.
#[cfg(any(target_os = "macos", windows))]
#[cfg_attr(
    windows,
    ignore = "task #146, the Windows half: run with --ignored; if it passes, drop this \
              attribute, else normalize wic.rs (and record the factor in the plan)"
)]
#[test]
fn hdr_avif_and_heic_stills_decode_on_the_203_nit_scale() {
    for name in STILLS.iter().filter(|n| !n.ends_with(".jxl")) {
        let nits = nits_for(name);
        let bytes = fixture(name);
        let img = crate::decode_named_bytes(name, &bytes, None, false)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let m = display_patches(&img)
            .unwrap_or_else(|| panic!("{name}: displayed as {:?}, not HDR", img.format));
        assert_on_scale(&format!("{name} display"), &nits, &m, 0.03);
        let src = crate::decode_named_bytes_for_export(name, &bytes)
            .unwrap_or_else(|e| panic!("{name} export: {e}"));
        let m = export_patches(&src).unwrap_or_else(|| panic!("{name}: export isn't HDR"));
        assert_on_scale(&format!("{name} export"), &nits, &m, 0.03);
    }
}

/// HDR MP4/MOV posters on macOS (task #144's AVFoundation path). Measured +2–3% on PQ
/// (2026-11-14; FFmpeg reads the same clip exactly) — about one just-noticeable step,
/// most likely 10-bit range rounding in Apple's YCbCr conversion. Hence 5%, not 3%.
#[cfg(target_os = "macos")]
#[test]
fn avfoundation_hdr_posters_are_on_the_203_nit_scale() {
    for name in CLIPS {
        let img = crate::decode_video_poster(
            &fixture_path(name),
            None,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        let m = display_patches(&img).unwrap_or_else(|| panic!("{name}: poster isn't HDR"));
        assert_on_scale(&format!("{name} poster"), &nits_for(name), &m, 0.05);
    }
}

/// HDR posters through FFmpeg (macOS's MKV/WebM route; Linux) — the same `FrameConverter`
/// the Session playback route uses for RGBA frames. Measured exactly on scale.
#[cfg(feature = "ffvideo")]
#[test]
fn ffmpeg_hdr_posters_are_on_the_203_nit_scale() {
    for name in CLIPS {
        let img = crate::ff_decode_video_poster(
            &crate::VideoInput::Path(fixture_path(name)),
            None,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        let m = display_patches(&img).unwrap_or_else(|| panic!("{name}: poster isn't HDR"));
        assert_on_scale(&format!("{name} poster"), &nits_for(name), &m, 0.03);
    }
}

/// HDR posters through Media Foundation (Windows) — the other open half of #146.
#[cfg(windows)]
#[test]
#[ignore = "task #146, the Windows half: run with --ignored; if it passes, drop this attribute"]
fn media_foundation_hdr_posters_are_on_the_203_nit_scale() {
    for name in CLIPS {
        let img = crate::decode_video_poster(
            &fixture_path(name),
            None,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        let m = display_patches(&img).unwrap_or_else(|| panic!("{name}: poster isn't HDR"));
        assert_on_scale(&format!("{name} poster"), &nits_for(name), &m, 0.05);
    }
}

/// HDR PNGs (`cICP` PQ/HLG, `png_hdr`) — pure Rust, so the same on every platform. Before
/// `png_hdr` these displayed their raw code values as SDR (the audit's first finding).
#[test]
fn hdr_pngs_decode_on_the_203_nit_scale() {
    for name in PNGS {
        let nits = nits_for(name);
        let bytes = fixture(name);
        let img = crate::decode_named_bytes(name, &bytes, None, false)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let m = display_patches(&img)
            .unwrap_or_else(|| panic!("{name}: displayed as {:?}, not HDR", img.format));
        assert_on_scale(&format!("{name} display"), &nits, &m, 0.01);
        assert!(img.peak > 4.0, "{name}: a real HDR peak, got {}", img.peak);
        let src = crate::decode_named_bytes_for_export(name, &bytes)
            .unwrap_or_else(|e| panic!("{name} export: {e}"));
        let m = export_patches(&src).unwrap_or_else(|| panic!("{name}: export isn't HDR"));
        assert_on_scale(&format!("{name} export"), &nits, &m, 0.01);
    }
}
