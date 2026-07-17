//! Dump the archive **door** tile (task #104) to a PNG so it can be *looked at*.
//!
//! The unit tests can only prove pixels changed — they cannot tell you whether the
//! glyph reads as an archive, is centred, or is the right weight. Run this and open
//! the file.
//!
//! ```sh
//! cargo run -p pb-app-core --example door_shot -- door.png
//! ```
//!
//! Dev-only, like `dump_preview` — never shipped.

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "door.png".to_string());
    let fit = pb_decode::FitBox {
        max_width: std::env::args()
            .nth(2)
            .and_then(|v| v.parse().ok())
            .unwrap_or(1280),
        max_height: std::env::args()
            .nth(3)
            .and_then(|v| v.parse().ok())
            .unwrap_or(960),
    };
    let img = pb_app_core::engine::archive_placeholder(pb_source::ArchiveKind::Zip, Some(fit));

    let file = std::fs::File::create(&out).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.width, img.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .expect("png header")
        .write_image_data(&img.pixels)
        .expect("png data");

    println!(
        "wrote {out} — {}x{}, codec {:?}",
        img.width, img.height, img.codec
    );
}
