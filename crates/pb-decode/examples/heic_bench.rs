//! THROWAWAY measurement harness for the HEIC-decode track. Three jobs:
//!   1. `probe`   — walk the ISOBMFF `meta` box, list items + dimensions + refs.
//!   2. single    — warm single-thread full decode-to-fit latency.
//!   3. concurrent — aggregate throughput across N threads (the serialization test).
//!
//!   cargo run -q --release --example heic_bench -p pb-decode -- <file> [threads]
//!   (add `--features libheif` to measure the libheif backend; `PB_HEIC_BACKEND=wic`
//!    forces WIC for the A/B. Throwaway A/B tool, kept for the Phase 3 prefetch work.)

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use pb_decode::{decode_image_file, FitBox};

// The CLAUDE.md display target (decode-to-fit box for Fit mode).
const FIT: FitBox = FitBox {
    max_width: 7680,
    max_height: 3840,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: heic_bench <file> [threads]");
        return;
    }
    let path = Arc::<Path>::from(Path::new(&args[0]));
    let threads: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(8);

    let bytes = std::fs::read(&path).expect("read file");
    println!("file: {} ({} bytes)\n", path.display(), bytes.len());
    probe(&bytes);

    // Preview-path latency (allow_preview = true) — what the blaze-by actually shows.
    // If this is slow and returns full-res dims, the file has no fast thumbnail and
    // the "preview" is really a full decode (the Sony-HEIC trap).
    println!("\n=== preview path (allow_preview=true) ===");
    let mut prev_ms = Vec::new();
    let mut prev_dims = (0u32, 0u32);
    for _ in 0..5 {
        let t = Instant::now();
        let img = decode_image_file(&path, Some(FIT), true).expect("preview decode");
        prev_ms.push(t.elapsed().as_secs_f64() * 1e3);
        prev_dims = (img.width, img.height);
    }
    prev_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "  median {:.1} ms  -> decoded {}x{} ({})",
        prev_ms[prev_ms.len() / 2],
        prev_dims.0,
        prev_dims.1,
        if prev_dims.0 <= 640 {
            "real thumbnail = FAST preview"
        } else {
            "FULL-RES = no thumbnail, preview path is a full decode!"
        }
    );

    // Warm up + single-thread latency (median of 5).
    println!(
        "\n=== single-thread full decode-to-fit (fit {}x{}) ===",
        FIT.max_width, FIT.max_height
    );
    let mut samples = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let img = decode_image_file(&path, Some(FIT), false).expect("decode");
        let ms = t.elapsed().as_secs_f64() * 1e3;
        samples.push(ms);
        // print first to confirm dims/codec
        if samples.len() == 1 {
            println!(
                "  decoded {}x{} ({}), {} px-bytes",
                img.orig_width,
                img.orig_height,
                img.codec,
                img.pixels.len()
            );
        }
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let single_ms = samples[samples.len() / 2];
    println!(
        "  per-decode ms: {:?}",
        samples
            .iter()
            .map(|m| (m * 10.0).round() / 10.0)
            .collect::<Vec<_>>()
    );
    println!(
        "  median single-thread: {:.1} ms  ({:.1}/s)",
        single_ms,
        1000.0 / single_ms
    );

    // Concurrent throughput: each of `threads` workers decodes the same file in a
    // loop for a fixed wall window; report aggregate decodes/s and the speedup vs
    // the single-thread rate. Speedup << threads ⇒ the decoder serializes.
    println!(
        "\n=== concurrent throughput, {} threads, 4.0s window ===",
        threads
    );
    let window = std::time::Duration::from_secs_f64(4.0);
    let start = Instant::now();
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let p = path.clone();
            std::thread::spawn(move || {
                let mut n = 0u64;
                while start.elapsed() < window {
                    let _ = decode_image_file(&p, Some(FIT), false);
                    n += 1;
                }
                n
            })
        })
        .collect();
    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    let secs = start.elapsed().as_secs_f64();
    let rate = total as f64 / secs;
    let ideal = 1000.0 / single_ms;
    println!(
        "  {} decodes in {:.2}s = {:.1}/s aggregate",
        total, secs, rate
    );
    println!(
        "  speedup vs single-thread: {:.2}x  (ideal ~{:.1}x = perfect scaling)",
        rate / ideal,
        threads as f64
    );
    println!("  per-thread effective: {:.1}/s", rate / threads as f64);
}

// ---------------------------------------------------------------------------
// Minimal ISOBMFF `meta` walker — just enough to list items, their types, their
// dimensions (via ipco/ispe + ipma association), and the iref relationships.
// ---------------------------------------------------------------------------

fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
fn be16(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}
fn typ(b: &[u8], i: usize) -> String {
    String::from_utf8_lossy(&b[i..i + 4]).to_string()
}

