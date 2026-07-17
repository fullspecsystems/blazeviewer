//! Dev-only: report the door artwork's bounding box at a sweep of alpha thresholds — i.e.
//! how much of the asset the crop actually removes, and how much of what's left is folder
//! rather than shadow. The card's artwork cap is derived from this (a cap sizes the whole
//! *frame*, but what the eye measures is the folder inside it), so when the asset changes,
//! run this before re-guessing the numbers.
//!
//! Note it reports the artwork **as `door_artwork()` hands it out** — already cropped — so
//! a tight `alpha > 0` box spanning the whole thing is the expected, correct output.
fn main() {
    let a = pb_app_core::engine::door_artwork().expect("artwork decodes");
    let (w, h) = (a.width as usize, a.height as usize);
    println!("asset: {w}x{h}");
    println!(
        "{:>6}  {:>16}  {:>10}  {:>26}",
        "alpha", "box", "size", "margins (l/r/t/b)"
    );
    for min_alpha in [1u8, 2, 4, 8, 16, 32, 64, 128, 200] {
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
        for y in 0..h {
            for x in 0..w {
                if a.pixels[(y * w + x) * 4 + 3] >= min_alpha {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        if x0 > x1 {
            println!("{min_alpha:>6}  (nothing)");
            continue;
        }
        println!(
            "{min_alpha:>6}  {:>16}  {:>10}  {:>26}",
            format!("x {x0}..{x1} y {y0}..{y1}"),
            format!("{}x{}", x1 - x0 + 1, y1 - y0 + 1),
            format!("{} / {} / {} / {}", x0, w - 1 - x1, y0, h - 1 - y1),
        );
    }
}
