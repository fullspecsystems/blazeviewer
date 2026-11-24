//! Render a 3D model (STL / OBJ / 3MF) to a PNG through the real decode path, to eyeball the
//! still renderer (task #151) without launching the app. Dev-only; writes only the PNG you name.
//!
//! ```sh
//! cargo run -p pb-decode --release --example model_png -- part.3mf out.png [width height]
//! cargo run -p pb-decode --release --example model_png -- part.3mf out.png 5120 2880 400 -100
//! ```
//!
//! Two more numbers turn the model first, as a drag of that many pixels would, and render
//! through the turned-view path Export and Copy use (task #153.6): the whole viewport, at
//! exactly `width`×`height`.

use std::time::Instant;

use image::ImageEncoder;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: model_png <model.stl|obj|3mf> <out.png> [width height [drag_x drag_y]]");
        std::process::exit(2);
    }
    let (w, h) = match (args.get(3), args.get(4)) {
        (Some(w), Some(h)) => (w.parse().expect("width"), h.parse().expect("height")),
        _ => (1600, 1200),
    };
    let bytes = std::fs::read(&args[1]).expect("read model");
    let fit = pb_decode::FitBox {
        max_width: w,
        max_height: h,
    };
    let turned = match (args.get(5), args.get(6)) {
        (Some(x), Some(y)) => {
            let (dx, dy): (f32, f32) = (x.parse().expect("drag_x"), y.parse().expect("drag_y"));
            let format = pb_mesh::ModelFormat::from_extension(
                std::path::Path::new(&args[1])
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or(""),
            )
            .expect("a model extension");
            let model = pb_mesh::parse(format, &bytes, &pb_mesh::Limits::default()).expect("parse");
            let vp = [w as f32, h as f32];
            let mut view = pb_mesh::LiveView::new(&model).expect("something to draw");
            view.orbit_by(dx, dy, vp);
            Some(view.frame(vp))
        }
        _ => None,
    };
    let t = Instant::now();
    let decoded = match &turned {
        Some(frame) => pb_decode::decode_model_view(&args[1], &bytes, frame, w, h),
        None => pb_decode::decode_named_bytes(&args[1], &bytes, Some(fit), false),
    };
    let img = match decoded {
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