#[derive(Default)]
struct Probe {
    primary: u32,
    infe: Vec<(u32, String, String)>,        // id, type, name
    ipco: Vec<(String, Option<(u32, u32)>)>, // child type, ispe dims (1-based index = property id)
    ipma: Vec<(u32, Vec<u16>)>,              // item_id -> property indices (1-based)
    iref: Vec<(String, u32, Vec<u32>)>,      // ref type, from, to[]
}

fn probe(buf: &[u8]) {
    let mut p = Probe::default();
    // Find the top-level `meta` box, then walk its children.
    walk_top(buf, &mut p);

    // Build item_id -> dims from ipma + ipco(ispe).
    let dims = |item: u32| -> Option<(u32, u32)> {
        let props = p.ipma.iter().find(|(id, _)| *id == item).map(|(_, v)| v)?;
        for &pi in props {
            let idx = (pi as usize).checked_sub(1)?;
            if let Some((t, Some(d))) = p.ipco.get(idx) {
                if t == "ispe" {
                    return Some(*d);
                }
            }
        }
        None
    };

    println!("=== items (primary = {}) ===", p.primary);
    println!("  {:>4}  {:<6} {:<11} {:<20}", "id", "type", "dims", "name");
    // sort by id
    let mut items = p.infe.clone();
    items.sort_by_key(|(id, _, _)| *id);
    for (id, t, name) in &items {
        let d = dims(*id)
            .map(|(w, h)| format!("{}x{}", w, h))
            .unwrap_or_else(|| "-".into());
        let star = if *id == p.primary { " <- PRIMARY" } else { "" };
        println!("  {:>4}  {:<6} {:<11} {:<20}{}", id, t, d, name, star);
    }

    println!("\n=== iref (relationships) ===");
    for (t, from, to) in &p.iref {
        println!("  {:<6} from {:>3} -> {:?}", t, from, to);
    }

    // Distinct ispe sizes (quick "what resolutions exist" view).
    let mut sizes: Vec<(u32, u32)> = p
        .ipco
        .iter()
        .filter_map(|(t, d)| if t == "ispe" { *d } else { None })
        .collect();
    sizes.sort();
    sizes.dedup();
    println!("\n=== distinct ispe resolutions ===");
    for (w, h) in sizes {
        println!("  {}x{}  ({:.1} MP)", w, h, (w as f64 * h as f64) / 1e6);
    }
}

/// Walk top-level boxes, recursing only into `meta`.
fn walk_top(buf: &[u8], p: &mut Probe) {
    let mut i = 0usize;
    while i + 8 <= buf.len() {
        let size = be32(buf, i) as usize;
        let t = typ(buf, i + 4);
        let (payload, next) = box_bounds(buf, i, size);
        if t == "meta" {
            // meta is a FullBox: 4 bytes version/flags before children.
            if payload + 4 <= buf.len() {
                walk_meta(buf, payload + 4, next, p);
            }
        }
        if next <= i {
            break;
        }
        i = next;
    }
}

/// Given a box at `i` with declared `size`, return (payload_start, next_box).
fn box_bounds(buf: &[u8], i: usize, size: usize) -> (usize, usize) {
    if size == 1 {
        // 64-bit largesize at i+8..i+16
        let large = u64::from_be_bytes(buf[i + 8..i + 16].try_into().unwrap_or([0; 8])) as usize;
        (i + 16, (i + large).min(buf.len()))
    } else if size == 0 {
        (i + 8, buf.len())
    } else {
        (i + 8, (i + size).min(buf.len()))
    }
}

fn walk_meta(buf: &[u8], start: usize, end: usize, p: &mut Probe) {
    let mut i = start;
    while i + 8 <= end {
        let size = be32(buf, i) as usize;
        let t = typ(buf, i + 4);
        let (payload, next) = box_bounds(buf, i, size);
        match t.as_str() {
            "pitm" => {
                // FullBox; version 0 -> u16 id, version 1 -> u32 id.
                let ver = buf[payload];
                p.primary = if ver == 0 {
                    be16(buf, payload + 4) as u32
                } else {
                    be32(buf, payload + 4)
                };
            }
            "iinf" => parse_iinf(buf, payload, next, p),
            "iref" => parse_iref(buf, payload, next, p),
            "iprp" => walk_iprp(buf, payload, next, p),
            _ => {}
        }
        if next <= i {
            break;
        }
        i = next;
    }
}

fn parse_iinf(buf: &[u8], payload: usize, end: usize, p: &mut Probe) {
    // FullBox: version(1)+flags(3); count u16 (v0) or u32 (v1+); then infe children.
    let ver = buf[payload];
    let mut i = if ver == 0 { payload + 6 } else { payload + 8 };
    while i + 8 <= end {
        let size = be32(buf, i) as usize;
        let t = typ(buf, i + 4);
        let (cpayload, next) = box_bounds(buf, i, size);
        if t == "infe" {
            parse_infe(buf, cpayload, next, p);
        }
        if next <= i {
            break;
        }
        i = next;
    }
}

