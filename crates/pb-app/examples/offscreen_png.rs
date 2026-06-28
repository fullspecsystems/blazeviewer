//! TEMP: render a decoded image through the real offscreen pipeline to raw RGBA,
//! to verify the render path independent of the swapchain / screen capture.
//!   cargo run -q --example offscreen_png -p pb-app -- <in-image> <out.rgba>
use std::path::Path;

use pb_decode::decode_image_file;
use pb_render::{render_offscreen_color, ColorTransform};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let img = decode_image_file(Path::new(&args[0]), None, false).expect("decode");
    let color = ColorTransform {
        matrix: img.color.matrix,
        trc: img.color.trc,
        enabled: img.color.enabled,
    };
    let out = render_offscreen_color(&img.pixels, img.width, img.height, 1280, 800, color);
    std::fs::write(&args[1], &out).expect("write");
    println!(
        "{}x{} enabled={} -> {} ({} bytes)",
        img.width,
        img.height,
        img.color.enabled,
        args[1],
        out.len()
    );
}
