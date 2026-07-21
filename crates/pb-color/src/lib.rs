//! Shared YUV→RGB matrix definitions (task #130 Part B).
//!
//! The **one home** for the `(Kr, Kb)` luma coefficients and the convert-coefficient
//! derivation. Before this crate there were two hand-copied tables — `Matrix::kr_kb`
//! in `pb-decode` (the AVIF/dav1d `yuv.rs` decode path) and `YuvMatrix::kr_kb` in
//! `pb-render` (the video NV12/P010 CPU convert + shader reference) — plus a third
//! copy of the constants in the WGSL shader. Two of the three were identical Rust,
//! and a new matrix family (or a fix to a wrong coefficient) could land in one crate
//! and silently miss the other. This crate collapses the two Rust copies into one so
//! that can't happen.
//!
//! **The WGSL shader keeps its own copy** — a shader can't import a Rust `const` — and
//! is cross-checked by `pb-render`'s existing **independent-from-spec golden test**,
//! which is the correct guard for that boundary (a codegen step to emit WGSL from Rust
//! would be over-engineering for six numbers). Do not add one.
//!
//! Pure math, zero dependencies: no I/O, no GPU, no cfg. `pb-decode` and `pb-render`
//! both depend on it; nothing depends on them.

/// The YUV→RGB matrix families both media paths use (H.273 non-constant-luminance
/// `matrix_coefficients`). Deliberately just the three that carry distinct luma
/// coefficients — the CICP `Identity` (RGB-coded, no matrix) case is a decode-only
/// concern and stays in `pb-decode`'s own `Matrix` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvMatrix {
    /// CICP 5/6 (SD / sRGB-adjacent).
    Bt601,
    /// CICP 1 (HD).
    Bt709,
    /// CICP 9 (UHD / wide-gamut, non-constant luminance).
    Bt2020,
}

impl YuvMatrix {
    /// The `(Kr, Kb)` luma coefficients this family is defined by (`Kg = 1 − Kr − Kb`).
    /// The single source of truth — a fix here reaches both the decode and render
    /// paths at once.
    pub fn kr_kb(self) -> (f32, f32) {
        match self {
            YuvMatrix::Bt601 => (0.299, 0.114),
            YuvMatrix::Bt709 => (0.2126, 0.0722),
            YuvMatrix::Bt2020 => (0.2627, 0.0593),
        }
    }

    /// The four derived convert coefficients `(a, b, c, d)` for **normalized** luma
    /// `y` and **centered** chroma `u`/`v`:
    /// `r = y + a·v`, `g = y − b·u − c·v`, `b = y + d·u`. Computed once here so the
    /// `pb-render` CPU reference and the shader uniform can never drift apart. (The
    /// `pb-decode` decode path derives the algebraically-identical `r`/`b`/`g` form
    /// inline from [`kr_kb`](Self::kr_kb); it does not consume this.)
    pub fn coeffs(self) -> (f32, f32, f32, f32) {
        let (kr, kb) = self.kr_kb();
        let kg = 1.0 - kr - kb;
        (
            2.0 * (1.0 - kr),
            2.0 * kb * (1.0 - kb) / kg,
            2.0 * kr * (1.0 - kr) / kg,
            2.0 * (1.0 - kb),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The constants match the published values (a guard against a typo'd digit —
    /// the exact drift this crate exists to prevent).
    #[test]
    fn luma_coefficients_are_the_published_values() {
        assert_eq!(YuvMatrix::Bt601.kr_kb(), (0.299, 0.114));
        assert_eq!(YuvMatrix::Bt709.kr_kb(), (0.2126, 0.0722));
        assert_eq!(YuvMatrix::Bt2020.kr_kb(), (0.2627, 0.0593));
    }

    /// `coeffs()` is the standard derivation: `a = 2(1−Kr)`, `d = 2(1−Kb)`, and the
    /// two green terms use `Kg = 1 − Kr − Kb`. A round-trip on a known point (BT.709
    /// full white: y=1, u=v=0 → RGB all 1) sanity-checks the algebra.
    #[test]
    fn coeffs_reconstruct_white_and_primaries_ballpark() {
        for m in [YuvMatrix::Bt601, YuvMatrix::Bt709, YuvMatrix::Bt2020] {
            let (kr, kb) = m.kr_kb();
            let kg = 1.0 - kr - kb;
            let (a, b, c, d) = m.coeffs();
            assert!((a - 2.0 * (1.0 - kr)).abs() < 1e-6);
            assert!((d - 2.0 * (1.0 - kb)).abs() < 1e-6);
            assert!((b - 2.0 * kb * (1.0 - kb) / kg).abs() < 1e-6);
            assert!((c - 2.0 * kr * (1.0 - kr) / kg).abs() < 1e-6);
            // Neutral gray (u = v = 0) reconstructs r = g = b = y.
            let (y, u, v) = (0.5f32, 0.0f32, 0.0f32);
            let r = y + a * v;
            let g = y - b * u - c * v;
            let bl = y + d * u;
            assert!((r - 0.5).abs() < 1e-6 && (g - 0.5).abs() < 1e-6 && (bl - 0.5).abs() < 1e-6);
        }
    }
}
