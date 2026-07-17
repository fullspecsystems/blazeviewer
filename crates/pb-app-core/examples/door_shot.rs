//! Dump the archive **door** tile (task #104) to a PNG so it can be *looked at* — the
//! artwork composited onto its backdrop, exactly as the deck shows it.
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
    let img = pb_app_core::engine::archive_placeholder(pb_source::ArchiveKind::Zip);

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
