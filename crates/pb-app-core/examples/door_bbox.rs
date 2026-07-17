//! Dev-only: report the door artwork's opaque bounding box, i.e. how much of the square
//! asset is transparent margin. The card's spacing depends on it.
fn main() {
    let a = pb_app_core::engine::door_artwork().expect("artwork decodes");
    let (w, h) = (a.width as usize, a.height as usize);
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
    let (mut sx0, mut sy0, mut sx1, mut sy1) = (w, h, 0usize, 0usize);
    for y in 0..h {
        for x in 0..w {
            let al = a.pixels[(y * w + x) * 4 + 3];
            if al > 0 {
                sx0 = sx0.min(x);
                sy0 = sy0.min(y);
                sx1 = sx1.max(x);
                sy1 = sy1.max(y);
            }
            if al > 200 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    println!("asset            : {w}x{h}");
    println!(
        "any alpha > 0    : x {sx0}..{sx1}  y {sy0}..{sy1}   ({}x{})",
        sx1 - sx0 + 1,
        sy1 - sy0 + 1
    );
    println!(
        "near-opaque >200 : x {x0}..{x1}  y {y0}..{y1}   ({}x{})",
        x1 - x0 + 1,
        y1 - y0 + 1
    );
    println!(
        "margins (opaque) : left {x0}  right {}  top {y0}  bottom {}",
        w - 1 - x1,
        h - 1 - y1
    );
}
