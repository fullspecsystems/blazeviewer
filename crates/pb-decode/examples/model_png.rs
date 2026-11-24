//! Render a 3D model (STL / OBJ / 3MF) to a PNG through the real decode path, to eyeball the
//! still renderer (task #151) without launching the app. Dev-only; writes only the PNG you name.
//!
//! ```sh
//! cargo run -p pb-decode --release --example model_png -- part.3mf out.png [width height]
//! ```

use std::time::Instant;

use image::ImageEncoder;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: model_png <model.stl|obj|3mf> <out.png> [width height]");
        std::process::exit(2);
    }
    let (w, h) = match (args.get(3), args.get(4)) {
        (Some(w), Some(h)) => (w.parse().expect("width"), h.parse().expect("height")),
        _ => (1600, 1200),
    };
    let bytes = std::fs::read(&args[1]).expect("read model");
    let t = Instant::now();
    let fit = pb_decode::FitBox {
        max_width: w,
        max_height: h,
    };
    let img = match pb_decode::decode_named_bytes(&args[1], &bytes, Some(fit), false) {
        Ok(img) => img,
        Err(e) => {
            eprintln!("decode failed: {e:?}");
            std::process::exit(1);
        }
    };
    let elapsed = t.elapsed();
    let mut out = std::fs::File::create(&args[2]).expect("create png");
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            &img.pixels,
            img.width,
            img.height,
            image::ExtendedColorType::Rgba8,
        )
        .expect("encode png");
    eprintln!(
        "{} {}x{} in {:.1} ms ({} bytes in)",
        img.codec,
        img.width,
        img.height,
        elapsed.as_secs_f64() * 1000.0,
        bytes.len()
    );
}