fn parse_infe(buf: &[u8], payload: usize, end: usize, p: &mut Probe) {
    // FullBox. version 2: item_id u16; version 3: item_id u32. Then
    // protection_index u16, item_type 4cc, then null-terminated name.
    if payload >= buf.len() {
        return;
    }
    let ver = buf[payload];
    let mut i = payload + 4;
    let id = if ver == 2 {
        let v = be16(buf, i) as u32;
        i += 2;
        v
    } else {
        let v = be32(buf, i);
        i += 4;
        v
    };
    i += 2; // protection_index
    if i + 4 > end {
        return;
    }
    let item_type = typ(buf, i);
    i += 4;
    // name: null-terminated
    let mut name = String::new();
    while i < end && buf[i] != 0 {
        name.push(buf[i] as char);
        i += 1;
    }
    p.infe.push((id, item_type, name));
}

fn parse_iref(buf: &[u8], payload: usize, end: usize, p: &mut Probe) {
    // FullBox: version determines id width (v0 u16, v1 u32). Children are
    // SingleItemTypeReference boxes: from_item, ref_count u16, to_item[].
    let ver = buf[payload];
    let wide = ver != 0;
    let mut i = payload + 4;
    while i + 8 <= end {
        let size = be32(buf, i) as usize;
        let t = typ(buf, i + 4);
        let (cpayload, next) = box_bounds(buf, i, size);
        let mut j = cpayload;
        let from = if wide {
            let v = be32(buf, j);
            j += 4;
            v
        } else {
            let v = be16(buf, j) as u32;
            j += 2;
            v
        };
        let count = be16(buf, j) as usize;
        j += 2;
        let mut to = Vec::new();
        for _ in 0..count {
            if wide {
                if j + 4 > next {
                    break;
                }
                to.push(be32(buf, j));
                j += 4;
            } else {
                if j + 2 > next {
                    break;
                }
                to.push(be16(buf, j) as u32);
                j += 2;
            }
        }
        p.iref.push((t, from, to));
        if next <= i {
            break;
        }
        i = next;
    }
}

fn walk_iprp(buf: &[u8], start: usize, end: usize, p: &mut Probe) {
    let mut i = start;
    while i + 8 <= end {
        let size = be32(buf, i) as usize;
        let t = typ(buf, i + 4);
        let (payload, next) = box_bounds(buf, i, size);
        match t.as_str() {
            "ipco" => parse_ipco(buf, payload, next, p),
            "ipma" => parse_ipma(buf, payload, next, p),
            _ => {}
        }
        if next <= i {
            break;
        }
        i = next;
    }
}

fn parse_ipco(buf: &[u8], start: usize, end: usize, p: &mut Probe) {
    let mut i = start;
    while i + 8 <= end {
        let size = be32(buf, i) as usize;
        let t = typ(buf, i + 4);
        let (payload, next) = box_bounds(buf, i, size);
        let dims = if t == "ispe" {
            // FullBox: version/flags(4), width u32, height u32.
            Some((be32(buf, payload + 4), be32(buf, payload + 8)))
        } else {
            None
        };
        p.ipco.push((t, dims));
        if next <= i {
            break;
        }
        i = next;
    }
}

fn parse_ipma(buf: &[u8], payload: usize, end: usize, p: &mut Probe) {
    // FullBox: version/flags(4); entry_count u32; per entry: item_id (u16 v0 / u32 v1),
    // association_count u8, then per assoc: 1 or 2 bytes (flags&1 -> 2 bytes), low
    // 7/15 bits = property index.
    let ver = buf[payload];
    let flags = be32(buf, payload) & 0x00ff_ffff;
    let wide_assoc = (flags & 1) != 0;
    let mut i = payload + 4;
    let count = be32(buf, i);
    i += 4;
    for _ in 0..count {
        if i + 4 > end {
            break;
        }
        let item = if ver == 0 {
            let v = be16(buf, i) as u32;
            i += 2;
            v
        } else {
            let v = be32(buf, i);
            i += 4;
            v
        };
        if i >= end {
            break;
        }
        let acount = buf[i] as usize;
        i += 1;
        let mut props = Vec::new();
        for _ in 0..acount {
            if wide_assoc {
                if i + 2 > end {
                    break;
                }
                let v = be16(buf, i) & 0x7fff;
                props.push(v);
                i += 2;
            } else {
                if i + 1 > end {
                    break;
                }
                let v = (buf[i] & 0x7f) as u16;
                props.push(v);
                i += 1;
            }
        }
        p.ipma.push((item, props));
    }
}
