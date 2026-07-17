//! Dev-only: characterize the door artwork's alpha channel — is the soft edge a *designed*
//! drop shadow (a smooth gradient), or a lossy-WebP artifact (color bleed under a faint,
//! noisy alpha)? Reports the raw asset, before any crop.
fn main() {
    // Decode the raw asset directly, uncropped, so we see exactly what was exported. An
    // optional path arg decodes some *other* file (routed by its extension), so the same
    // asset can be compared across decoders — e.g. the ImageIO-converted PNG vs our WebP.
    let img = if let Some(path) = std::env::args().nth(1) {
        let bytes = std::fs::read(&path).expect("read the file");
        let name = std::path::Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "in.png".into());
        println!("decoding: {path}\n");
        pb_decode::decode_named_bytes(&name, &bytes, None, false).expect("decodes")
    } else {
        #[cfg(windows)]
        let bytes = include_bytes!("../assets/folder-zip-yellow.webp").as_slice();
        #[cfg(not(windows))]
        let bytes = include_bytes!("../assets/folder-zip-blue.webp").as_slice();
        println!("decoding: the embedded WebP asset (via pb_decode)\n");
        pb_decode::decode_named_bytes("door.webp", bytes, None, false).expect("decodes")
    };
    let (w, h) = (img.width as usize, img.height as usize);
    let a = |x: usize, y: usize| img.pixels[(y * w + x) * 4 + 3];
    let rgb = |x: usize, y: usize| {
        let i = (y * w + x) * 4;
        (img.pixels[i], img.pixels[i + 1], img.pixels[i + 2])
    };

    // 1. Alpha histogram (coarse buckets).
    let mut buckets = [0u64; 9]; // 0, 1-3, 4-15, 16-31, 32-63, 64-127, 128-200, 201-254, 255
    for p in img.pixels.chunks_exact(4) {
        let al = p[3];
        let b = match al {
            0 => 0,
            1..=3 => 1,
            4..=15 => 2,
            16..=31 => 3,
            32..=63 => 4,
            64..=127 => 5,
            128..=200 => 6,
            201..=254 => 7,
            255 => 8,
        };
        buckets[b] += 1;
    }
    let total = (w * h) as f64;
    let labels = [
        "0 (clear)",
        "1-3 (haze?)",
        "4-15",
        "16-31",
        "32-63",
        "64-127",
        "128-200",
        "201-254",
        "255 (solid)",
    ];
    println!("asset {w}x{h}  —  alpha histogram:");
    for (l, c) in labels.iter().zip(buckets) {
        println!("  {l:>14}: {c:>9}  ({:>5.2}%)", c as f64 / total * 100.0);
    }

    // 2. A horizontal scanline through the vertical middle — is alpha a smooth ramp into the
    //    folder (a shadow), or does it jump? Sample every ~4% of the width.
    let y = h / 2;
    println!("\nscanline y={y} (alpha at 4% steps across):");
    print!("  ");
    for k in 0..=25 {
        let x = (k * (w - 1)) / 25;
        print!("{:>4}", a(x, y));
    }
    println!();

    // 3. The colour under the faint alpha in the margin — a designed shadow is near-black;
    //    lossy colour-bleed is the *folder's* blue/manila smeared out past its edge.
    println!("\nfaint-alpha (1..=8) pixel colours, top-left margin quadrant:");
    let mut shown = 0;
    'outer: for yy in (0..h / 2).step_by(3) {
        for xx in (0..w / 2).step_by(3) {
            let al = a(xx, yy);
            if (1..=8).contains(&al) {
                let (r, g, b) = rgb(xx, yy);
                println!("  ({xx:>4},{yy:>4}) a={al:>3}  rgb=({r:>3},{g:>3},{b:>3})");
                shown += 1;
                if shown >= 12 {
                    break 'outer;
                }
            }
        }
    }
    if shown == 0 {
        println!("  (none — the margin is fully clear)");
    }

    // 4. The decisive test: composite the artwork over solid backgrounds the *correct* way
    //    (straight-alpha, in sRGB/perceptual space — what CoreGraphics and Affinity do) and
    //    write it out. If this looks tight, a broad shadow on screen is a *rendering* bug,
    //    not the data; if it looks broad, the data carries the shadow and Finder is only
    //    hiding it on its grey chrome.
    for (label, bg) in [("white", [255u8, 255, 255]), ("gray235", [235, 235, 235])] {
        let mut out = vec![0u8; w * h * 3];
        for (i, px) in img.pixels.chunks_exact(4).enumerate() {
            let a = px[3] as u32;
            for c in 0..3 {
                // over = fg*a + bg*(1-a), straight alpha, 8-bit perceptual (no linearize).
                out[i * 3 + c] = ((px[c] as u32 * a + bg[c] as u32 * (255 - a)) / 255) as u8;
            }
        }
        let path = format!("/tmp/door_over_{label}.png");
        let file = std::fs::File::create(&path).expect("create png");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .expect("header")
            .write_image_data(&out)
            .expect("write");
        println!("\nwrote {path}");
    }
}
