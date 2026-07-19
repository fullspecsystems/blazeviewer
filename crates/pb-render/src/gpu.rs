//! wgpu presenter (DX12 backend on Windows) + a headless render for golden tests.
//!
//! One image is drawn as a textured quad, letterboxed to the screen via
//! `fit_rect` (no crop), over a dark clear color. `WgpuRenderer` presents to a
//! window surface; `render_offscreen` renders to a buffer for tests.

use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::upload::{StagingUpload, UploadStrategy};
use crate::{
    ColorTransform, DeriveSource, DerivedFit, PlanarPresentation, RenderError, Renderer,
    ViewTransform,
};

/// Background (letterbox) color, straight RGBA8.
pub const LETTERBOX: [u8; 4] = [10, 10, 12, 255];

/// The HDR-capable intermediate the scene renders into: linear light, BT.709
/// primaries, extended range (scRGB). Wide-gamut / HDR values survive here as
/// numbers outside [0,1]; the tone-map pass (SDR) or native scRGB swapchain (HDR,
/// later) turns it into final pixels.
const INTERMEDIATE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Scene pass: photo texel → scene-linear scRGB, written to the fp16 intermediate.
/// `mode` (cx.p1.w): 0 = sRGB-encoded (linearize sRGB, identity primaries),
/// 1 = profiled (linearize the TRC, apply the source→BT.709 matrix), 2 = already
/// scene-linear (HDR fp16) → passthrough. No clamping — wide-gamut colors keep
/// their out-of-[0,1] values for the HDR path.
const SCENE_WGSL: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@location(0) position: vec2<f32>, @location(1) uv: vec2<f32>) -> VsOut {
    var o: VsOut;
    o.pos = vec4<f32>(position, 0.0, 1.0);
    o.uv = uv;
    return o;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct ColorXf {
    r0: vec4<f32>,
    r1: vec4<f32>,
    r2: vec4<f32>,
    p0: vec4<f32>,
    p1: vec4<f32>,
    scale: vec4<f32>,   // .x = output scale (SDR→HDR-surface white level, else 1.0)
    range: vec4<f32>,   // planar range expand: (y_black, y_scale, c_center, c_scale)
};
@group(0) @binding(2) var<uniform> cx: ColorXf;

// Source EOTF (encoded -> linear), evaluated exactly as moxcms::ParametricCurve.
fn eotf(x: f32) -> f32 {
    if (x < cx.p1.x) {
        return cx.p0.w * x + cx.p1.z;                          // c*x + f
    }
    return pow(cx.p0.y * x + cx.p0.z, cx.p0.x) + cx.p1.y;      // (a*x + b)^g + e
}

fn srgb_to_linear(x: f32) -> f32 {
    if (x <= 0.04045) {
        return x / 12.92;
    }
    return pow((x + 0.055) / 1.055, 2.4);
}

// SMPTE ST 2084 (PQ) EOTF: encoded [0,1] -> scene-linear scRGB (1.0 = 203 nits).
// Bit-for-bit the CPU `pb_render::yuv::pq_eotf` / `pb_decode ...::pq_to_scrgb`.
fn pq_eotf(x: f32) -> f32 {
    let m1 = 2610.0 / 16384.0;
    let m2 = 2523.0 / 4096.0 * 128.0;
    let c1 = 3424.0 / 4096.0;
    let c2 = 2413.0 / 4096.0 * 32.0;
    let c3 = 2392.0 / 4096.0 * 32.0;
    let e = clamp(x, 0.0, 1.0);
    let ep = pow(e, 1.0 / m2);
    let num = max(ep - c1, 0.0);
    let den = c2 - c3 * ep;
    if (den <= 0.0) { return 0.0; }
    let y = pow(num / den, 1.0 / m1);      // display luminance / 10000
    return y * 10000.0 / 203.0;
}

// Hybrid Log-Gamma EOTF (inverse OETF + 1000-nit OOTF) -> scene-linear scRGB.
fn hlg_eotf(x: f32) -> f32 {
    let a = 0.17883277;
    let b = 0.28466892;
    let c = 0.5599107;
    let e = clamp(x, 0.0, 1.0);
    var ys: f32;
    if (e <= 0.5) {
        ys = (e * e) / 3.0;
    } else {
        ys = (exp((e - c) / a) + b) / 12.0;
    }
    let nits = 1000.0 * pow(max(ys, 0.0), 1.2);
    return nits / 203.0;
}

@fragment
fn fs_scene(in: VsOut) -> @location(0) vec4<f32> {
    let s = textureSample(tex, samp, in.uv);
    var lin: vec3<f32>;
    if (cx.p1.w > 1.5) {
        lin = s.rgb;                                           // already scene-linear
    } else if (cx.p1.w > 0.5) {
        let e = vec3<f32>(eotf(s.r), eotf(s.g), eotf(s.b));
        lin = vec3<f32>(dot(cx.r0.xyz, e), dot(cx.r1.xyz, e), dot(cx.r2.xyz, e));
    } else {
        lin = vec3<f32>(srgb_to_linear(s.r), srgb_to_linear(s.g), srgb_to_linear(s.b));
    }
    // cx.r3.w carries the output scale: SDR content on an HDR surface is scaled to
    // the SDR white level; HDR content (and the SDR-display path) uses 1.0.
    return vec4<f32>(lin * cx.scale.x, s.a);
}

// Planar scene variant (task 79.10 NV12; task #91 Phase 2 P010 + PQ/HLG): Y rides
// `tex.r`, the interleaved half-res UV plane rides `uv_tex.rg`. One entry serves
// both 8-bit (NV12) and 10-bit high-aligned (P010) frames and every transfer,
// driven by uniforms — no bit-depth branch:
//   * range expand: `cx.range = (y_black, y_scale, c_center, c_scale)`, computed
//     in Rust (`planar_range`) so it already folds in full/limited AND 8/10-bit
//     (P010's `65535/64` code recovery). `yn=(y−y_black)·y_scale`, chroma centered.
//   * YUV matrix: `r0.w / r1.w / r2.w / scale.z` = the four derived coefficients.
//   * transfer (mode = `p1.w`): 0 = sRGB-like (no primaries matrix), 1 = parametric
//     + matrix, 3 = PQ + matrix, 4 = HLG + matrix. The primaries matrix rides
//     `r0/r1/r2.xyz`, applied AFTER the per-channel EOTF (BT.2020→709 for HDR).
// The encoded R'G'B' is clamped to [0,1] BEFORE the EOTF (matching the CPU
// functions); nothing is clamped after — the fp16 scene carries HDR/wide-gamut.
// Applied EXACTLY once (planar frames arrive raw — the single-application
// contract). Must match `pb_render::yuv::planar_to_scene` / the golden reference.
@group(0) @binding(3) var uv_tex: texture_2d<f32>;

@fragment
fn fs_scene_planar(in: VsOut) -> @location(0) vec4<f32> {
    let yv = textureSample(tex, samp, in.uv).r;
    let uvv = textureSample(uv_tex, samp, in.uv).rg;
    let yn = (yv - cx.range.x) * cx.range.y;
    let un = (uvv.r - cx.range.z) * cx.range.w;
    let vn = (uvv.g - cx.range.z) * cx.range.w;
    let enc = clamp(vec3<f32>(
        yn + cx.r0.w * vn,
        yn - cx.r1.w * un - cx.r2.w * vn,
        yn + cx.scale.z * un,
    ), vec3<f32>(0.0), vec3<f32>(1.0));
    let mode = cx.p1.w;
    var lin: vec3<f32>;
    if (mode > 3.5) {                              // HLG + primaries matrix
        let e = vec3<f32>(hlg_eotf(enc.r), hlg_eotf(enc.g), hlg_eotf(enc.b));
        lin = vec3<f32>(dot(cx.r0.xyz, e), dot(cx.r1.xyz, e), dot(cx.r2.xyz, e));
    } else if (mode > 2.5) {                       // PQ + primaries matrix
        let e = vec3<f32>(pq_eotf(enc.r), pq_eotf(enc.g), pq_eotf(enc.b));
        lin = vec3<f32>(dot(cx.r0.xyz, e), dot(cx.r1.xyz, e), dot(cx.r2.xyz, e));
    } else if (mode > 0.5) {                       // parametric + primaries matrix
        let e = vec3<f32>(eotf(enc.r), eotf(enc.g), eotf(enc.b));
        lin = vec3<f32>(dot(cx.r0.xyz, e), dot(cx.r1.xyz, e), dot(cx.r2.xyz, e));
    } else {                                       // sRGB-like (source primaries = sRGB)
        lin = vec3<f32>(srgb_to_linear(enc.r), srgb_to_linear(enc.g), srgb_to_linear(enc.b));
    }
    return vec4<f32>(lin * cx.scale.x, 1.0);
}
"#;

/// Present pass: the scene-linear intermediate → the surface. A fullscreen triangle
/// samples the intermediate, then branches on the surface type (`params.z`):
///   - **SDR 8-bit surface** (0): per-channel extended Reinhard with the image's
///     peak as the white point (peak = 1 ⇒ identity, faithful SDR), then sRGB-encode.
///   - **HDR fp16 scRGB surface** (1): output scene-linear scaled to the SDR white
///     level (`params.y`). Negatives (wide gamut) and values > 1 (HDR) are kept —
///     scRGB carries them to the panel.
const PRESENT_WGSL: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> VsOut {
    var o: VsOut;
    let uv = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    o.uv = uv;
    o.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct Present { params: vec4<f32> };   // x = peak (SDR white pt), y = EDR roll-off headroom (0 = straight), z = hdr flag
@group(0) @binding(2) var<uniform> pr: Present;

fn srgb_oetf(c: f32) -> f32 {
    let x = clamp(c, 0.0, 1.0);
    if (x <= 0.0031308) {
        return 12.92 * x;
    }
    return 1.055 * pow(x, 1.0 / 2.4) - 0.055;
}

fn reinhard(v: f32, lw: f32) -> f32 {
    let x = max(v, 0.0);
    return x * (1.0 + x / (lw * lw)) / (1.0 + x);
}

// Highlight roll-off for the macOS EDR surface: identity at/below SDR white (1.0),
// smoothly compressing values above 1 so they asymptote to `headroom` rather than
// hard-clipping at the panel's EDR limit. SDR/diffuse and wide-gamut (negative)
// values pass through unchanged. `headroom` <= 1 clamps highlights to SDR white.
fn rolloff(v: f32, headroom: f32) -> f32 {
    if (v <= 1.0) { return v; }
    let t = v - 1.0;
    let m = max(headroom - 1.0, 0.0);
    return 1.0 + m * t / (t + m + 1e-6);
}

@fragment
fn fs_present(in: VsOut) -> @location(0) vec4<f32> {
    let s = textureSample(tex, samp, in.uv);
    if (pr.params.z > 0.5) {
        // HDR/wide-gamut scRGB surface: the intermediate is already final scene-linear
        // scRGB. On Windows (headroom 0) the DWM compositor tone-maps, so pass straight
        // through. On macOS, EDR hard-clips above the panel headroom, so roll highlights
        // off toward it — keeping SDR/diffuse and wide-gamut (negative) values intact.
        let hr = pr.params.y;
        if (hr > 0.0) {
            return vec4<f32>(rolloff(s.r, hr), rolloff(s.g, hr), rolloff(s.b, hr), 1.0);
        }
        return vec4<f32>(s.rgb, 1.0);
    }
    let lw = pr.params.x;
    let o = vec3<f32>(
        srgb_oetf(reinhard(s.r, lw)),
        srgb_oetf(reinhard(s.g, lw)),
        srgb_oetf(reinhard(s.b, lw)),
    );
    return vec4<f32>(o, 1.0);
}
"#;

/// Overlay pass: an sRGB UI bitmap (info panel / help) composited into the
/// scene-linear intermediate (before present), so it works for both the SDR and
/// HDR output paths uniformly. The bitmap is sRGB-encoded, so it is linearized
/// here; the present pass re-encodes (SDR) or scales it to SDR white (HDR). Uses
/// the scene bind-group layout (the color uniform at binding 2 is unused here).
const OVERLAY_WGSL: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@location(0) position: vec2<f32>, @location(1) uv: vec2<f32>) -> VsOut {
    var o: VsOut;
    o.pos = vec4<f32>(position, 0.0, 1.0);
    o.uv = uv;
    return o;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct ColorXf {
    r0: vec4<f32>, r1: vec4<f32>, r2: vec4<f32>,
    p0: vec4<f32>, p1: vec4<f32>, scale: vec4<f32>,
};
@group(0) @binding(2) var<uniform> cx: ColorXf;

fn srgb_to_linear(x: f32) -> f32 {
    if (x <= 0.04045) {
        return x / 12.92;
    }
    return pow((x + 0.055) / 1.055, 2.4);
}

@fragment
fn fs_overlay(in: VsOut) -> @location(0) vec4<f32> {
    let s = textureSample(tex, samp, in.uv);
    let lin = vec3<f32>(srgb_to_linear(s.r), srgb_to_linear(s.g), srgb_to_linear(s.b));
    // Same output scale as the scene, so UI sits at the SDR white level on an HDR
    // surface (and is unchanged on the SDR path).
    return vec4<f32>(lin * cx.scale.x, s.a);
}
"#;

/// egui-overlay pass: the shell renders the rich panels (Inspector / Help / folder
/// tree) into an offscreen **`Rgba8UnormSrgb`** texture with `egui-wgpu`, which
/// composites them in linear light with *premultiplied* alpha and stores sRGB. We
/// sample that texture through an sRGB view (so the sampler decodes back to
/// premultiplied **linear**) with a bufferless fullscreen triangle, lift it to the
/// SDR-white level (`cx.scale.x`, matching the CPU overlays on an HDR surface), and
/// blend it into the fp16 intermediate with a *premultiplied* blend state — so no
/// double sRGB conversion happens and the panels sit at the right brightness on both
/// SDR and HDR output paths. Reuses the scene bind-group layout (only `scale.x` of
/// the color uniform is read).
const EGUI_WGSL: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_egui(@builtin(vertex_index) vi: u32) -> VsOut {
    var o: VsOut;
    let uv = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    o.uv = uv;
    o.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct ColorXf {
    r0: vec4<f32>, r1: vec4<f32>, r2: vec4<f32>,
    p0: vec4<f32>, p1: vec4<f32>, scale: vec4<f32>,
};
@group(0) @binding(2) var<uniform> cx: ColorXf;

@fragment
fn fs_egui(in: VsOut) -> @location(0) vec4<f32> {
    // The sRGB view decodes the store back to premultiplied linear; keep the alpha
    // straight for the premultiplied-over blend. Scale only the (premultiplied) RGB.
    let s = textureSample(tex, samp, in.uv);
    return vec4<f32>(s.rgb * cx.scale.x, s.a);
}
"#;

/// Mipmap generation (#106.7 §6 / gpu-mipmap-hq-scaling plan): one 2× box-downsample per level,
/// so trilinear sampling gives near-Lanczos quality when a photo is fit-downscaled, retiring the
/// soft/aliased mipless-bilinear look. Correctness (Codex-reviewed): averages in **linear light**
/// (encoded averaging is badly wrong on high-contrast edges) with **premultiplied alpha** (else
/// transparent PNG/SVG edges halo), reading four explicit `textureLoad`s. `fs_srgb` decodes/encodes
/// sRGB (SDR mode-0 `Rgba8Unorm`); `fs_linear` averages directly (HDR mode-2 `Rgba16Float`, already
/// scene-linear). Source-ICC (mode 1) images are NOT mipped (stay L0) — their TRC isn't threaded
/// here. Known Phase-1 limit: on odd source dims the trailing row/column is **DROPPED** — the 2×2
/// box for the last destination texel starts at `2*(dst-1)`, which never reaches source texel
/// `2*dst` (the edge clamp only guards reads *past* the extent, it never pulls the orphan texel
/// in). So each odd level loses ≤1 row/col and the mip phase is slightly biased; pinned by
/// `odd_dims_drop_the_trailing_row_and_col` so the #110 derive can treat the bias as a known
/// quantity. A polyphase (NPOT-correct) box is a later refinement — #110 plan §3b.
const MIPGEN_WGSL: &str = r#"
@vertex
fn vs(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    return vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
}

@group(0) @binding(0) var src: texture_2d<f32>;

fn s2l(x: f32) -> f32 {
    if (x <= 0.04045) { return x / 12.92; }
    return pow((x + 0.055) / 1.055, 2.4);
}
fn l2s(x: f32) -> f32 {
    let c = clamp(x, 0.0, 1.0);
    if (c <= 0.0031308) { return 12.92 * c; }
    return 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}

fn box2x(pos: vec4<f32>, is_srgb: bool) -> vec4<f32> {
    let dim = vec2<i32>(textureDimensions(src));
    let base = vec2<i32>(pos.xy) * 2;
    var acc = vec3<f32>(0.0, 0.0, 0.0);   // premultiplied, linear
    var asum = 0.0;
    for (var dy = 0; dy < 2; dy = dy + 1) {
        for (var dx = 0; dx < 2; dx = dx + 1) {
            let c = clamp(base + vec2<i32>(dx, dy), vec2<i32>(0, 0), dim - vec2<i32>(1, 1));
            let t = textureLoad(src, c, 0);
            var rgb = t.rgb;
            if (is_srgb) { rgb = vec3<f32>(s2l(rgb.r), s2l(rgb.g), s2l(rgb.b)); }
            acc = acc + rgb * t.a;
            asum = asum + t.a;
        }
    }
    let a = asum / 4.0;
    var lin = vec3<f32>(0.0, 0.0, 0.0);
    if (asum > 0.0) { lin = acc / asum; }   // un-premultiply: (acc/4)/(asum/4)
    if (is_srgb) { lin = vec3<f32>(l2s(lin.r), l2s(lin.g), l2s(lin.b)); }
    return vec4<f32>(lin, a);
}

@fragment
fn fs_srgb(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    return box2x(pos, true);
}
@fragment
fn fs_linear(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    return box2x(pos, false);
}
"#;

/// #110 Phase 110a: the two-pass scale-aware Lanczos derive (mip-assisted separable resample of a
/// retained `Original` into an exact-size Fit). Coefficients are precomputed on the CPU
/// ([`crate::resample::lanczos_axis_kernel`] — already normalized per destination coordinate) and
/// read from storage buffers; the shader only applies them.
///
/// **Colour/alpha chain (the #110 plan §3b load-bearing sequence).** Mips store STRAIGHT alpha,
/// and mode-0 mips are sRGB-ENCODED — so the H pass re-linearizes (mode 0 only) and
/// re-premultiplies on load, accumulates *signed* premultiplied-linear (Lanczos lobes go
/// negative), and the intermediate stays **premultiplied-linear fp16 — no un-premultiply between
/// passes**. The V pass filters those values as-is, then the final store un-premultiplies ONCE
/// (α > ε, else RGB = 0) and encodes:
///   - `fs_v_srgb` (mode-0 final): clamp + sRGB OETF → straight `Rgba8Unorm` — what the scene
///     shader's mode-0 sampler expects (straight alpha + `ALPHA_BLENDING`; a premultiplied Fit
///     would double-apply α and darken edges).
///   - `fs_v_linear` (mode-2 final): straight scene-linear `Rgba16Float`, **unclamped** — scRGB
///     wide-gamut negatives and HDR >1 ride through, same no-clamp policy as the scene pass
///     (Lanczos ringing shares that latitude; the A/B harness judges it).
///
/// Taps clamp to the source extent (clamp-to-edge, matching the kernel's normalization contract).
/// The H pass binds ONE mip level of the Original as a single-level view — no mip of the source is
/// ever a render attachment during the derive (the intermediate/final are separate textures), so
/// the `generate_mips` view-aliasing rule holds by construction.
const DERIVE_WGSL: &str = r#"
@vertex
fn vs(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    return vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
}

@group(0) @binding(0) var src: texture_2d<f32>;
struct DeriveParams {
    taps: u32,
    srgb_in: u32,   // 1 = mode-0 source (sRGB-encoded): EOTF each tap; 0 = already linear
    _pad0: u32,
    _pad1: u32,
};
@group(0) @binding(1) var<uniform> params: DeriveParams;
@group(0) @binding(2) var<storage, read> starts: array<i32>;
@group(0) @binding(3) var<storage, read> weights: array<f32>;

fn s2l(x: f32) -> f32 {
    if (x <= 0.04045) { return x / 12.92; }
    return pow((x + 0.055) / 1.055, 2.4);
}
fn l2s(x: f32) -> f32 {
    let c = clamp(x, 0.0, 1.0);
    if (c <= 0.0031308) { return 12.92 * c; }
    return 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}

// fp16's largest finite value: the sanitize/containment bound for the linear path. An Inf in a
// hostile/broken fp16 source must never enter the accumulator (Inf × a negative lobe → NaN
// spreads to every neighbour), and un-premultiplying just above the alpha floor can amplify
// finite HDR past fp16 range — both are pinned to this bound instead.
const F16_MAX: f32 = 65504.0;

fn sanitize(v: vec4<f32>) -> vec4<f32> {
    // min/max (not clamp): WGSL min/max yield the non-NaN operand, so NaN also lands finite.
    return min(max(v, vec4<f32>(-F16_MAX)), vec4<f32>(F16_MAX));
}

// H pass → (dst_w × src_h) fp16 target: taps run along X of the bound source level. Loads the
// straight-alpha source, linearizes (mode 0) + premultiplies, accumulates signed premult-linear.
// Exactly-zero weights (boundary taps by construction) are skipped — fewer loads, and a
// non-finite texel can't ride in on a zero weight.
@fragment
fn fs_h(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let dim = vec2<i32>(textureDimensions(src));
    let x = u32(pos.x);
    let y = i32(pos.y);
    let start = starts[x];
    var acc = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    for (var t = 0u; t < params.taps; t = t + 1u) {
        let w = weights[x * params.taps + t];
        if (w == 0.0) { continue; }
        let cx = clamp(start + i32(t), 0, dim.x - 1);
        let s = sanitize(textureLoad(src, vec2<i32>(cx, y), 0));
        var rgb = s.rgb;
        if (params.srgb_in == 1u) { rgb = vec3<f32>(s2l(rgb.r), s2l(rgb.g), s2l(rgb.b)); }
        let a = clamp(s.a, 0.0, 1.0);
        acc = acc + vec4<f32>(rgb * a, a) * w;
    }
    return acc;
}

// V-pass accumulator: taps along Y of the fp16 intermediate (already premultiplied-linear —
// filtered as-is, per the no-unpremult-between-passes rule). Loads are sanitized again: an H
// result past fp16 range stores as Inf in the intermediate, which must not spread here.
fn v_acc(pos: vec4<f32>) -> vec4<f32> {
    let dim = vec2<i32>(textureDimensions(src));
    let x = i32(pos.x);
    let y = u32(pos.y);
    let start = starts[y];
    var acc = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    for (var t = 0u; t < params.taps; t = t + 1u) {
        let w = weights[y * params.taps + t];
        if (w == 0.0) { continue; }
        let cy = clamp(start + i32(t), 0, dim.y - 1);
        acc = acc + sanitize(textureLoad(src, vec2<i32>(x, cy), 0)) * w;
    }
    return acc;
}

// Un-premultiply ONCE at the final store. The divisor is the UNCLAMPED filtered alpha: Lanczos
// overshoots alpha past 1.0 at an opaque/transparent step (~1.08 at 2×), and since the premult
// RGB overshoots by the same factor, dividing by the true filtered alpha recovers the exact
// straight colour — dividing by a clamped 1.0 would brighten it (visible fringe; Codex P1).
// Only the STORED alpha clamps to [0,1]. Fully transparent output gets RGB = 0.
fn unpremult(acc: vec4<f32>) -> vec4<f32> {
    var rgb = vec3<f32>(0.0, 0.0, 0.0);
    if (acc.a > 1e-4) { rgb = acc.rgb / acc.a; }
    return vec4<f32>(rgb, clamp(acc.a, 0.0, 1.0));
}

@fragment
fn fs_v_srgb(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let s = unpremult(v_acc(pos));
    return vec4<f32>(l2s(s.r), l2s(s.g), l2s(s.b), s.a); // l2s clamps (SDR ringing clipped)
}

@fragment
fn fs_v_linear(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let s = unpremult(v_acc(pos));
    // Contain, don't clamp to SDR: HDR/wide-gamut values keep their range, but the divide just
    // above the alpha floor can amplify past fp16's finite range (Codex P2) — pin to ±F16_MAX.
    return vec4<f32>(min(max(s.rgb, vec3<f32>(-F16_MAX)), vec3<f32>(F16_MAX)), s.a);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
}

const ATTRS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2];

const INDICES: [u16; 6] = [0, 1, 2, 0, 2, 3];

/// GPU-side mirror of [`ColorTransform`], laid out as six `vec4`s to match the
/// shader's `ColorXf` uniform (each field 16-byte aligned). `r0..r2` are the
/// matrix rows; `p0 = (g,a,b,c)`, `p1 = (d,e,f,mode)`, `scale.x` = output scale.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ColorUniform {
    r0: [f32; 4],
    r1: [f32; 4],
    r2: [f32; 4],
    p0: [f32; 4],
    p1: [f32; 4],
    scale: [f32; 4],
    /// Planar range-expansion constants `fs_scene_planar` reads:
    /// `(y_black, y_scale, c_center, c_scale)` — precomputed in Rust for 8/10-bit
    /// × full/limited so the shader has no bit-depth branch (task #91 Phase 2).
    /// Inert for the RGBA path.
    range: [f32; 4],
}

impl ColorUniform {
    /// `mode`: 0 = sRGB-encoded, 1 = convert (matrix+TRC), 2 = scene-linear (HDR),
    /// 3 = PQ (matrix, task #91 Phase 2), 4 = HLG (matrix). `scale`: output
    /// multiplier (SDR content → HDR-surface white level, else 1.0).
    fn new(c: &ColorTransform, mode: f32, scale: f32) -> Self {
        let m = &c.matrix;
        let t = &c.trc;
        Self {
            r0: [m[0][0], m[0][1], m[0][2], 0.0],
            r1: [m[1][0], m[1][1], m[1][2], 0.0],
            r2: [m[2][0], m[2][1], m[2][2], 0.0],
            p0: [t[0], t[1], t[2], t[3]],
            p1: [t[4], t[5], t[6], mode],
            scale: [scale, 0.0, 0.0, 0.0],
            range: [0.0, 1.0, 0.5, 1.0],
        }
    }

    /// [`Self::new`] plus the planar convert parameters `fs_scene_planar` reads:
    /// `r0.w/r1.w/r2.w/scale.z` = the derived YUV matrix coefficients, and
    /// `range = (y_black, y_scale, c_center, c_scale)` = the range-expansion
    /// constants (which encode full/limited **and** 8/10-bit, so the shader needs
    /// no branch — task #91 Phase 2, generalizing the task 79.10 NV12 packing).
    fn new_planar(
        c: &ColorTransform,
        mode: f32,
        scale: f32,
        yuv: &crate::YuvParams,
        ten_bit: bool,
    ) -> Self {
        let (a, b, cq, d) = yuv.matrix.coeffs();
        let mut u = Self::new(c, mode, scale);
        u.r0[3] = a;
        u.r1[3] = b;
        u.r2[3] = cq;
        u.scale[2] = d;
        let (yb, ys, cc, cs) = crate::yuv::planar_range(ten_bit, yuv.full_range);
        u.range = [yb, ys, cc, cs];
        u
    }
}

/// The `fs_scene_planar` transfer mode (`p1.w`) for a planar transfer: 0 = sRGB-
/// like, 1 = parametric + primaries, 3 = PQ + primaries, 4 = HLG + primaries.
fn planar_mode(transfer: crate::PlanarTransfer) -> f32 {
    match transfer {
        crate::PlanarTransfer::SrgbLike => 0.0,
        crate::PlanarTransfer::Parametric => 1.0,
        crate::PlanarTransfer::Pq => 3.0,
        crate::PlanarTransfer::Hlg => 4.0,
    }
}

/// Present-pass uniform (one `vec4` for std140 alignment): `x` = SDR tone-map white
/// point (image peak), `y` = HDR scRGB scale (SDR-white in 80-nit units), `z` = HDR
/// output flag (1.0 = fp16 scRGB surface, 0.0 = SDR 8-bit).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PresentUniform {
    params: [f32; 4],
}

impl PresentUniform {
    /// `peak` = SDR tone-map white point (the displayed image's peak; 1.0 for SDR).
    /// `hdr` = true on an fp16 scRGB surface. `edr_headroom` = the macOS EDR roll-off
    /// target (>1 rolls highlights off toward it; 0 = pass straight through, the
    /// Windows/DWM path) — see `display::DisplayHdr::edr_headroom`.
    fn new(peak: f32, hdr: bool, edr_headroom: f32) -> Self {
        Self {
            params: [
                peak.max(1.0),
                edr_headroom,
                if hdr { 1.0 } else { 0.0 },
                0.0,
            ],
        }
    }
}

/// The four corners of the image quad in clip space, placed and UV-mapped by the
/// per-photo `ViewTransform` (scaling mode + rotation + zoom + pan).
fn quad_vertices(
    view: &ViewTransform,
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
    top_inset: u32,
) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    // Fit/center against the content region *below* a translucent top bar, then slide the
    // whole placement down by the inset. The surface is still the full `screen_h`, so a
    // zoomed/cropped photo's overflow rides up under the bar (task #59 spike). `top_inset == 0`
    // reduces to the classic full-surface fit.
    let content_h = screen_h.saturating_sub(top_inset).max(1);
    let mut p = view.placement(img_w, img_h, screen_w, content_h);
    p.y += top_inset as f32;
    let x0 = (p.x / sw) * 2.0 - 1.0;
    let x1 = ((p.x + p.w) / sw) * 2.0 - 1.0;
    let y_top = 1.0 - (p.y / sh) * 2.0;
    let y_bot = 1.0 - ((p.y + p.h) / sh) * 2.0;
    [
        Vertex {
            pos: [x0, y_top],
            uv: p.uvs[0],
        },
        Vertex {
            pos: [x1, y_top],
            uv: p.uvs[1],
        },
        Vertex {
            pos: [x1, y_bot],
            uv: p.uvs[2],
        },
        Vertex {
            pos: [x0, y_bot],
            uv: p.uvs[3],
        },
    ]
}

/// A bind-group layout entry: a filterable float texture, a filtering sampler, and
/// a fragment uniform buffer — the shape both the image (color uniform) and
/// tone-map (peak uniform) passes use.
fn tex_sampler_uniform_bgl(device: &wgpu::Device, label: &str) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    })
}

/// [`tex_sampler_uniform_bgl`] plus a second texture at binding 3 — the planar
/// two-plane layout (`fs_scene_planar`: Y at 0, UV at 3; task 79.10 / #91). Both
/// NV12 (`R8Unorm`/`Rg8Unorm`) and P010 (`R16Unorm`/`Rg16Unorm`) are
/// filterable-float, so one layout serves both.
fn planar_bgl(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    let texture = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("planar-bgl"),
        entries: &[
            texture(0),
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            texture(3),
        ],
    })
}

/// The render pipelines and their bind-group layouts.
struct Pipelines {
    /// Scene: photo quad → fp16 scRGB-linear intermediate (alpha-blended over the
    /// letterbox so transparent images composite cleanly).
    scene: wgpu::RenderPipeline,
    /// Scene variant for planar video frames: two planes + in-shader YUV +
    /// transfer (NV12 79.10; P010/PQ/HLG task #91 Phase 2). One pipeline for both.
    scene_planar: wgpu::RenderPipeline,
    /// Tone-map: fullscreen intermediate → SDR `surface_format`.
    tonemap: wgpu::RenderPipeline,
    /// Overlay: sRGB UI bitmap → surface, alpha-blended on top.
    overlay: wgpu::RenderPipeline,
    /// Subtitle overlay: the same shader and quad as `overlay`, but blended
    /// **premultiplied** — `pb_hud::subtitle` emits premultiplied RGBA (its own tests
    /// enforce it), and running that through `overlay`'s straight `ALPHA_BLENDING` would
    /// multiply by alpha a second time, sucking the life out of every antialiased edge.
    subtitle: wgpu::RenderPipeline,
    /// egui rich-panel overlay: an offscreen premultiplied-sRGB texture → the fp16
    /// intermediate, premultiplied-blended (bufferless fullscreen).
    egui: wgpu::RenderPipeline,
    /// Layout for the image (and overlay) bind groups: tex + sampler + color uniform.
    scene_bgl: wgpu::BindGroupLayout,
    /// Layout for the planar bind group: Y tex + sampler + color uniform + UV tex.
    planar_bgl: wgpu::BindGroupLayout,
    /// Layout for the tone-map bind group: intermediate tex + sampler + peak uniform.
    tonemap_bgl: wgpu::BindGroupLayout,
}

/// Mipmap-generation pipelines (see [`MIPGEN_WGSL`]). Two, because the render-target format is
/// baked into a pipeline: `srgb` targets `Rgba8Unorm` (SDR, decode/encode sRGB), `linear` targets
/// `Rgba16Float` (HDR, already scene-linear). The bind-group layout is a single sampled texture
/// (the previous mip level) — `textureLoad`, no sampler.
struct MipGen {
    bgl: wgpu::BindGroupLayout,
    srgb: wgpu::RenderPipeline,
    linear: wgpu::RenderPipeline,
}

fn build_mipgen(device: &wgpu::Device) -> MipGen {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("pb-mipgen"),
        source: wgpu::ShaderSource::Wgsl(MIPGEN_WGSL.into()),
    });
    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("mipgen-bgl"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        }],
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("mipgen-layout"),
        bind_group_layouts: &[&bgl],
        push_constant_ranges: &[],
    });
    let make = |entry: &'static str, format: wgpu::TextureFormat| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("mipgen-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: "vs",
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: entry,
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        })
    };
    MipGen {
        srgb: make("fs_srgb", wgpu::TextureFormat::Rgba8Unorm),
        linear: make("fs_linear", wgpu::TextureFormat::Rgba16Float),
        bgl,
    }
}

/// Mip-chain length for a texture: `1 + floor(log2(max(w,h)))`, computed on integers (not float
/// `log2`, per Codex) so odd sizes are exact. `max(w,h)==1 → 1` (no chain).
fn mip_levels(w: u32, h: u32) -> u32 {
    32 - w.max(h).max(1).leading_zeros()
}

/// Fill mip levels `1..levels` of `tex` by 2× box-downsampling the previous level (linear-light,
/// premultiplied — see [`MIPGEN_WGSL`]). `srgb` selects the SDR (sRGB) vs HDR (linear) pipeline.
/// Records one render pass per level into its own encoder and submits it — the L0 upload was
/// submitted first, so same-queue ordering guarantees each level reads a completed previous level.
fn generate_mips(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    mip: &MipGen,
    tex: &wgpu::Texture,
    levels: u32,
    srgb: bool,
) {
    let pipeline = if srgb { &mip.srgb } else { &mip.linear };
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("mipgen"),
    });
    for level in 1..levels {
        // Non-overlapping single-level views: the source is level-1 (sampled), the target is
        // `level` (rendered). Binding the all-mips view as source would alias the attachment.
        let src_view = tex.create_view(&wgpu::TextureViewDescriptor {
            label: Some("mipgen-src"),
            base_mip_level: level - 1,
            mip_level_count: Some(1),
            ..Default::default()
        });
        let dst_view = tex.create_view(&wgpu::TextureViewDescriptor {
            label: Some("mipgen-dst"),
            base_mip_level: level,
            mip_level_count: Some(1),
            ..Default::default()
        });
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mipgen-bg"),
            layout: &mip.bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&src_view),
            }],
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("mipgen-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &dst_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &bg, &[]);
        pass.draw(0..3, 0..1); // fullscreen triangle → covers every target texel
    }
    queue.submit(Some(encoder.finish()));
}

/// The two-pass Lanczos derive pipelines (see [`DERIVE_WGSL`]), built once. Three pipelines
/// because the render-target format is baked in: `h` targets the fp16 premultiplied-linear
/// intermediate; `v_srgb` targets the mode-0 `Rgba8Unorm` final; `v_linear` the mode-2
/// `Rgba16Float` final. One bind-group layout serves both passes (texture + params uniform +
/// starts/weights storage). Phase 110a: built and tested, not yet on any production path — 110b
/// wires it behind the `ScalePolicy` seam.
struct DeriveLanczos {
    bgl: wgpu::BindGroupLayout,
    h: wgpu::RenderPipeline,
    v_srgb: wgpu::RenderPipeline,
    v_linear: wgpu::RenderPipeline,
}

/// GPU-side mirror of [`DERIVE_WGSL`]'s `DeriveParams`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct DeriveParams {
    taps: u32,
    srgb_in: u32,
    _pad: [u32; 2],
}

fn build_derive(device: &wgpu::Device) -> DeriveLanczos {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("pb-derive"),
        source: wgpu::ShaderSource::Wgsl(DERIVE_WGSL.into()),
    });
    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("derive-bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("derive-layout"),
        bind_group_layouts: &[&bgl],
        push_constant_ranges: &[],
    });
    let mk = |entry: &'static str, format: wgpu::TextureFormat, label: &'static str| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: "vs",
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: entry,
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        })
    };
    DeriveLanczos {
        h: mk("fs_h", wgpu::TextureFormat::Rgba16Float, "derive-h"),
        v_srgb: mk(
            "fs_v_srgb",
            wgpu::TextureFormat::Rgba8Unorm,
            "derive-v-srgb",
        ),
        v_linear: mk(
            "fs_v_linear",
            wgpu::TextureFormat::Rgba16Float,
            "derive-v-linear",
        ),
        bgl,
    }
}

/// Derive an exact-size (`dst_w`×`dst_h`) Fit from mip level `src_mip` of a retained Original
/// (#110 Phase 110a). `srgb_in` says how the source stores pixels: `true` = mode 0 (sRGB-encoded
/// `Rgba8Unorm`, final is the same), `false` = mode 2 (scene-linear fp16, final is fp16). The
/// caller picks the mip per policy (last-eligible-mip vs `mip_bias = -1` — the 110c A/B) and is
/// responsible for eligibility (never a `was_clamped` or mode-1 source — those fall back to the
/// CPU Fit) and for VRAM accounting (the fp16 scratch intermediate is `dst_w × src_mip_h × 8`
/// bytes, allocated and dropped per derive here — pool-vs-transient is a 110b measurement).
///
/// The output size must be the FITTED IMAGE size (aspect-correct, rotation/inset applied by the
/// caller via `fit_rect` semantics, never an upscale) — a viewport-sized target would distort.
/// Submits its own encoder; same-queue ordering makes the result safe to bind on return. The
/// final carries `COPY_SRC` for readback (tests / future screenshot path).
#[allow(clippy::too_many_arguments)]
fn derive_fit_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    derive: &DeriveLanczos,
    src: &wgpu::Texture,
    src_mip: u32,
    srgb_in: bool,
    dst_w: u32,
    dst_h: u32,
    a: u32,
) -> wgpu::Texture {
    let (sw, sh) = (
        (src.width() >> src_mip).max(1),
        (src.height() >> src_mip).max(1),
    );
    let kh = crate::resample::lanczos_axis_kernel(sw, dst_w, a);
    let kv = crate::resample::lanczos_axis_kernel(sh, dst_h, a);

    let tex = |w: u32, h: u32, format, label: &str| {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    };
    // H intermediate: dst width × SOURCE height, premultiplied-linear fp16 (signed lobes).
    let mid = tex(
        dst_w,
        sh,
        wgpu::TextureFormat::Rgba16Float,
        "derive-intermediate",
    );
    let final_format = if srgb_in {
        wgpu::TextureFormat::Rgba8Unorm
    } else {
        wgpu::TextureFormat::Rgba16Float
    };
    let out = tex(dst_w, dst_h, final_format, "derive-fit");

    let kernel_bufs = |k: &crate::resample::AxisKernel, srgb: bool, label: &str| {
        let params = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents: bytemuck::bytes_of(&DeriveParams {
                taps: k.taps,
                srgb_in: srgb as u32,
                _pad: [0; 2],
            }),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let starts = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents: bytemuck::cast_slice(&k.starts),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let weights = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents: bytemuck::cast_slice(&k.weights),
            usage: wgpu::BufferUsages::STORAGE,
        });
        (params, starts, weights)
    };
    let bind = |view: &wgpu::TextureView, bufs: &(wgpu::Buffer, wgpu::Buffer, wgpu::Buffer)| {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("derive-bg"),
            layout: &derive.bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: bufs.0.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: bufs.1.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: bufs.2.as_entire_binding(),
                },
            ],
        })
    };
    // The H pass reads exactly ONE mip level through a single-level view (level-agnostic shader;
    // `textureDimensions` reports that level's extent).
    let src_view = src.create_view(&wgpu::TextureViewDescriptor {
        label: Some("derive-src"),
        base_mip_level: src_mip,
        mip_level_count: Some(1),
        ..Default::default()
    });
    let mid_view = mid.create_view(&wgpu::TextureViewDescriptor::default());
    let out_view = out.create_view(&wgpu::TextureViewDescriptor::default());
    let h_bufs = kernel_bufs(&kh, srgb_in, "derive-h-k");
    let v_bufs = kernel_bufs(&kv, false, "derive-v-k"); // V pass is always linear-in
    let h_bg = bind(&src_view, &h_bufs);
    let v_bg = bind(&mid_view, &v_bufs);

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("derive"),
    });
    for (pipeline, bg, view) in [
        (&derive.h, &h_bg, &mid_view),
        (
            if srgb_in {
                &derive.v_srgb
            } else {
                &derive.v_linear
            },
            &v_bg,
            &out_view,
        ),
    ] {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("derive-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bg, &[]);
        pass.draw(0..3, 0..1);
    }
    queue.submit(Some(encoder.finish()));
    out
}

fn build_pipelines(device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> Pipelines {
    let scene_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("pb-scene"),
        source: wgpu::ShaderSource::Wgsl(SCENE_WGSL.into()),
    });
    let tonemap_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("pb-present"),
        source: wgpu::ShaderSource::Wgsl(PRESENT_WGSL.into()),
    });
    let overlay_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("pb-overlay"),
        source: wgpu::ShaderSource::Wgsl(OVERLAY_WGSL.into()),
    });
    let egui_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("pb-egui"),
        source: wgpu::ShaderSource::Wgsl(EGUI_WGSL.into()),
    });

    let scene_bgl = tex_sampler_uniform_bgl(device, "img-bgl");
    let tonemap_bgl = tex_sampler_uniform_bgl(device, "tonemap-bgl");

    let scene_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("pb-scene-layout"),
        bind_group_layouts: &[&scene_bgl],
        push_constant_ranges: &[],
    });
    let tonemap_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("pb-tonemap-layout"),
        bind_group_layouts: &[&tonemap_bgl],
        push_constant_ranges: &[],
    });

    let quad_buffers = [wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &ATTRS,
    }];

    // Scene → fp16 intermediate.
    let scene = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("pb-scene-pipeline"),
        layout: Some(&scene_layout),
        vertex: wgpu::VertexState {
            module: &scene_mod,
            entry_point: "vs_main",
            compilation_options: Default::default(),
            buffers: &quad_buffers,
        },
        fragment: Some(wgpu::FragmentState {
            module: &scene_mod,
            entry_point: "fs_scene",
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: INTERMEDIATE_FORMAT,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });

    // Fullscreen present → surface (no vertex buffer; overwrites the whole frame).
    let tonemap = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("pb-present-pipeline"),
        layout: Some(&tonemap_layout),
        vertex: wgpu::VertexState {
            module: &tonemap_mod,
            entry_point: "vs_fullscreen",
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &tonemap_mod,
            entry_point: "fs_present",
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: surface_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });

    // Overlay → the fp16 intermediate (alpha-blended in linear), so one present
    // pass serves both SDR and HDR. Reuses the scene bind-group layout.
    let overlay = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("pb-overlay-pipeline"),
        layout: Some(&scene_layout),
        vertex: wgpu::VertexState {
            module: &overlay_mod,
            entry_point: "vs_main",
            compilation_options: Default::default(),
            buffers: &quad_buffers,
        },
        fragment: Some(wgpu::FragmentState {
            module: &overlay_mod,
            entry_point: "fs_overlay",
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: INTERMEDIATE_FORMAT,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });

    // Subtitles → the fp16 intermediate. Identical to `overlay` in every respect but the
    // blend: the rasterized cue block is **premultiplied** (`pb_hud::subtitle`), so it
    // composites premultiplied-over (src One) like the egui layer below, not with the
    // straight `ALPHA_BLENDING` the other CPU overlays are authored for. Same shader, same
    // quad, same bind-group layout — only the blend equation differs.
    let subtitle = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("pb-subtitle-pipeline"),
        layout: Some(&scene_layout),
        vertex: wgpu::VertexState {
            module: &overlay_mod,
            entry_point: "vs_main",
            compilation_options: Default::default(),
            buffers: &quad_buffers,
        },
        fragment: Some(wgpu::FragmentState {
            module: &overlay_mod,
            entry_point: "fs_overlay",
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: INTERMEDIATE_FORMAT,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });

    // egui rich panels → the fp16 intermediate. Bufferless fullscreen triangle; the
    // egui texture is premultiplied (linear after the sRGB-view decode), so the blend
    // is premultiplied-over (src One), not the straight `ALPHA_BLENDING` the CPU
    // overlays use. Reuses the scene bind-group layout.
    let egui = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("pb-egui-pipeline"),
        layout: Some(&scene_layout),
        vertex: wgpu::VertexState {
            module: &egui_mod,
            entry_point: "vs_egui",
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &egui_mod,
            entry_point: "fs_egui",
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: INTERMEDIATE_FORMAT,
                blend: Some(wgpu::BlendState {
                    color: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                    alpha: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });

    // Planar scene variant (task 79.10 NV12; task #91 Phase 2 P010): same
    // module/vertex/target/blend as `scene`, its own two-texture layout + the
    // generalized `fs_scene_planar` entry point (one pipeline for NV12 and P010 —
    // both are filterable-float textures, so one bind-group layout serves both).
    let planar_bgl = planar_bgl(device);
    let planar_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("pb-scene-planar-layout"),
        bind_group_layouts: &[&planar_bgl],
        push_constant_ranges: &[],
    });
    let scene_planar = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("pb-scene-planar-pipeline"),
        layout: Some(&planar_layout),
        vertex: wgpu::VertexState {
            module: &scene_mod,
            entry_point: "vs_main",
            compilation_options: Default::default(),
            buffers: &quad_buffers,
        },
        fragment: Some(wgpu::FragmentState {
            module: &scene_mod,
            entry_point: "fs_scene_planar",
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: INTERMEDIATE_FORMAT,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });

    Pipelines {
        scene,
        scene_planar,
        tonemap,
        overlay,
        subtitle,
        egui,
        scene_bgl,
        planar_bgl,
        tonemap_bgl,
    }
}

/// sRGB EOTF (encoded → linear) for one channel — to express the letterbox color
/// in the intermediate's linear space.
fn srgb_to_linear(u: u8) -> f64 {
    let x = u as f64 / 255.0;
    if x <= 0.04045 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

/// The letterbox clear color (sRGB `rgb`) in the intermediate's linear space (so it
/// round-trips back to `rgb` through the tone-map pass's sRGB encode). The background
/// is always opaque (alpha 1).
fn letterbox_linear(rgb: [u8; 3]) -> wgpu::Color {
    wgpu::Color {
        r: srgb_to_linear(rgb[0]),
        g: srgb_to_linear(rgb[1]),
        b: srgb_to_linear(rgb[2]),
        a: 1.0,
    }
}

/// Create the fp16 scRGB-linear intermediate render target sized to the surface,
/// plus the tone-map bind group that samples it. Rebuilt on resize.
fn make_intermediate(
    device: &wgpu::Device,
    tonemap_bgl: &wgpu::BindGroupLayout,
    peak_buf: &wgpu::Buffer,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::BindGroup) {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("scene-intermediate"),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: INTERMEDIATE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("tonemap-bg"),
        layout: tonemap_bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: peak_buf.as_entire_binding(),
            },
        ],
    });
    (tex, bind_group)
}

/// The four corners of an overlay panel quad: `panel_w`×`panel_h` pixels, placed
/// `margin` px in from the bottom-right of the screen (equal right + bottom insets).
fn overlay_quad_vertices(
    panel_w: u32,
    panel_h: u32,
    screen_w: u32,
    screen_h: u32,
    margin: u32,
) -> [Vertex; 4] {
    bottom_right_quad_xy(panel_w, panel_h, screen_w, screen_h, margin, margin)
}

/// Bottom-right anchored quad with **independent** right + bottom insets — lets the
/// rich panel lift above the info-line strip (`bottom_margin`) while keeping its right
/// edge fixed (`right_margin`), the mirror of [`top_right_quad_xy`].
fn bottom_right_quad_xy(
    panel_w: u32,
    panel_h: u32,
    screen_w: u32,
    screen_h: u32,
    right_margin: u32,
    bottom_margin: u32,
) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    let x1 = sw - right_margin as f32;
    let x0 = x1 - panel_w as f32;
    let y1 = sh - bottom_margin as f32;
    let y0 = y1 - panel_h as f32;
    let x0n = (x0 / sw) * 2.0 - 1.0;
    let x1n = (x1 / sw) * 2.0 - 1.0;
    let y_top = 1.0 - (y0 / sh) * 2.0;
    let y_bot = 1.0 - (y1 / sh) * 2.0;
    [
        Vertex {
            pos: [x0n, y_top],
            uv: [0.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_top],
            uv: [1.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_bot],
            uv: [1.0, 1.0],
        },
        Vertex {
            pos: [x0n, y_bot],
            uv: [0.0, 1.0],
        },
    ]
}

/// The four corners of a bottom-anchored quad placed `margin` px up from the bottom,
/// horizontally left / center / right by `align` — the info line's three positions.
fn bottom_aligned_quad(
    panel_w: u32,
    panel_h: u32,
    screen_w: u32,
    screen_h: u32,
    margin: u32,
    align: crate::HAlign,
) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    let pw = panel_w as f32;
    let x0 = match align {
        crate::HAlign::Left => margin as f32,
        crate::HAlign::Center => ((sw - pw) * 0.5).max(0.0),
        crate::HAlign::Right => sw - margin as f32 - pw,
    };
    let x1 = x0 + pw;
    let y1 = sh - margin as f32;
    let y0 = y1 - panel_h as f32;
    let x0n = (x0 / sw) * 2.0 - 1.0;
    let x1n = (x1 / sw) * 2.0 - 1.0;
    let y_top = 1.0 - (y0 / sh) * 2.0;
    let y_bot = 1.0 - (y1 / sh) * 2.0;
    [
        Vertex {
            pos: [x0n, y_top],
            uv: [0.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_top],
            uv: [1.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_bot],
            uv: [1.0, 1.0],
        },
        Vertex {
            pos: [x0n, y_bot],
            uv: [0.0, 1.0],
        },
    ]
}

/// The four corners of the bottom-center toast quad: `panel_w`×`panel_h` px,
/// horizontally centered and `bottom_margin` px up from the bottom edge.
fn toast_quad_vertices(
    panel_w: u32,
    panel_h: u32,
    screen_w: u32,
    screen_h: u32,
    bottom_margin: u32,
) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    let x0 = ((sw - panel_w as f32) * 0.5).max(0.0);
    let x1 = x0 + panel_w as f32;
    let y1 = sh - bottom_margin as f32;
    let y0 = y1 - panel_h as f32;
    let x0n = (x0 / sw) * 2.0 - 1.0;
    let x1n = (x1 / sw) * 2.0 - 1.0;
    let y_top = 1.0 - (y0 / sh) * 2.0;
    let y_bot = 1.0 - (y1 / sh) * 2.0;
    [
        Vertex {
            pos: [x0n, y_top],
            uv: [0.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_top],
            uv: [1.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_bot],
            uv: [1.0, 1.0],
        },
        Vertex {
            pos: [x0n, y_bot],
            uv: [0.0, 1.0],
        },
    ]
}

/// The four corners of the **subtitle** quad: `panel_w`×`panel_h` px with its top-left
/// corner at an absolute `(x, y)` in physical px.
///
/// Every other overlay helper takes an *inset from an edge*, because every other overlay
/// is anchored to one. A subtitle is not: it tracks the **picture**, and its position is
/// whatever `pb_app_core::subtitle::place` decided — vertically anchored to the video's
/// bottom edge, horizontally centered on the viewport, and clamped by rules this crate
/// deliberately cannot see. So the origin arrives already computed, and this only converts
/// it to NDC.
///
/// `x`/`y` are `f32` and **`y` may be slightly negative** — by design. `place` clamps the
/// *text's* box on screen, not the bitmap's, so a soft drop shadow is allowed to bleed off
/// the top edge rather than shoving the words down to keep it.
fn subtitle_quad_vertices(
    panel_w: u32,
    panel_h: u32,
    screen_w: u32,
    screen_h: u32,
    x: f32,
    y: f32,
) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    let x1 = x + panel_w as f32;
    let y1 = y + panel_h as f32;
    let x0n = (x / sw) * 2.0 - 1.0;
    let x1n = (x1 / sw) * 2.0 - 1.0;
    let y_top = 1.0 - (y / sh) * 2.0;
    let y_bot = 1.0 - (y1 / sh) * 2.0;
    [
        Vertex {
            pos: [x0n, y_top],
            uv: [0.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_top],
            uv: [1.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_bot],
            uv: [1.0, 1.0],
        },
        Vertex {
            pos: [x0n, y_bot],
            uv: [0.0, 1.0],
        },
    ]
}

/// The four corners of a panel quad centered on both axes of the screen — the
/// empty-state message placement.
fn center_quad_vertices(panel_w: u32, panel_h: u32, screen_w: u32, screen_h: u32) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    let x0 = ((sw - panel_w as f32) * 0.5).max(0.0);
    let y0 = ((sh - panel_h as f32) * 0.5).max(0.0);
    let x1 = x0 + panel_w as f32;
    let y1 = y0 + panel_h as f32;
    let x0n = (x0 / sw) * 2.0 - 1.0;
    let x1n = (x1 / sw) * 2.0 - 1.0;
    let y_top = 1.0 - (y0 / sh) * 2.0;
    let y_bot = 1.0 - (y1 / sh) * 2.0;
    [
        Vertex {
            pos: [x0n, y_top],
            uv: [0.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_top],
            uv: [1.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_bot],
            uv: [1.0, 1.0],
        },
        Vertex {
            pos: [x0n, y_bot],
            uv: [0.0, 1.0],
        },
    ]
}

/// The four corners of the top-left tree quad: `panel_w`×`panel_h` px, placed
/// `margin` px in from the top and left edges — the folder-tree panel's corner,
/// mirroring the info panel's bottom-right inset.
fn top_left_quad_vertices(
    panel_w: u32,
    panel_h: u32,
    screen_w: u32,
    screen_h: u32,
    margin: u32,
) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    let x0 = margin as f32;
    let x1 = x0 + panel_w as f32;
    let y0 = margin as f32;
    let y1 = y0 + panel_h as f32;
    let x0n = (x0 / sw) * 2.0 - 1.0;
    let x1n = (x1 / sw) * 2.0 - 1.0;
    let y_top = 1.0 - (y0 / sh) * 2.0;
    let y_bot = 1.0 - (y1 / sh) * 2.0;
    [
        Vertex {
            pos: [x0n, y_top],
            uv: [0.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_top],
            uv: [1.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_bot],
            uv: [1.0, 1.0],
        },
        Vertex {
            pos: [x0n, y_bot],
            uv: [0.0, 1.0],
        },
    ]
}

/// The four corners of the top-right pie quad: `panel_w`×`panel_h` px, placed
/// `margin` px in from the top and right edges (the "loading" affordance corner).
fn top_right_quad_vertices(
    panel_w: u32,
    panel_h: u32,
    screen_w: u32,
    screen_h: u32,
    margin: u32,
) -> [Vertex; 4] {
    // Same inset on both axes (the pie).
    top_right_quad_xy(panel_w, panel_h, screen_w, screen_h, margin, margin)
}

/// Top-right anchored quad with **independent** right + top insets — lets the scan-count
/// chip align its right edge with the pie (`right_margin`) while sitting below it
/// (`top_margin`).
fn top_right_quad_xy(
    panel_w: u32,
    panel_h: u32,
    screen_w: u32,
    screen_h: u32,
    right_margin: u32,
    top_margin: u32,
) -> [Vertex; 4] {
    let (sw, sh) = (screen_w as f32, screen_h as f32);
    let x1 = sw - right_margin as f32;
    let x0 = x1 - panel_w as f32;
    let y0 = top_margin as f32;
    let y1 = y0 + panel_h as f32;
    let x0n = (x0 / sw) * 2.0 - 1.0;
    let x1n = (x1 / sw) * 2.0 - 1.0;
    let y_top = 1.0 - (y0 / sh) * 2.0;
    let y_bot = 1.0 - (y1 / sh) * 2.0;
    [
        Vertex {
            pos: [x0n, y_top],
            uv: [0.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_top],
            uv: [1.0, 0.0],
        },
        Vertex {
            pos: [x1n, y_bot],
            uv: [1.0, 1.0],
        },
        Vertex {
            pos: [x0n, y_bot],
            uv: [0.0, 1.0],
        },
    ]
}

/// Downscale `image` (RGBA8) so neither dimension exceeds `max`, preserving
/// aspect with nearest-neighbor sampling. Returns the input borrowed unchanged
/// when it already fits.
fn clamp_to_max(image: &[u8], w: u32, h: u32, max: u32) -> (Cow<'_, [u8]>, u32, u32) {
    if w <= max && h <= max {
        return (Cow::Borrowed(image), w, h);
    }
    let scale = max as f64 / w.max(h) as f64;
    let tw = ((w as f64 * scale) as u32).clamp(1, max);
    let th = ((h as f64 * scale) as u32).clamp(1, max);
    let mut out = vec![0u8; (tw as usize) * (th as usize) * 4];
    for y in 0..th {
        let sy = ((y as u64 * h as u64) / th as u64) as u32;
        for x in 0..tw {
            let sx = ((x as u64 * w as u64) / tw as u64) as u32;
            let s = ((sy * w + sx) * 4) as usize;
            let d = ((y * tw + x) * 4) as usize;
            out[d..d + 4].copy_from_slice(&image[s..s + 4]);
        }
    }
    (Cow::Owned(out), tw, th)
}

/// A freshly created image texture and its scene bind group, returned by [`create_image_texture`].
/// #110: `texture` is owned so the GPU Lanczos derive can sample the mip chain of the full-res
/// `Original`; `was_clamped`/`mode` gate whether a slot is a valid derive source (a `clamp_to_max`'d
/// or source-ICC (mode 1) image is not).
struct UploadedImage {
    bind_group: wgpu::BindGroup,
    texture: wgpu::Texture,
    was_clamped: bool,
    mode: f32,
}

#[allow(clippy::too_many_arguments)]
fn create_image_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    bgl: &wgpu::BindGroupLayout,
    uploader: &mut dyn UploadStrategy,
    image: &[u8],
    img_w: u32,
    img_h: u32,
    color: &ColorTransform,
    hdr: bool,
    scale: f32,
    // Mip policy (gpu-mipmap-hq-scaling): `Some` (only the ring's full-res `Original` rep passes
    // it) builds a mipmap chain so trilinear gives near-Lanczos fit-downscaling. `None` (Fit,
    // preview, single-image/animation, UI bitmaps) stays single-level — those are shown ~1:1 or
    // re-uploaded per frame. Source-ICC (mode 1) images are never mipped (their TRC isn't in the
    // mip shader), so they fall back to single-level even when a policy is passed.
    mip: Option<&MipGen>,
) -> UploadedImage {
    // HDR images are scene-linear fp16 (Rgba16Float, mode 2); everything else is
    // sRGB/source-encoded RGBA8 (mode 1 if a profile applies, else 0).
    let (tex_format, mode) = if hdr {
        (wgpu::TextureFormat::Rgba16Float, 2.0)
    } else {
        (
            wgpu::TextureFormat::Rgba8Unorm,
            if color.enabled { 1.0 } else { 0.0 },
        )
    };
    // Downscale anything beyond the GPU's max texture dimension so huge images
    // (e.g. panoramas) upload instead of failing device validation. RGBA8 only —
    // HDR sources are already fit-sized in the decoder.
    let (orig_w, orig_h) = (img_w, img_h);
    let (image, img_w, img_h) = if hdr {
        (Cow::Borrowed(image), img_w, img_h)
    } else {
        clamp_to_max(
            image,
            img_w,
            img_h,
            device.limits().max_texture_dimension_2d,
        )
    };
    let image: &[u8] = &image;

    // Mip only the full-res Original rep, and never a source-ICC (mode 1) image (its TRC isn't in
    // the mip shader). `levels == 1` when the image is 1×1.
    let do_mip = mip.is_some() && mode != 1.0;
    let levels = if do_mip { mip_levels(img_w, img_h) } else { 1 };

    let size = wgpu::Extent3d {
        width: img_w,
        height: img_h,
        depth_or_array_layers: 1,
    };
    let mut usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
    if levels > 1 {
        usage |= wgpu::TextureUsages::RENDER_ATTACHMENT; // mip levels are rendered into
    }
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("image"),
        size,
        mip_level_count: levels,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: tex_format,
        usage,
        view_formats: &[],
    });
    // The staging-ring upload (`copy_buffer_to_texture`), not `write_texture` — fills mip 0.
    uploader.upload(device, queue, &tex, image, img_w, img_h);
    // Generate the rest of the chain by 2× box-downsampling (linear-light, premultiplied). Runs
    // after the L0 copy on the same queue, so ordering is guaranteed. `srgb = !hdr` because the
    // only SDR case reaching here is mode 0 (mode 1 set `do_mip` false).
    if levels > 1 {
        generate_mips(device, queue, mip.expect("do_mip"), &tex, levels, !hdr);
    }
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = image_bind_group(device, bgl, &view, color, mode, scale);
    UploadedImage {
        bind_group,
        texture: tex,
        was_clamped: img_w != orig_w || img_h != orig_h,
        mode,
    }
}

/// Build the scene bind group for an image texture view: the linear/trilinear sampler + the
/// per-image ColorXf uniform (matrix + TRC + `mode` + output `scale`), baked off the keypress
/// frame. Shared by [`create_image_texture`] (every upload) and the #110 derive (whose Fit is
/// GPU-produced — same bind-group shape, no upload). The linear sampler keeps large photos
/// smooth when scaled down; mipped Originals get trilinear through the same descriptor.
fn image_bind_group(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    view: &wgpu::TextureView,
    color: &ColorTransform,
    mode: f32,
    scale: f32,
) -> wgpu::BindGroup {
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let color_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("color-uniform"),
        contents: bytemuck::bytes_of(&ColorUniform::new(color, mode, scale)),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("img-bg"),
        layout: bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: color_buf.as_entire_binding(),
            },
        ],
    })
}

/// The decode-to-fit output size for a `w`×`h` source in a fit box — EXACTLY the CPU rule
/// (`pb_decode::common::downscale_to_fit`): contain-scale `min(fw/w, fh/h, 1)`, a `>= 0.999`
/// near-identity band returning the source unchanged, else round-to-nearest with a floor of 1.
/// Matching the incumbent matters: the derived Fit and the CPU Fit must agree on geometry or
/// the 110c A/B compares different sizes.
fn contain_dims(w: u32, h: u32, fit_w: u32, fit_h: u32) -> (u32, u32) {
    let scale = (fit_w as f64 / w as f64)
        .min(fit_h as f64 / h as f64)
        .min(1.0);
    if scale >= 0.999 {
        return (w, h);
    }
    (
        ((w as f64 * scale).round() as u32).max(1),
        ((h as f64 * scale).round() as u32).max(1),
    )
}

/// Pick the derive's source mip: the LAST (coarsest) level still ≥ the target on both axes —
/// maximally box-prefiltered, cheapest taps — then `mip_bias` (−1 = one level finer: residual
/// 2–4× and a wider scale-aware kernel; the real 110c design fork per Codex). Clamped to the
/// chain. Level dims floor at 1 (wgpu mip sizing).
fn select_derive_mip(w: u32, h: u32, levels: u32, dst_w: u32, dst_h: u32, bias: i32) -> u32 {
    let mut level = 0u32;
    while level + 1 < levels
        && (w >> (level + 1)).max(1) >= dst_w
        && (h >> (level + 1)).max(1) >= dst_h
    {
        level += 1;
    }
    (level as i32 + bias).clamp(0, levels.saturating_sub(1) as i32) as u32
}

/// Thin wrapper over [`create_image_texture`] keeping only the bind group — for every non-ring
/// uploader (toast/pie/overlay/tree/subtitle/egui/single-image) that never samples the texture again
/// (the bind group's view keeps it alive). Ring slots use `create_image_texture` directly so the
/// #110 derive can reach the `Original` texture.
#[allow(clippy::too_many_arguments)]
fn upload_image(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    bgl: &wgpu::BindGroupLayout,
    uploader: &mut dyn UploadStrategy,
    image: &[u8],
    img_w: u32,
    img_h: u32,
    color: &ColorTransform,
    hdr: bool,
    scale: f32,
    mip: Option<&MipGen>,
) -> wgpu::BindGroup {
    create_image_texture(
        device, queue, bgl, uploader, image, img_w, img_h, color, hdr, scale, mip,
    )
    .bind_group
}

/// [`upload_image`] through a reusable slot (task #79 phase 3 — the `set_image`
/// present path). While geometry + format match the slot — every frame of an
/// animation / video — the pixels upload into the **existing** texture and the
/// color uniform is rewritten in place: zero resource creation per frame. Any
/// mismatch (new item, resize, SDR↔HDR) rebuilds the slot, which is exactly the
/// old per-call cost.
#[allow(clippy::too_many_arguments)]
fn upload_image_reusable(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    bgl: &wgpu::BindGroupLayout,
    uploader: &mut dyn UploadStrategy,
    slot: &mut Option<ReuseSlot>,
    image: &[u8],
    img_w: u32,
    img_h: u32,
    color: &ColorTransform,
    hdr: bool,
    scale: f32,
) -> ReuseOutcome {
    let (tex_format, mode) = if hdr {
        (wgpu::TextureFormat::Rgba16Float, 2.0)
    } else {
        (
            wgpu::TextureFormat::Rgba8Unorm,
            if color.enabled { 1.0 } else { 0.0 },
        )
    };
    let (image, img_w, img_h) = if hdr {
        (Cow::Borrowed(image), img_w, img_h)
    } else {
        clamp_to_max(
            image,
            img_w,
            img_h,
            device.limits().max_texture_dimension_2d,
        )
    };
    let image: &[u8] = &image;

    if let Some(s) = slot.as_ref() {
        if s.w == img_w && s.h == img_h && s.format == tex_format {
            // Steady state: in-place texture upload + a uniform rewrite. Queue
            // order serializes this after every submitted draw that sampled the
            // old frame — no fence, no wait, nothing created.
            uploader.upload(device, queue, &s.tex, image, img_w, img_h);
            queue.write_buffer(
                &s.color_buf,
                0,
                bytemuck::bytes_of(&ColorUniform::new(color, mode, scale)),
            );
            return ReuseOutcome::Reused;
        }
    }

    // (Re)build the slot — the one-time cost the old path paid every call.
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("image-reuse"),
        size: wgpu::Extent3d {
            width: img_w,
            height: img_h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: tex_format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    uploader.upload(device, queue, &tex, image, img_w, img_h);
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let color_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("color-uniform-reuse"),
        contents: bytemuck::bytes_of(&ColorUniform::new(color, mode, scale)),
        // COPY_DST so later same-geometry frames rewrite it in place.
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("img-bg-reuse"),
        layout: bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: color_buf.as_entire_binding(),
            },
        ],
    });
    *slot = Some(ReuseSlot {
        tex,
        uv_tex: None,
        color_buf,
        w: img_w,
        h: img_h,
        format: tex_format,
    });
    ReuseOutcome::Rebuilt(bind_group)
}

/// [`upload_image_reusable`] for an NV12 video frame (task 79.10): the Y plane
/// into an `R8Unorm` texture, the interleaved UV plane into a half-res
/// `Rg8Unorm` one, the YUV parameters packed into the color uniform. The steady
/// state (every frame of one clip) is two in-place uploads + one uniform write —
/// zero resource creation, same never-wait staging ring, same slot discipline.
///
/// Upload a two-plane planar frame (NV12 8-bit or P010 16-bit) into the reuse
/// slot and build/refresh its bind group. `mode` selects the shader transfer
/// branch (0 sRGB-like, 1 parametric, 3 PQ, 4 HLG). The Y/UV texture formats
/// follow `format`: `R8Unorm`/`Rg8Unorm` for NV12, `R16Unorm`/`Rg16Unorm` for
/// P010 (task #91 Phase 2). The `StagingUpload` derives bytes-per-row from the
/// texture format, so the same upload path serves both precisions.
#[allow(clippy::too_many_arguments)]
fn upload_planar_reusable(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    bgl: &wgpu::BindGroupLayout,
    uploader: &mut dyn UploadStrategy,
    slot: &mut Option<ReuseSlot>,
    y: &[u8],
    uv: &[u8],
    img_w: u32,
    img_h: u32,
    color: &ColorTransform,
    yuv: crate::YuvParams,
    format: crate::PlanarFormat,
    mode: f32,
    scale: f32,
) -> ReuseOutcome {
    let ten_bit = format.is_ten_bit();
    let (y_fmt, uv_fmt) = if ten_bit {
        (
            wgpu::TextureFormat::R16Unorm,
            wgpu::TextureFormat::Rg16Unorm,
        )
    } else {
        (wgpu::TextureFormat::R8Unorm, wgpu::TextureFormat::Rg8Unorm)
    };
    let uniform = ColorUniform::new_planar(color, mode, scale, &yuv, ten_bit);
    if let Some(s) = slot.as_ref() {
        if let Some(uv_tex) = s
            .uv_tex
            .as_ref()
            .filter(|_| s.w == img_w && s.h == img_h && s.format == y_fmt)
        {
            uploader.upload(device, queue, &s.tex, y, img_w, img_h);
            uploader.upload(device, queue, uv_tex, uv, img_w / 2, img_h / 2);
            queue.write_buffer(&s.color_buf, 0, bytemuck::bytes_of(&uniform));
            return ReuseOutcome::Reused;
        }
    }

    let plane = |label: &str, w: u32, h: u32, format: wgpu::TextureFormat| {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    };
    let y_tex = plane("video-y-reuse", img_w, img_h, y_fmt);
    let uv_tex = plane("video-uv-reuse", img_w / 2, img_h / 2, uv_fmt);
    uploader.upload(device, queue, &y_tex, y, img_w, img_h);
    uploader.upload(device, queue, &uv_tex, uv, img_w / 2, img_h / 2);
    let y_view = y_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let uv_view = uv_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let color_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("color-uniform-planar"),
        contents: bytemuck::bytes_of(&uniform),
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("planar-bg-reuse"),
        layout: bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&y_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: color_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(&uv_view),
            },
        ],
    });
    *slot = Some(ReuseSlot {
        tex: y_tex,
        uv_tex: Some(uv_tex),
        color_buf,
        w: img_w,
        h: img_h,
        format: y_fmt,
    });
    ReuseOutcome::Rebuilt(bind_group)
}

fn vertex_buffer(
    device: &wgpu::Device,
    view: &ViewTransform,
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("vbuf"),
        contents: bytemuck::cast_slice(&quad_vertices(view, img_w, img_h, screen_w, screen_h, 0)),
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
    })
}

fn index_buffer(device: &wgpu::Device) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("ibuf"),
        contents: bytemuck::cast_slice(&INDICES),
        usage: wgpu::BufferUsages::INDEX,
    })
}

/// Scene pass: clear the (linear) intermediate to the letterbox and draw the photo
/// quad alpha-blended over it.
fn draw_scene(
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
    vbuf: &wgpu::Buffer,
    ibuf: &wgpu::Buffer,
    clear: wgpu::Color,
) {
    let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("scene"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(clear),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
    rp.set_pipeline(pipeline);
    rp.set_bind_group(0, bind_group, &[]);
    rp.set_vertex_buffer(0, vbuf.slice(..));
    rp.set_index_buffer(ibuf.slice(..), wgpu::IndexFormat::Uint16);
    rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
}

/// Clear-only scene pass: fill the intermediate with `clear` (the letterbox
/// background) and draw nothing. Used for the blank, image-free state.
fn clear_scene(encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView, clear: wgpu::Color) {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("scene-blank"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(clear),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
}

/// Tone-map pass: fullscreen-sample the intermediate into `view` (the surface).
fn draw_tonemap(
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
) {
    let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("tonemap"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
    rp.set_pipeline(pipeline);
    rp.set_bind_group(0, bind_group, &[]);
    rp.draw(0..3, 0..1);
}

fn instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        // `PRIMARY` = the per-OS primary backend: DX12 on Windows, Metal on macOS,
        // Vulkan on Linux (ADR-002). Excludes the GL secondary backend, so the
        // "no GL fallback" posture is unchanged — this just adds Metal for the port.
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    })
}

/// Per-adapter limits: conservative defaults except two raised to what *this* GPU
/// supports — `max_texture_dimension_2d` (16384 on a 5090, often 8192 elsewhere)
/// and `max_buffer_size`. The latter matters because the staging-ring upload
/// stages a whole frame in one buffer; a large Original-mode/panorama image can
/// exceed wgpu's 256 MiB default even while fitting the texture-dimension limit.
/// Keeping the rest at defaults stays portable, and `clamp_to_max` downscales
/// images that exceed whatever limit we end up with.
fn device_limits(adapter: &wgpu::Adapter) -> wgpu::Limits {
    let adapter_limits = adapter.limits();
    wgpu::Limits {
        max_texture_dimension_2d: adapter_limits.max_texture_dimension_2d,
        max_buffer_size: adapter_limits.max_buffer_size,
        ..wgpu::Limits::default()
    }
}

fn device_descriptor(limits: wgpu::Limits) -> wgpu::DeviceDescriptor<'static> {
    wgpu::DeviceDescriptor {
        label: Some("pb-render"),
        required_features: wgpu::Features::empty(),
        required_limits: limits,
        memory_hints: wgpu::MemoryHints::Performance,
    }
}

/// Request a device, opting into `TEXTURE_FORMAT_16BIT_NORM` (needed for P010's
/// `R16Unorm`/`Rg16Unorm` planes) when the adapter advertises it. On any
/// device-creation failure with the feature on, retry **without** it so the
/// renderer always comes up — P010 then falls back to CPU convert. Returns whether
/// the feature was granted (`supports_p010`). Task #91 Phase 2 (Codex Q3): Metal
/// and DX12 expose it; Vulkan and software adapters (WARP/lavapipe) may not.
async fn request_device_p010(adapter: &wgpu::Adapter) -> (wgpu::Device, wgpu::Queue, bool) {
    let want = wgpu::Features::TEXTURE_FORMAT_16BIT_NORM;
    if adapter.features().contains(want) {
        let mut desc = device_descriptor(device_limits(adapter));
        desc.required_features = want;
        if let Ok((d, q)) = adapter.request_device(&desc, None).await {
            return (d, q, true);
        }
    }
    let (d, q) = adapter
        .request_device(&device_descriptor(device_limits(adapter)), None)
        .await
        .expect("request device");
    (d, q, false)
}

/// The corner info-panel overlay, when shown.
struct OverlayDraw {
    bind_group: wgpu::BindGroup,
    vbuf: wgpu::Buffer,
    panel_w: u32,
    panel_h: u32,
    margin: u32,
    /// Secondary inset, used only by the top-right **chip** (the scan count): `margin` is
    /// its right inset, `margin_top` its top inset (so it can sit *below* the pie). `0` and
    /// ignored for every other layer.
    margin_top: u32,
}

/// The subtitle overlay, when shown (task #90.5).
///
/// Its own struct rather than an [`OverlayDraw`], because the two differ in kind: every
/// `OverlayDraw` is an **inset from a screen edge**, while a subtitle's origin is an
/// **absolute point** the core already computed (`SubtitleEngine::rect`). Squeezing it
/// into `margin`/`margin_top` would have meant two `u32`s silently meaning something else
/// for one layer — and would not have held `y`, which can be negative.
struct SubtitleDraw {
    bind_group: wgpu::BindGroup,
    vbuf: wgpu::Buffer,
    panel_w: u32,
    panel_h: u32,
    /// Top-left origin in physical px. See [`subtitle_quad_vertices`] for why `y` may be
    /// negative.
    x: f32,
    y: f32,
}

/// `PB_DOOR_DIAG=1` → per-frame draw-source diagnostics to stderr (dev-only; zero cost
/// when off — the env is read once). Pairs with the core's `[door-diag]` lines to trace
/// the "archive card over a photo" defect end to end.
fn door_diag() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PB_DOOR_DIAG").is_some())
}

/// One resident ring slot: a pre-uploaded image texture reused across photos, so
/// a keypress is a rebind (`present_slot`) — never a decode or upload. v1 slots
/// are image-sized; the win is that the texture is uploaded during prefetch, off
/// the keypress frame. Fixed-size slots + sub-rect UVs (zero prefetch-time
/// allocation) are the documented next optimization behind `reserve_ring`'s
/// `slot_w/slot_h` params.
struct RingSlot {
    bind_group: wgpu::BindGroup,
    w: u32,
    h: u32,
    /// Scene-linear peak (SDR tone-map white point on an SDR display); 1.0 for SDR.
    peak: f32,
    /// #110: the owned image texture (mip chain included for the `Original` rep), so the GPU Lanczos
    /// derive can sample it. `was_clamped`/`mode` gate derive eligibility (Original rep, mipped,
    /// `mode != 1`, not `clamp_to_max`'d).
    texture: wgpu::Texture,
    was_clamped: bool,
    mode: f32,
    /// The CONTENT's dynamic range, separate from the storage/transfer `mode` (#110 §3c): today
    /// `content_hdr == (mode == 2)`, but a derived fp16 Fit of SDR content would split them —
    /// the scene scale (SDR-white on an HDR surface) keys off content, never storage.
    content_hdr: bool,
}

/// What `render` draws this frame. Pure decision so the priority is unit-testable
/// without a GPU (task #18 finding #5): a presented ring slot wins; else the frame
/// **held** across a geometry-change ring rebuild (so a resize / scale-mode switch
/// isn't blank while the async re-decode is in flight); else the single-image path;
/// and `blank` (empty state / teardown) overrides everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrawSource {
    Blank,
    RingSlot,
    Held,
    Single,
}

/// Pick the draw source from the renderer's display flags. `ring_slot_live` = a live
/// slot is currently presented; `held_present` = a frame was carried across the last
/// ring rebuild. The bug this fixes: without `Held`, a same-index geometry change fell
/// through to `Single`, showing a stale single-image bind group instead of the frame
/// that was actually on screen.
fn choose_draw_source(blank: bool, ring_slot_live: bool, held_present: bool) -> DrawSource {
    if blank {
        DrawSource::Blank
    } else if ring_slot_live {
        DrawSource::RingSlot
    } else if held_present {
        DrawSource::Held
    } else {
        DrawSource::Single
    }
}

/// On-screen presenter for a window surface.
pub struct WgpuRenderer {
    surface: wgpu::Surface<'static>,
    /// Kept so the surface can be re-queried for its capabilities on every reconfigure. A
    /// lost/reset surface (WSLg / remote / software lavapipe, when the display connection
    /// hiccups) can momentarily report a *different* — even empty — present-mode set than at
    /// construction, and `configure` **panics** on a mode that's no longer supported. See
    /// [`WgpuRenderer::reconfigure_surface`].
    adapter: wgpu::Adapter,
    /// Mipmap-generation pipelines, built once (gpu-mipmap-hq-scaling). Used by `upload_slot` when
    /// uploading the full-res `Original` rep so trilinear sampling downscales it near-Lanczos.
    mipgen: MipGen,
    /// #110: the two-pass Lanczos derive pipelines, built once. Used by `derive_held_fit` to
    /// produce an exact-size Fit from the held Original's mip chain (no decode, no upload).
    derive: DeriveLanczos,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    /// Scene → fp16 intermediate, tone-map → surface, overlay → surface.
    scene_pipeline: wgpu::RenderPipeline,
    tonemap_pipeline: wgpu::RenderPipeline,
    overlay_pipeline: wgpu::RenderPipeline,
    /// The overlay pipeline's premultiplied-blend twin, for the subtitle layer only.
    subtitle_pipeline: wgpu::RenderPipeline,
    egui_pipeline: wgpu::RenderPipeline,
    /// Layout for image (and overlay) bind groups: tex + sampler + color uniform.
    bgl: wgpu::BindGroupLayout,
    /// Layout for the tone-map bind group (rebuilt with the intermediate on resize).
    tonemap_bgl: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    vbuf: wgpu::Buffer,
    ibuf: wgpu::Buffer,
    img_w: u32,
    img_h: u32,
    /// The fp16 scRGB-linear intermediate the scene renders into, plus the bind
    /// group the tone-map pass samples it through. Both rebuilt on resize.
    intermediate: wgpu::Texture,
    tonemap_bind_group: wgpu::BindGroup,
    /// Present uniform (displayed image's peak + HDR-surface flag).
    peak_buf: wgpu::Buffer,
    /// True when the surface is fp16 scRGB (HDR/wide-gamut display).
    hdr_surface: bool,
    /// True when the display has EDR headroom (macOS) / desktop HDR is on (Windows) —
    /// drives `wantsExtendedDynamicRangeContent` on the macOS CAMetalLayer.
    hdr_on: bool,
    /// EDR roll-off target for the present pass (macOS); 0 = pass straight through.
    edr_headroom: f32,
    /// SDR-content output scale in scRGB units (used only on an HDR surface).
    sdr_scale: f32,
    /// Per-photo view transform (scaling mode + rotation + zoom + pan).
    view: ViewTransform,
    overlay: Option<OverlayDraw>,
    /// The basic info line (`i`), its own bottom-right layer, independent of the
    /// rich-panel `overlay` slot so the two coexist (task #54).
    info_line: Option<OverlayDraw>,
    /// The transient bottom-center status toast, drawn as its own overlay layer
    /// (independent of the info panel) — e.g. "Recursive folders: on".
    toast: Option<OverlayDraw>,
    /// The top-right "loading" pie, shown while the next photo isn't ready yet
    /// (its own overlay layer, composited above the photo + panels).
    pie: Option<OverlayDraw>,
    /// The top-right scan-count chip ("12 / 1234…"), shown while a folder scan streams in,
    /// sitting just below the pie. Its own layer; cleared when the scan ends.
    chip: Option<OverlayDraw>,
    /// The subtitle cue block (task #90.5), drawn directly above the picture and below
    /// every piece of chrome — so a toast or the info line stays readable over it.
    subtitle: Option<SubtitleDraw>,
    /// A centered, persistent message panel — the empty-state "Press O to open…"
    /// hint shown over the blank background. Its own layer; cleared the moment a
    /// photo is shown (`set_image` / `present_slot`).
    message: Option<OverlayDraw>,
    /// The folder-tree panel (`Shift+F`), anchored `margin` px in from the
    /// top-left corner (the info panel's inset, mirrored). Its own layer.
    tree: Option<OverlayDraw>,
    /// The egui rich-panel overlay's bind group over a shell-owned offscreen texture
    /// (`Rgba8UnormSrgb`). Retained across nav frames — the shell only re-hands it on
    /// (re)allocation / resize, and re-renders egui *into* the same texture when a
    /// panel changes, so per-nav-frame egui cost is zero (task #54 Phase 4). Composited
    /// bufferless-fullscreen, above the CPU overlay layers.
    egui: Option<wgpu::BindGroup>,
    upload: Box<dyn UploadStrategy>,
    /// Resident texture ring (Phase 3). Empty until `reserve_ring`; each `Some`
    /// slot holds a pre-uploaded photo. `present_slot` selects which one draws.
    ring: Vec<Option<RingSlot>>,
    /// When `Some(i)`, `render` draws ring slot `i` instead of `bind_group`.
    present_idx: Option<usize>,
    /// The frame that was on screen when the ring was last rebuilt (`reserve_ring` on a
    /// geometry change), moved out of the ring so it survives the rebuild. While the async
    /// re-decode is in flight, `render` draws this (GPU-refit to the new viewport) instead of
    /// the stale single-image `bind_group`, so a resize / scale-mode switch / deck rebuild
    /// never flashes blank or a wrong frame (task #18 finding #5). One image's worth of
    /// texture, outside the ring budget; released the moment a real frame is presented.
    held: Option<RingSlot>,
    /// Background (letterbox) fill, sRGB, shown around a non-covering photo.
    /// Defaults to [`LETTERBOX`]; the app overrides it from user settings.
    letterbox: [u8; 3],
    /// Spike (task #59): height in **physical px** of a translucent top bar the surface
    /// extends *under* (a glass toolbar). The photo is fit/centered against the content region
    /// below it (`surface_h - inset`) and the quad is offset down by `inset`, so a fitted photo
    /// stays fully below the bar while a zoomed/cropped one's natural overflow shows under the
    /// glass. `0` (default) = the classic opaque-bar behavior, untouched.
    content_top_inset: u32,
    /// No image loaded (bare launch with no folder, or the last photo deleted):
    /// draw just the letterbox background, no photo quad. Cleared by `set_image` /
    /// `present_slot`, set by `clear_image`.
    blank: bool,
    /// The reusable single-image present slot (task #79 phase 3): while
    /// `set_image` keeps receiving frames of the same geometry/format — the
    /// animation / video playback steady state — pixels upload into this slot's
    /// existing texture and only its color uniform is rewritten, instead of
    /// creating a texture + view + sampler + uniform + bind group **per frame**
    /// (the #76 follow-up). Rebuilt automatically on any size/format change.
    reuse: Option<ReuseSlot>,
    /// Planar scene pipeline + its two-texture layout (task 79.10 NV12; task #91
    /// Phase 2 P010 — video frames from the hardware-decode producer; YUV→RGB +
    /// transfer happen in `fs_scene_planar`).
    scene_planar_pipeline: wgpu::RenderPipeline,
    planar_bgl: wgpu::BindGroupLayout,
    /// What `bind_group` currently holds — picks the scene pipeline for the
    /// single-image draw (`set_image` sets `Rgba`, ring slots are always RGBA).
    scene_kind: SceneKind,
    /// Whether this device was created with `TEXTURE_FORMAT_16BIT_NORM` — P010's
    /// `R16Unorm`/`Rg16Unorm` textures need it. When false, the producer must fall
    /// back to CPU convert for P010 (task #91 Phase 2, Codex Q3).
    supports_p010: bool,
}

/// What the single-image `bind_group` currently holds, so `render` picks the
/// right scene pipeline (task #91 Phase 2 — replaces the `scene_is_nv12` bool).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SceneKind {
    /// An RGBA8 / Rgba16Float image — the `scene` pipeline.
    Rgba,
    /// A planar (NV12 / P010) two-plane video frame — the `scene_planar` pipeline.
    Planar,
}

/// See [`WgpuRenderer::reuse`]. Same-queue submission order makes the in-place
/// texture write safe: the upload copy is ordered after every previously
/// submitted draw that sampled the texture, and the next draw samples the new
/// frame — no fence, no wait.
///
/// The slot deliberately does **not** hold the bind group (wgpu 22 resources
/// aren't `Clone`): the renderer's `bind_group` field is the one built against
/// this slot's texture — a [`ReuseOutcome::Reused`] means "keep it".
struct ReuseSlot {
    tex: wgpu::Texture,
    /// The NV12 chroma plane (`Rg8Unorm`, half-res) when this slot holds a video
    /// frame's two-plane layout (task 79.10); `None` for the RGBA formats. `tex`
    /// is then the `R8Unorm` luma plane.
    uv_tex: Option<wgpu::Texture>,
    /// `UNIFORM | COPY_DST` so per-frame color/mode/scale changes are a
    /// `write_buffer`, not a new buffer + bind group.
    color_buf: wgpu::Buffer,
    w: u32,
    h: u32,
    format: wgpu::TextureFormat,
}

/// What [`upload_image_reusable`] did with the frame.
enum ReuseOutcome {
    /// Uploaded into the existing slot texture; the caller's current bind group
    /// (built over that texture on the last rebuild) stays valid — keep it.
    Reused,
    /// Geometry/format changed: the slot was rebuilt and this is its new bind group.
    Rebuilt(wgpu::BindGroup),
}

/// Pick the swapchain present mode. Mailbox = low latency, no tearing; fall back to Fifo
/// if unsupported. On a software (Cpu) adapter — lavapipe under WSLg — prefer Fifo even when
/// Mailbox is advertised: its triple-buffering is unstable there while Fifo (plain vsync) is
/// steadier. **`PB_PRESENT_FIFO=1` forces Fifo** — an A/B lever for the present-drop bug, since
/// Mailbox's `Present(0,0)` is the DXGI/DWM/RDP path we suspect silently discards frames.
fn preferred_present_mode(
    caps: &wgpu::SurfaceCapabilities,
    device_type: wgpu::DeviceType,
) -> wgpu::PresentMode {
    let force_fifo = std::env::var("PB_PRESENT_FIFO").is_ok_and(|v| v == "1");
    let want_mailbox = !force_fifo
        && caps.present_modes.contains(&wgpu::PresentMode::Mailbox)
        && device_type != wgpu::DeviceType::Cpu;
    if want_mailbox {
        wgpu::PresentMode::Mailbox
    } else if caps.present_modes.contains(&wgpu::PresentMode::Fifo) {
        wgpu::PresentMode::Fifo
    } else {
        // Fifo is guaranteed by the spec, but be defensive on odd software surfaces.
        caps.present_modes
            .first()
            .copied()
            .unwrap_or(wgpu::PresentMode::Fifo)
    }
}

impl WgpuRenderer {
    /// Create a presenter for `target` (e.g. an `Arc<Window>`) and upload `image`
    /// with its `color` transform (use [`ColorTransform::srgb`] for sRGB sources).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        target: impl Into<wgpu::SurfaceTarget<'static>>,
        width: u32,
        height: u32,
        image: &[u8],
        img_w: u32,
        img_h: u32,
        color: ColorTransform,
        hdr: bool,
        peak: f32,
    ) -> Self {
        let instance = instance();
        let surface = instance.create_surface(target).expect("create surface");
        pollster::block_on(Self::new_async(
            instance, surface, width, height, image, img_w, img_h, color, hdr, peak,
        ))
    }

    /// macOS-only: create the presenter on a **host-owned, retained `CAMetalLayer`** — the
    /// AppKit/SwiftUI shell path (NS1, ADR-021), where the window/view/layer belong to the
    /// Swift host and Rust only draws into the layer it's handed. winit is absent on that
    /// target, so there is no raw-window-handle; the surface goes through wgpu's unsafe
    /// `SurfaceTargetUnsafe::CoreAnimationLayer` instead.
    ///
    /// # Safety
    /// - `layer` must point to a valid, **retained** `CAMetalLayer`, and it must outlive
    ///   the returned renderer — the host drops the renderer *before* the view/layer dies.
    /// - Creation, `resize`, `render`, and drop must all happen on the **main thread**
    ///   (AppKit layers are not safe to reconfigure off it); the FFI effect-drain rule
    ///   already pins the callers there.
    /// - The host reports size changes (`resize`) before drawing at a new size.
    #[cfg(target_os = "macos")]
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn new_from_ca_layer(
        layer: *mut std::ffi::c_void,
        width: u32,
        height: u32,
        image: &[u8],
        img_w: u32,
        img_h: u32,
        color: ColorTransform,
        hdr: bool,
        peak: f32,
    ) -> Self {
        let instance = instance();
        // SAFETY: the caller upholds the layer contract documented above.
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(layer))
        }
        .expect("create surface from CAMetalLayer");
        pollster::block_on(Self::new_async(
            instance, surface, width, height, image, img_w, img_h, color, hdr, peak,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn new_async(
        instance: wgpu::Instance,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
        image: &[u8],
        img_w: u32,
        img_h: u32,
        color: ColorTransform,
        hdr: bool,
        peak: f32,
    ) -> Self {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
            })
            .await
            .expect("no compatible GPU adapter");
        let (device, queue, supports_p010) = request_device_p010(&adapter).await;

        let caps = surface.get_capabilities(&adapter);
        // HDR/wide-gamut output: when the desktop is in HDR mode and the surface can
        // be `Rgba16Float`, present that — a float flip-model swapchain is always
        // **scRGB** (linear, BT.709, extended range), so wide-gamut/HDR values reach
        // the panel. Otherwise pick an **8-bit non-sRGB** surface (the present pass
        // sRGB-encodes; an sRGB surface would double-encode and washes out).
        let disp = crate::display::primary_hdr();
        // Use the fp16 scRGB surface whenever the panel benefits: HDR-on (extended
        // range), OR wide-gamut (P3+). On macOS `wide_gamut` is set even for an SDR P3
        // panel, so P3 photos light up there without needing EDR (the surface's
        // CAMetalLayer is configured to extended-linear-sRGB in pb-app); on Windows
        // `wide_gamut` tracks `hdr_on`, so behavior is unchanged.
        let want_fp16 = (disp.hdr_on || disp.wide_gamut)
            && caps.formats.contains(&wgpu::TextureFormat::Rgba16Float);
        let format = if want_fp16 {
            wgpu::TextureFormat::Rgba16Float
        } else {
            caps.formats
                .iter()
                .copied()
                .find(|f| {
                    matches!(
                        f,
                        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm
                    )
                })
                .or_else(|| caps.formats.iter().copied().find(|f| !f.is_srgb()))
                .unwrap_or(caps.formats[0])
        };
        // See `preferred_present_mode` for the Mailbox/Fifo policy and the `PB_PRESENT_FIFO`
        // A/B lever. Real GPUs (DX12 / Metal / Vulkan) keep Mailbox — the low-latency path is
        // the point; software adapters and the forced-Fifo lever get plain vsync.
        let present_mode = preferred_present_mode(&caps, adapter.get_info().device_type);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let sdr_scale = disp.sdr_scale();
        let scene_scale = if want_fp16 && !hdr { sdr_scale } else { 1.0 };
        let view = ViewTransform::default();
        let pipelines = build_pipelines(&device, format);
        let mipgen = build_mipgen(&device);
        let derive = build_derive(&device);
        let mut upload: Box<dyn UploadStrategy> = Box::new(StagingUpload::new());
        let bind_group = upload_image(
            &device,
            &queue,
            &pipelines.scene_bgl,
            upload.as_mut(),
            image,
            img_w,
            img_h,
            &color,
            hdr,
            scene_scale,
            None, // single-image / animation path — never mipped (re-uploaded per frame)
        );
        let vbuf = vertex_buffer(&device, &view, img_w, img_h, config.width, config.height);
        let ibuf = index_buffer(&device);

        // Present uniform (peak for SDR tone-map; HDR-surface flag) + the fp16
        // intermediate the scene renders into.
        let peak_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("present-uniform"),
            contents: bytemuck::bytes_of(&PresentUniform::new(peak, want_fp16, disp.edr_headroom)),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let (intermediate, tonemap_bind_group) = make_intermediate(
            &device,
            &pipelines.tonemap_bgl,
            &peak_buf,
            config.width,
            config.height,
        );

        Self {
            surface,
            adapter,
            mipgen,
            derive,
            device,
            queue,
            config,
            scene_pipeline: pipelines.scene,
            tonemap_pipeline: pipelines.tonemap,
            overlay_pipeline: pipelines.overlay,
            subtitle_pipeline: pipelines.subtitle,
            egui_pipeline: pipelines.egui,
            bgl: pipelines.scene_bgl,
            tonemap_bgl: pipelines.tonemap_bgl,
            bind_group,
            vbuf,
            ibuf,
            img_w,
            img_h,
            intermediate,
            tonemap_bind_group,
            peak_buf,
            hdr_surface: want_fp16,
            hdr_on: disp.hdr_on,
            edr_headroom: disp.edr_headroom,
            sdr_scale,
            view,
            overlay: None,
            info_line: None,
            toast: None,
            pie: None,
            chip: None,
            subtitle: None,
            upload,
            ring: Vec::new(),
            present_idx: None,
            held: None,
            letterbox: [LETTERBOX[0], LETTERBOX[1], LETTERBOX[2]],
            content_top_inset: 0,
            blank: false,
            reuse: None,
            message: None,
            tree: None,
            egui: None,
            scene_planar_pipeline: pipelines.scene_planar,
            planar_bgl: pipelines.planar_bgl,
            scene_kind: SceneKind::Rgba,
            supports_p010,
        }
    }

    /// The per-image output scale: SDR content on an HDR surface is lifted to the
    /// SDR white level; HDR content (and any SDR-surface content) uses 1.0.
    fn scene_scale(&self, hdr: bool) -> f32 {
        if self.hdr_surface && !hdr {
            self.sdr_scale
        } else {
            1.0
        }
    }

    /// Update the present uniform's tone-map white point to the displayed image's
    /// peak (only consequential on an SDR surface; harmless on an HDR surface).
    fn set_present_peak(&self, peak: f32) {
        self.queue.write_buffer(
            &self.peak_buf,
            0,
            bytemuck::bytes_of(&PresentUniform::new(
                peak,
                self.hdr_surface,
                self.edr_headroom,
            )),
        );
    }

    /// The display refresh cap helper will live here in Phase 3; for now the app
    /// reads it from winit directly.
    pub fn present_mode(&self) -> wgpu::PresentMode {
        self.config.present_mode
    }

    /// Reconfigure the surface, **re-querying its capabilities first**. A lost/reset surface —
    /// common on WSLg / remote / software (lavapipe) backends when the display connection
    /// hiccups ("Connection reset by peer") — can report a shrunken or **empty** present-mode
    /// set, and `wgpu`'s `configure` *panics* on a present mode that's no longer in the list
    /// (the crash: "Requested present mode Mailbox is not in the list of supported present
    /// modes: []"). So skip entirely while the surface reports no usable modes — the frame is
    /// dropped and the next one retries once the connection recovers — and otherwise clamp the
    /// present mode (and format / alpha) to something actually supported (Mailbox if available,
    /// else the spec-guaranteed Fifo). Returns whether the surface was (re)configured.
    ///
    /// A no-op change on healthy surfaces (Windows DX12 / macOS Metal): the caps are stable, so
    /// the config is re-applied unchanged — this only adds a safety net where the surface can
    /// vanish out from under us.
    fn reconfigure_surface(&mut self) -> bool {
        let caps = self.surface.get_capabilities(&self.adapter);
        if caps.present_modes.is_empty() || caps.formats.is_empty() {
            // Surface not currently usable (lost / reset). Configuring now would panic — drop
            // the frame and retry next time; the surface comes back when the display does.
            return false;
        }
        if !caps.present_modes.contains(&self.config.present_mode) {
            // Re-pick via the same policy the constructor used, so `PB_PRESENT_FIFO` still holds
            // through a recovery (else a mode clamp could silently restore Mailbox mid-capture).
            self.config.present_mode =
                preferred_present_mode(&caps, self.adapter.get_info().device_type);
        }
        if !caps.formats.contains(&self.config.format) {
            self.config.format = caps.formats[0];
        }
        if !caps.alpha_modes.contains(&self.config.alpha_mode) {
            self.config.alpha_mode = caps.alpha_modes[0];
        }
        self.surface.configure(&self.device, &self.config);
        true
    }
}

impl Renderer for WgpuRenderer {
    /// Override the letterbox / background fill color (sRGB), shown around a photo
    /// that doesn't cover the screen. Takes effect on the next `render`. Off the
    /// photo hot path — set from user settings, not per frame.
    fn surface_size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    fn set_letterbox(&mut self, rgb: [u8; 3]) {
        self.letterbox = rgb;
    }

    fn set_content_top_inset(&mut self, px: u32) {
        self.content_top_inset = px;
    }

    /// Set or clear the transient bottom-center status toast (tasks.json #10). It
    /// is an independent overlay layer, so it composites *over* the info panel
    /// rather than replacing it; the caller fades it by re-uploading with scaled
    /// alpha. `bottom_margin` is the gap from the bottom edge.
    fn set_toast(&mut self, panel: Option<(&[u8], u32, u32)>, bottom_margin: u32) {
        self.toast = match panel {
            Some((rgba, w, h)) => {
                let scale = self.scene_scale(false);
                let bind_group = upload_image(
                    &self.device,
                    &self.queue,
                    &self.bgl,
                    self.upload.as_mut(),
                    rgba,
                    w,
                    h,
                    &ColorTransform::srgb(),
                    false,
                    scale,
                    None, // UI bitmap — never mipped
                );
                let vbuf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("toast-vbuf"),
                        contents: bytemuck::cast_slice(&toast_quad_vertices(
                            w,
                            h,
                            self.config.width,
                            self.config.height,
                            bottom_margin,
                        )),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
                Some(OverlayDraw {
                    bind_group,
                    vbuf,
                    panel_w: w,
                    panel_h: h,
                    margin: bottom_margin,
                    margin_top: 0,
                })
            }
            None => None,
        };
    }

    /// Set or clear the top-right "loading" pie (shown while the next photo isn't
    /// ready). Its own overlay layer, composited above the photo and the panels;
    /// the caller animates the fill / fade by re-uploading the rasterized bitmap.
    /// `margin` is the gap from the top and right edges.
    fn set_pie(&mut self, panel: Option<(&[u8], u32, u32)>, margin: u32) {
        self.pie = match panel {
            Some((rgba, w, h)) => {
                let scale = self.scene_scale(false);
                let bind_group = upload_image(
                    &self.device,
                    &self.queue,
                    &self.bgl,
                    self.upload.as_mut(),
                    rgba,
                    w,
                    h,
                    &ColorTransform::srgb(),
                    false,
                    scale,
                    None, // UI bitmap — never mipped
                );
                let vbuf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("pie-vbuf"),
                        contents: bytemuck::cast_slice(&top_right_quad_vertices(
                            w,
                            h,
                            self.config.width,
                            self.config.height,
                            margin,
                        )),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
                Some(OverlayDraw {
                    bind_group,
                    vbuf,
                    panel_w: w,
                    panel_h: h,
                    margin,
                    margin_top: 0,
                })
            }
            None => None,
        };
    }

    fn device(&self) -> &wgpu::Device {
        &self.device
    }

    fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    fn set_egui_overlay(&mut self, texture: Option<&wgpu::Texture>) {
        self.egui = texture.map(|tex| {
            // Sample through the texture's own (sRGB) format so the sampler decodes the
            // store back to premultiplied linear; `fs_egui` then composites it straight.
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });
            // Only `scale.x` is read by `fs_egui` — the SDR-white lift on an HDR surface
            // (1.0 on SDR), matching the CPU overlay layers.
            let color_buf = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("egui-color-uniform"),
                    contents: bytemuck::bytes_of(&ColorUniform::new(
                        &ColorTransform::srgb(),
                        0.0,
                        self.scene_scale(false),
                    )),
                    usage: wgpu::BufferUsages::UNIFORM,
                });
            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("egui-bg"),
                layout: &self.bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: color_buf.as_entire_binding(),
                    },
                ],
            })
        });
    }

    fn set_edr_headroom(&mut self, headroom: f32) {
        self.edr_headroom = headroom.max(1.0);
        // Peak is unused on the HDR surface (the present pass keys off the headroom),
        // so any value is fine here; the next per-image `set_present_peak` refreshes it.
        self.queue.write_buffer(
            &self.peak_buf,
            0,
            bytemuck::bytes_of(&PresentUniform::new(
                1.0,
                self.hdr_surface,
                self.edr_headroom,
            )),
        );
    }

    fn hdr_surface_wants_edr(&self) -> Option<bool> {
        self.hdr_surface.then_some(self.hdr_on)
    }

    fn image_size(&self) -> (u32, u32) {
        (self.img_w, self.img_h)
    }

    fn poll(&self) {
        self.device.poll(wgpu::Maintain::Poll);
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if width == self.config.width && height == self.config.height {
            return; // already correctly sized — avoid a redundant reconfigure
        }
        self.config.width = width;
        self.config.height = height;
        self.reconfigure_surface();
        // The fp16 intermediate must track the surface size.
        let (intermediate, tonemap_bind_group) = make_intermediate(
            &self.device,
            &self.tonemap_bgl,
            &self.peak_buf,
            width,
            height,
        );
        self.intermediate = intermediate;
        self.tonemap_bind_group = tonemap_bind_group;
        // Re-place the quad for the new viewport.
        self.queue.write_buffer(
            &self.vbuf,
            0,
            bytemuck::cast_slice(&quad_vertices(
                &self.view,
                self.img_w,
                self.img_h,
                width,
                height,
                self.content_top_inset,
            )),
        );
        // The overlay panel's corner position depends on the viewport.
        if let Some(ov) = &self.overlay {
            self.queue.write_buffer(
                &ov.vbuf,
                0,
                bytemuck::cast_slice(&overlay_quad_vertices(
                    ov.panel_w, ov.panel_h, width, height, ov.margin,
                )),
            );
        }
        if let Some(t) = &self.toast {
            self.queue.write_buffer(
                &t.vbuf,
                0,
                bytemuck::cast_slice(&toast_quad_vertices(
                    t.panel_w, t.panel_h, width, height, t.margin,
                )),
            );
        }
        if let Some(p) = &self.pie {
            self.queue.write_buffer(
                &p.vbuf,
                0,
                bytemuck::cast_slice(&top_right_quad_vertices(
                    p.panel_w, p.panel_h, width, height, p.margin,
                )),
            );
        }
        if let Some(c) = &self.chip {
            self.queue.write_buffer(
                &c.vbuf,
                0,
                bytemuck::cast_slice(&top_right_quad_xy(
                    c.panel_w,
                    c.panel_h,
                    width,
                    height,
                    c.margin,
                    c.margin_top,
                )),
            );
        }
        if let Some(m) = &self.message {
            self.queue.write_buffer(
                &m.vbuf,
                0,
                bytemuck::cast_slice(&center_quad_vertices(m.panel_w, m.panel_h, width, height)),
            );
        }
        if let Some(t) = &self.tree {
            self.queue.write_buffer(
                &t.vbuf,
                0,
                bytemuck::cast_slice(&top_left_quad_vertices(
                    t.panel_w, t.panel_h, width, height, t.margin,
                )),
            );
        }
        // The subtitle's origin is the core's, and it is about to recompute it against the
        // new viewport (a resize re-places the video, which re-runs `tick_subtitles`). Its
        // vertices are still NDC, though, so they must be re-derived from the *new* screen
        // size or the block jumps and stretches for the frame in between. Same px origin,
        // new NDC: correct now, and replaced by the core's answer a tick later.
        if let Some(s) = &self.subtitle {
            self.queue.write_buffer(
                &s.vbuf,
                0,
                bytemuck::cast_slice(&subtitle_quad_vertices(
                    s.panel_w, s.panel_h, width, height, s.x, s.y,
                )),
            );
        }
    }

    fn clear_image(&mut self) {
        // Show the letterbox background only; the kept bind group/ring is simply not
        // drawn until the next set_image / present_slot (see `render`).
        self.blank = true;
        self.present_idx = None;
        self.held = None; // empty state / teardown: drop any held frame
    }

    fn set_image(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        color: ColorTransform,
        hdr: bool,
        peak: f32,
    ) {
        let scale = self.scene_scale(hdr);
        // The reusable slot (task #79 phase 3): during animation/video playback this
        // is the per-frame path — same-geometry frames upload in place, creating
        // nothing; `self.bind_group` (built over the slot's texture on the last
        // rebuild) stays valid on a `Reused` outcome. The invariant that holds this
        // together: `set_image` is the only writer of both `reuse` and `bind_group`,
        // so a reuse hit always follows the rebuild that paired them.
        match upload_image_reusable(
            &self.device,
            &self.queue,
            &self.bgl,
            self.upload.as_mut(),
            &mut self.reuse,
            rgba,
            width,
            height,
            &color,
            hdr,
            scale,
        ) {
            ReuseOutcome::Rebuilt(bg) => self.bind_group = bg,
            ReuseOutcome::Reused => {}
        }
        self.scene_kind = SceneKind::Rgba; // an RGBA bind group draws with the RGBA pipeline
        self.blank = false; // an image is showing again
        self.message = None; // hide the empty-state hint
        self.held = None; // the new single image supersedes any held frame
        self.set_present_peak(peak);
        // Revert to the single-image path; a later present_slot re-selects a slot.
        self.present_idx = None;
        self.img_w = width;
        self.img_h = height;
        // Re-place the quad for the new image.
        self.queue.write_buffer(
            &self.vbuf,
            0,
            bytemuck::cast_slice(&quad_vertices(
                &self.view,
                width,
                height,
                self.config.width,
                self.config.height,
                self.content_top_inset,
            )),
        );
    }

    fn supports_p010(&self) -> bool {
        self.supports_p010
    }

    /// The wgpu override of the trait's CPU fallback (task 79.10 NV12; task #91
    /// Phase 2 P010/PQ/HLG): the planes upload into the two-plane reuse slot and
    /// `fs_scene_planar` converts (YUV + range + transfer + primaries) on the GPU.
    /// `PB_VIDEO_CPU_CONVERT=1` forces the CPU path (the A/B lever and the escape
    /// hatch if a driver misbehaves). P010 on a device without
    /// `TEXTURE_FORMAT_16BIT_NORM` also takes the CPU path.
    fn set_video_planar(
        &mut self,
        y: &[u8],
        uv: &[u8],
        width: u32,
        height: u32,
        p: PlanarPresentation,
    ) {
        let cpu_hatch = std::env::var_os("PB_VIDEO_CPU_CONVERT").is_some_and(|v| v == "1");
        let needs_16bit = p.format.is_ten_bit() && !self.supports_p010;
        if cpu_hatch || needs_16bit {
            let f = crate::yuv::planar_to_scene(
                y, uv, width, height, p.format, p.yuv, p.transfer, &p.color, p.peak,
            );
            self.set_image(&f.bytes, width, height, p.color, f.hdr, f.peak);
            return;
        }
        let hdr = p.transfer.is_hdr();
        let scale = self.scene_scale(hdr);
        let mode = planar_mode(p.transfer);
        match upload_planar_reusable(
            &self.device,
            &self.queue,
            &self.planar_bgl,
            self.upload.as_mut(),
            &mut self.reuse,
            y,
            uv,
            width,
            height,
            &p.color,
            p.yuv,
            p.format,
            mode,
            scale,
        ) {
            ReuseOutcome::Rebuilt(bg) => self.bind_group = bg,
            ReuseOutcome::Reused => {}
        }
        self.scene_kind = SceneKind::Planar;
        self.blank = false;
        self.message = None;
        self.held = None; // a live video frame supersedes any held still
                          // HDR video tone-maps like an HDR still (peak drives the SDR present);
                          // SDR video is peak 1.0 (identity).
        self.set_present_peak(if hdr { p.peak } else { 1.0 });
        self.present_idx = None;
        self.img_w = width;
        self.img_h = height;
        self.queue.write_buffer(
            &self.vbuf,
            0,
            bytemuck::cast_slice(&quad_vertices(
                &self.view,
                width,
                height,
                self.config.width,
                self.config.height,
                self.content_top_inset,
            )),
        );
    }

    fn set_view(&mut self, view: ViewTransform) {
        self.view = view;
        self.queue.write_buffer(
            &self.vbuf,
            0,
            bytemuck::cast_slice(&quad_vertices(
                &self.view,
                self.img_w,
                self.img_h,
                self.config.width,
                self.config.height,
                self.content_top_inset,
            )),
        );
    }

    fn set_overlay(
        &mut self,
        panel: Option<(&[u8], u32, u32)>,
        right_margin: u32,
        bottom_margin: u32,
    ) {
        self.overlay = match panel {
            Some((rgba, w, h)) => {
                // Overlays (Inspector / help) are sRGB UI bitmaps, composited into
                // the linear intermediate; scaled to SDR white on an HDR surface.
                let scale = self.scene_scale(false);
                let bind_group = upload_image(
                    &self.device,
                    &self.queue,
                    &self.bgl,
                    self.upload.as_mut(),
                    rgba,
                    w,
                    h,
                    &ColorTransform::srgb(),
                    false,
                    scale,
                    None, // UI bitmap — never mipped
                );
                let vbuf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("overlay-vbuf"),
                        contents: bytemuck::cast_slice(&bottom_right_quad_xy(
                            w,
                            h,
                            self.config.width,
                            self.config.height,
                            right_margin,
                            bottom_margin,
                        )),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
                Some(OverlayDraw {
                    bind_group,
                    vbuf,
                    panel_w: w,
                    panel_h: h,
                    margin: right_margin,
                    margin_top: bottom_margin,
                })
            }
            None => None,
        };
    }

    /// Set or clear the basic info line — its own bottom-anchored layer (`align`:
    /// left / center / right), so it coexists with the rich-panel `overlay` slot.
    /// Drawn like the info panel: an sRGB UI bitmap composited into the intermediate.
    fn set_info_line(
        &mut self,
        panel: Option<(&[u8], u32, u32)>,
        margin: u32,
        align: crate::HAlign,
    ) {
        self.info_line = match panel {
            Some((rgba, w, h)) => {
                let scale = self.scene_scale(false);
                let bind_group = upload_image(
                    &self.device,
                    &self.queue,
                    &self.bgl,
                    self.upload.as_mut(),
                    rgba,
                    w,
                    h,
                    &ColorTransform::srgb(),
                    false,
                    scale,
                    None, // UI bitmap — never mipped
                );
                let vbuf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("info-line-vbuf"),
                        contents: bytemuck::cast_slice(&bottom_aligned_quad(
                            w,
                            h,
                            self.config.width,
                            self.config.height,
                            margin,
                            align,
                        )),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
                Some(OverlayDraw {
                    bind_group,
                    vbuf,
                    panel_w: w,
                    panel_h: h,
                    margin,
                    margin_top: 0,
                })
            }
            None => None,
        };
    }

    /// Set or clear the folder-tree panel (`Shift+F`): drawn `margin` px in from the
    /// top and left edges. Its own overlay layer, drawn like the info panel.
    fn set_tree(&mut self, panel: Option<(&[u8], u32, u32)>, margin: u32) {
        self.tree = match panel {
            Some((rgba, w, h)) => {
                let scale = self.scene_scale(false);
                let bind_group = upload_image(
                    &self.device,
                    &self.queue,
                    &self.bgl,
                    self.upload.as_mut(),
                    rgba,
                    w,
                    h,
                    &ColorTransform::srgb(),
                    false,
                    scale,
                    None, // UI bitmap — never mipped
                );
                let vbuf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("tree-vbuf"),
                        contents: bytemuck::cast_slice(&top_left_quad_vertices(
                            w,
                            h,
                            self.config.width,
                            self.config.height,
                            margin,
                        )),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
                Some(OverlayDraw {
                    bind_group,
                    vbuf,
                    panel_w: w,
                    panel_h: h,
                    margin,
                    margin_top: 0,
                })
            }
            None => None,
        };
    }

    /// Set or clear the subtitle cue block at an absolute `(x, y)` in physical px.
    ///
    /// Re-uploaded only when the core says the bitmap changed (its `gen` token) — a cue
    /// holds on screen for seconds, so this runs on a cue change, never per frame.
    fn set_subtitle_overlay(&mut self, panel: Option<(&[u8], u32, u32)>, x: f32, y: f32) {
        self.subtitle = match panel {
            Some((rgba, w, h)) => {
                let scale = self.scene_scale(false);
                let bind_group = upload_image(
                    &self.device,
                    &self.queue,
                    &self.bgl,
                    self.upload.as_mut(),
                    rgba,
                    w,
                    h,
                    &ColorTransform::srgb(),
                    false,
                    scale,
                    None, // UI bitmap — never mipped
                );
                let vbuf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("subtitle-vbuf"),
                        contents: bytemuck::cast_slice(&subtitle_quad_vertices(
                            w,
                            h,
                            self.config.width,
                            self.config.height,
                            x,
                            y,
                        )),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
                Some(SubtitleDraw {
                    bind_group,
                    vbuf,
                    panel_w: w,
                    panel_h: h,
                    x,
                    y,
                })
            }
            None => None,
        };
    }

    fn reserve_ring(&mut self, capacity: usize, _slot_w: u32, _slot_h: u32) {
        // A geometry change rebuilds the ring empty. Move the frame that was on screen out
        // of the ring into `held` first, so `render` can keep showing it (GPU-refit to the
        // new viewport) until the async re-decode presents — no blank/freeze (task #18 #5).
        // Only overwrite `held` when there IS a live presented slot; a `present_idx` of None
        // means we're already showing a held (or single-image) frame, which must be
        // preserved across a *repeated* invalidation rather than dropped.
        if let Some(slot) = self
            .present_idx
            .and_then(|i| self.ring.get_mut(i).and_then(Option::take))
        {
            // Keep img_w/img_h in sync with the held frame so the follow-up `resize`/`set_view`
            // re-places the quad for *its* dimensions (not the incoming image's).
            self.img_w = slot.w;
            self.img_h = slot.h;
            self.held = Some(slot);
        }
        self.ring = (0..capacity).map(|_| None).collect();
        self.present_idx = None;
    }

    #[allow(clippy::too_many_arguments)]
    fn upload_slot(
        &mut self,
        slot: usize,
        rgba: &[u8],
        w: u32,
        h: u32,
        color: ColorTransform,
        hdr: bool,
        peak: f32,
        mip: bool,
    ) {
        if slot >= self.ring.len() {
            return;
        }
        let scale = self.scene_scale(hdr);
        // `mip` (only the full-res Original rep passes true) builds a mipmap chain so trilinear
        // fit-downscaling is near-Lanczos. Disjoint field borrows: `self.upload` (mut) vs
        // `self.mipgen` (shared).
        let mipgen = mip.then_some(&self.mipgen);
        let uploaded = create_image_texture(
            &self.device,
            &self.queue,
            &self.bgl,
            self.upload.as_mut(),
            rgba,
            w,
            h,
            &color,
            hdr,
            scale,
            mipgen,
        );
        self.ring[slot] = Some(RingSlot {
            bind_group: uploaded.bind_group,
            w,
            h,
            peak,
            texture: uploaded.texture,
            was_clamped: uploaded.was_clamped,
            mode: uploaded.mode,
            content_hdr: hdr,
        });
    }

    fn remap_ring(&mut self, new_capacity: usize, remaps: &[pb_core::SlotRemap]) -> Vec<usize> {
        let mut old = std::mem::take(&mut self.ring);
        let mut next: Vec<Option<RingSlot>> = (0..new_capacity).map(|_| None).collect();
        let mut moved = Vec::new();
        for r in remaps {
            // Release-validated (not just debug-asserted): an out-of-range or duplicate `to`
            // is a caller bug — skip the move (the slot stays empty; the re-present falls
            // through to the held fallback / async decode) rather than panic or clobber.
            if r.to >= new_capacity || next[r.to].is_some() {
                continue;
            }
            if let Some(slot) = old.get_mut(r.from).and_then(Option::take) {
                next[r.to] = Some(slot);
                moved.push(r.to);
            }
        }
        // Held fallback: the presented texture, when NOT itself relocated (already taken by a
        // remap above), moves into `held` with img dims synced — exactly `reserve_ring`'s
        // stash — so a no-retained-hit geometry change still shows the old frame, never blank.
        if let Some(slot) = self
            .present_idx
            .and_then(|i| old.get_mut(i).and_then(Option::take))
        {
            self.img_w = slot.w;
            self.img_h = slot.h;
            self.held = Some(slot);
        }
        self.ring = next;
        self.present_idx = None;
        moved
    }

    fn derive_fit(
        &mut self,
        source: DeriveSource,
        dst_slot: usize,
        fit_w: u32,
        fit_h: u32,
        kernel: u32,
        mip_bias: i32,
    ) -> Option<DerivedFit> {
        if dst_slot >= self.ring.len() || fit_w == 0 || fit_h == 0 {
            return None;
        }
        let src = match source {
            DeriveSource::Held => self.held.as_ref()?,
            // Deriving INTO the source slot would drop the Original mid-derive — refuse.
            DeriveSource::Ring(s) if s == dst_slot => return None,
            DeriveSource::Ring(s) => self.ring.get(s)?.as_ref()?,
        };
        // Eligibility (#110 §6): a genuine mipped Original — only Originals are mipped, so the
        // chain length IS the rep check — never `clamp_to_max`'d (nearest-neighbour aliasing is
        // baked into every mip), never source-ICC mode 1 (its TRC isn't in the derive chain).
        if src.was_clamped || src.mode == 1.0 || src.texture.mip_level_count() <= 1 {
            return None;
        }
        // ACTUAL uploaded dims from the texture (RingSlot.w/h retain pre-clamp dims by
        // contract; a clamped slot was rejected above, but the texture stays the source of
        // truth for kernel geometry regardless).
        let (tw, th) = (src.texture.width(), src.texture.height());
        let (dw, dh) = contain_dims(tw, th, fit_w, fit_h);
        let level = select_derive_mip(tw, th, src.texture.mip_level_count(), dw, dh, mip_bias);
        // Bound the transient fp16 H-intermediate (dst_w × source-mip height × 8B) — it is
        // allocated OUTSIDE the ring budget for one submission, so an unbounded scratch could
        // spike peak VRAM past a small GPU's headroom (Codex 110b review P1). ~236 MB covers
        // the worst realistic case on the 7680-wide target (a display-res photo from L0); a
        // refusal falls back to the CPU Fit, which needs no GPU scratch at all.
        const DERIVE_SCRATCH_MAX: u64 = 256 * 1024 * 1024;
        let src_mip_h = (th >> level).max(1);
        if dw as u64 * src_mip_h as u64 * 8 > DERIVE_SCRATCH_MAX {
            return None;
        }
        let srgb_in = src.mode == 0.0;
        let content_hdr = src.content_hdr;
        let peak = src.peak;
        let out = derive_fit_texture(
            &self.device,
            &self.queue,
            &self.derive,
            &src.texture,
            level,
            srgb_in,
            dw,
            dh,
            kernel,
        );
        let view = out.create_view(&wgpu::TextureViewDescriptor::default());
        let mode = if srgb_in { 0.0 } else { 2.0 };
        // §3c: the Fit's scene scale keys off the CONTENT dynamic range, not the storage
        // format — an fp16 Fit of SDR content on an HDR surface would still need the
        // SDR-white scale. (Today mode 2 ⟺ HDR content; this is the general form.)
        let scale = self.scene_scale(content_hdr);
        let bind_group = image_bind_group(
            &self.device,
            &self.bgl,
            &view,
            &ColorTransform::srgb(),
            mode,
            scale,
        );
        let bytes = dw as u64 * dh as u64 * if srgb_in { 4 } else { 8 };
        self.ring[dst_slot] = Some(RingSlot {
            bind_group,
            w: dw,
            h: dh,
            peak,
            texture: out,
            was_clamped: false,
            mode,
            content_hdr,
        });
        Some(DerivedFit {
            w: dw,
            h: dh,
            bytes,
        })
    }

    fn present_slot(&mut self, slot: usize) -> bool {
        let Some((w, h, peak)) = self
            .ring
            .get(slot)
            .and_then(|s| s.as_ref())
            .map(|s| (s.w, s.h, s.peak))
        else {
            return false; // unknown / not-yet-uploaded slot: keep the current frame (and its hold)
        };
        self.blank = false; // a photo is showing again
        self.message = None; // hide the empty-state hint
        self.held = None; // a real frame supersedes the held one — free its texture
        self.set_present_peak(peak);
        self.present_idx = Some(slot);
        self.img_w = w;
        self.img_h = h;
        self.queue.write_buffer(
            &self.vbuf,
            0,
            bytemuck::cast_slice(&quad_vertices(
                &self.view,
                w,
                h,
                self.config.width,
                self.config.height,
                self.content_top_inset,
            )),
        );
        true
    }

    fn render(&mut self) -> Result<bool, RenderError> {
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            // Dropped frames (`Ok(false)`) are NOT silent successes: nothing reached
            // the screen, and the caller must retry or the compositor keeps the stale
            // frame up indefinitely. The eprintln is deliberate — this is rare, and
            // its absence from a trace cost a debugging session (2026-07-04).
            //
            // `Lost` vs `Outdated` are logged separately (they mean different things — a
            // genuinely lost surface may need *recreation*, not just reconfigure), and the log
            // now reports what `reconfigure_surface` ACTUALLY did (it can bail without
            // reconfiguring when the surface reports no usable caps) instead of always claiming
            // "reconfigured". `reconfigure_surface` re-applies at the CURRENT `config` size; if
            // that size has drifted from the window, the shell's `heal_surface_if_dropped`
            // re-asserts the true size. See the surface-present-bug instrument bundle.
            Err(err @ (wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated)) => {
                let backend = self.adapter.get_info().backend;
                let (w, h, mode) = (
                    self.config.width,
                    self.config.height,
                    self.config.present_mode,
                );
                let reconfigured = self.reconfigure_surface();
                eprintln!(
                    "render: surface {err:?} — backend={backend:?} config={w}x{h} present_mode={mode:?} — {}, frame dropped",
                    if reconfigured { "reconfigured" } else { "NOT reconfigured (surface reported no usable caps)" },
                );
                return Ok(false);
            }
            Err(wgpu::SurfaceError::Timeout) => {
                let backend = self.adapter.get_info().backend;
                eprintln!(
                    "render: drawable timeout — backend={backend:?} config={}x{} present_mode={:?} — frame dropped",
                    self.config.width, self.config.height, self.config.present_mode
                );
                return Ok(false);
            }
            Err(wgpu::SurfaceError::OutOfMemory) => return Err(RenderError::OutOfMemory),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let intermediate_view = self
            .intermediate
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("present"),
            });
        // Pass 1: scene → fp16 intermediate. The selected resident-ring slot if one
        // is presented, else the single image — a keypress rebinds via `present_idx`,
        // no upload here. When blank (no image loaded), clear to the letterbox
        // background and draw no photo quad — a plain, image-free screen.
        if self.blank {
            clear_scene(
                &mut encoder,
                &intermediate_view,
                letterbox_linear(self.letterbox),
            );
        } else {
            // Ring slots are always RGBA; only the single-image bind group can hold
            // an NV12 two-plane frame (task 79.10) — pick its pipeline to match, or
            // the bind-group layout mismatch is a validation error.
            let single = || match self.scene_kind {
                SceneKind::Planar => (&self.scene_planar_pipeline, &self.bind_group),
                SceneKind::Rgba => (&self.scene_pipeline, &self.bind_group),
            };
            let ring_slot = self
                .present_idx
                .and_then(|i| self.ring.get(i))
                .and_then(|s| s.as_ref());
            // `blank` is already handled by the outer branch, so it's false here; the
            // held-frame preference keeps a geometry change from flashing a stale single
            // image (task #18 finding #5). See `choose_draw_source`.
            let source = choose_draw_source(false, ring_slot.is_some(), self.held.is_some());
            // PB_DOOR_DIAG: what the quad actually draws this frame (dev-only; zero cost when
            // off). Pairs with the core's `[door-diag] draw` line to tell "card over a stale
            // photo" (quad = Held/Single while the core says door presented) from an overlay
            // problem — so the next occurrence is diagnosable, not guessed.
            if door_diag() {
                eprintln!(
                    "[door-diag] render source={source:?} present_idx={:?} ring_live={} held={} blank={} backend={:?} present_mode={:?} img={}x{}",
                    self.present_idx,
                    ring_slot.is_some(),
                    self.held.is_some(),
                    self.blank,
                    self.adapter.get_info().backend,
                    self.config.present_mode,
                    self.img_w,
                    self.img_h,
                );
            }
            let (pipeline, bind_group) = match source {
                DrawSource::RingSlot => (&self.scene_pipeline, &ring_slot.unwrap().bind_group),
                DrawSource::Held => (
                    &self.scene_pipeline,
                    &self.held.as_ref().unwrap().bind_group,
                ),
                DrawSource::Single | DrawSource::Blank => single(),
            };
            draw_scene(
                &mut encoder,
                &intermediate_view,
                pipeline,
                bind_group,
                &self.vbuf,
                &self.ibuf,
                letterbox_linear(self.letterbox),
            );
        }
        // Pass 1b: the subtitle cue block — **first** of the overlays, so every piece of
        // chrome composites above it. A subtitle belongs to the picture; a toast is the app
        // talking over it, and the toast that says "Subtitles: English" appears in the same
        // bottom-center strip as the cue it is talking about. Its own premultiplied blend
        // (see `Pipelines::subtitle`).
        if let Some(s) = &self.subtitle {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("subtitle"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.subtitle_pipeline);
            rp.set_bind_group(0, &s.bind_group, &[]);
            rp.set_vertex_buffer(0, s.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        // Pass 2: alpha-blend the info panel into the intermediate (in linear), so
        // the single present pass below serves both the SDR and HDR output paths.
        if let Some(ov) = &self.overlay {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.overlay_pipeline);
            rp.set_bind_group(0, &ov.bind_group, &[]);
            rp.set_vertex_buffer(0, ov.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        // Pass 2a: the basic info line (bottom-right), its own layer so it sits
        // alongside the rich panel (which lifts above it) rather than replacing it.
        if let Some(l) = &self.info_line {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("info-line"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.overlay_pipeline);
            rp.set_bind_group(0, &l.bind_group, &[]);
            rp.set_vertex_buffer(0, l.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        // Pass 2a′: the folder-tree panel (top-left corner), its own layer beside
        // the info panel.
        if let Some(t) = &self.tree {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tree"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.overlay_pipeline);
            rp.set_bind_group(0, &t.bind_group, &[]);
            rp.set_vertex_buffer(0, t.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        // Pass 2b: the transient status toast (bottom-center), composited on top of
        // the photo and the info panel.
        if let Some(t) = &self.toast {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("toast"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.overlay_pipeline);
            rp.set_bind_group(0, &t.bind_group, &[]);
            rp.set_vertex_buffer(0, t.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        // Pass 2c: the top-right "loading" pie, composited above everything else so
        // it stays visible while the next photo decodes.
        if let Some(p) = &self.pie {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pie"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.overlay_pipeline);
            rp.set_bind_group(0, &p.bind_group, &[]);
            rp.set_vertex_buffer(0, p.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        // Pass 2c′: the top-right scan-count chip, just below the pie.
        if let Some(c) = &self.chip {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("chip"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.overlay_pipeline);
            rp.set_bind_group(0, &c.bind_group, &[]);
            rp.set_vertex_buffer(0, c.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        // Pass 2d: the centered empty-state message ("Press O to open…"), composited
        // over the blank background.
        if let Some(m) = &self.message {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("message"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.overlay_pipeline);
            rp.set_bind_group(0, &m.bind_group, &[]);
            rp.set_vertex_buffer(0, m.vbuf.slice(..));
            rp.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        // Pass 2e: the egui rich-panel overlay (Inspector / Help / folder tree), drawn
        // last so it sits above every CPU layer. The shell-owned offscreen texture is
        // already premultiplied; the bufferless fullscreen triangle stretches it 1:1
        // over the viewport and `fs_egui` composites it with premultiplied blend.
        if let Some(bg) = &self.egui {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui-overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &intermediate_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.egui_pipeline);
            rp.set_bind_group(0, bg, &[]);
            rp.draw(0..3, 0..1);
        }
        // Pass 3: present the intermediate onto the surface (SDR tone-map+encode, or
        // HDR scRGB scale, per the present uniform).
        draw_tonemap(
            &mut encoder,
            &view,
            &self.tonemap_pipeline,
            &self.tonemap_bind_group,
        );
        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        Ok(true)
    }
}

/// Render the scene to an RGBA8 buffer (`screen_w * screen_h * 4`) with no
/// window. Backs the golden-image tests. Uses `Rgba8Unorm` so texels are exact.
pub fn render_offscreen(
    image: &[u8],
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
) -> Vec<u8> {
    render_offscreen_color(
        image,
        img_w,
        img_h,
        screen_w,
        screen_h,
        ColorTransform::srgb(),
    )
}

/// Like [`render_offscreen`] but applies `color` (the in-shader source→sRGB
/// conversion). Backs the color-management golden tests.
pub fn render_offscreen_color(
    image: &[u8],
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
    color: ColorTransform,
) -> Vec<u8> {
    pollster::block_on(render_offscreen_async(
        OffscreenSource::Rgba(image),
        img_w,
        img_h,
        screen_w,
        screen_h,
        color,
    ))
}

/// Headless render of one NV12 frame through the real two-plane scene pipeline
/// (`fs_scene_planar`) + present pass — the task 79.10 golden harness, compared
/// against [`crate::yuv::nv12_to_rgba`] (the CPU reference) in tests.
pub fn render_offscreen_nv12(
    y: &[u8],
    uv: &[u8],
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
    params: crate::YuvParams,
) -> Vec<u8> {
    pollster::block_on(render_offscreen_async(
        OffscreenSource::Planar {
            y,
            uv,
            present: crate::PlanarPresentation {
                format: crate::PlanarFormat::Nv12,
                transfer: crate::PlanarTransfer::SrgbLike,
                yuv: params,
                color: ColorTransform::srgb(),
                peak: 1.0,
            },
        },
        img_w,
        img_h,
        screen_w,
        screen_h,
        ColorTransform::srgb(),
    ))
}

/// What [`render_offscreen_async`] draws: an RGBA8 image or a planar plane pair.
enum OffscreenSource<'a> {
    Rgba(&'a [u8]),
    Planar {
        y: &'a [u8],
        uv: &'a [u8],
        present: crate::PlanarPresentation,
    },
}

async fn render_offscreen_async(
    source: OffscreenSource<'_>,
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
    color: ColorTransform,
) -> Vec<u8> {
    let instance = instance();
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        })
        .await
        .expect("no GPU adapter");
    let (device, queue, _p010) = request_device_p010(&adapter).await;

    let format = wgpu::TextureFormat::Rgba8Unorm;
    let pipelines = build_pipelines(&device, format);
    let mut upload = StagingUpload::new();
    let (scene_pipeline, bind_group) = match source {
        OffscreenSource::Rgba(image) => (
            &pipelines.scene,
            upload_image(
                &device,
                &queue,
                &pipelines.scene_bgl,
                &mut upload,
                image,
                img_w,
                img_h,
                &color,
                false,
                1.0,
                None, // offscreen: default non-mipped; a mipped variant is a test opt-in
            ),
        ),
        OffscreenSource::Planar { y, uv, present } => {
            let mut slot = None;
            let ReuseOutcome::Rebuilt(bg) = upload_planar_reusable(
                &device,
                &queue,
                &pipelines.planar_bgl,
                &mut upload,
                &mut slot,
                y,
                uv,
                img_w,
                img_h,
                &present.color,
                present.yuv,
                present.format,
                planar_mode(present.transfer),
                1.0,
            ) else {
                unreachable!("a fresh slot always rebuilds");
            };
            (&pipelines.scene_planar, bg)
        }
    };
    let vbuf = vertex_buffer(
        &device,
        &ViewTransform::default(),
        img_w,
        img_h,
        screen_w,
        screen_h,
    );
    let ibuf = index_buffer(&device);

    // Same scene → fp16 intermediate → present path as the on-screen renderer, so
    // the golden tests exercise the real pipeline. SDR peak = 1.0 (identity tone-map).
    let peak_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("present-uniform"),
        contents: bytemuck::bytes_of(&PresentUniform::new(1.0, false, 0.0)),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let (intermediate, tonemap_bind_group) = make_intermediate(
        &device,
        &pipelines.tonemap_bgl,
        &peak_buf,
        screen_w,
        screen_h,
    );
    let intermediate_view = intermediate.create_view(&wgpu::TextureViewDescriptor::default());

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen"),
        size: wgpu::Extent3d {
            width: screen_w,
            height: screen_h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());

    // Readback buffer: bytes_per_row must be 256-aligned.
    let unpadded = screen_w * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * screen_h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("offscreen"),
    });
    draw_scene(
        &mut encoder,
        &intermediate_view,
        scene_pipeline,
        &bind_group,
        &vbuf,
        &ibuf,
        letterbox_linear([LETTERBOX[0], LETTERBOX[1], LETTERBOX[2]]),
    );
    draw_tonemap(&mut encoder, &view, &pipelines.tonemap, &tonemap_bind_group);
    encoder.copy_texture_to_buffer(
        wgpu::ImageCopyTexture {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::ImageCopyBuffer {
            buffer: &readback,
            layout: wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(screen_h),
            },
        },
        wgpu::Extent3d {
            width: screen_w,
            height: screen_h,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(std::iter::once(encoder.finish()));

    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device.poll(wgpu::Maintain::Wait);
    rx.recv().expect("map channel").expect("map readback");

    let mapped = slice.get_mapped_range();
    let mut out = Vec::with_capacity((screen_w * screen_h * 4) as usize);
    for row in 0..screen_h {
        let start = (row * padded) as usize;
        out.extend_from_slice(&mapped[start..start + unpadded as usize]);
    }
    drop(mapped);
    readback.unmap();
    out
}

/// Headless render of one planar frame through `fs_scene_planar` into the fp16
/// **scene intermediate**, returned as scene-linear scRGB `[f32;4]` per pixel —
/// read back *before* the tone-map so HDR values > 1.0 and wide-gamut negatives
/// are visible (task #91 Phase 2 golden; Codex: RGBA8 can't prove HDR). `None`
/// when the adapter lacks `TEXTURE_FORMAT_16BIT_NORM` and the frame is P010 (the
/// test then asserts the CPU fallback instead). `screen_*` should equal `img_*`
/// so the quad fills the target and every pixel is the converted image.
pub fn render_offscreen_planar_scene(
    y: &[u8],
    uv: &[u8],
    present: crate::PlanarPresentation,
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
) -> Option<Vec<[f32; 4]>> {
    pollster::block_on(render_offscreen_planar_scene_async(
        y, uv, present, img_w, img_h, screen_w, screen_h,
    ))
}

async fn render_offscreen_planar_scene_async(
    y: &[u8],
    uv: &[u8],
    present: crate::PlanarPresentation,
    img_w: u32,
    img_h: u32,
    screen_w: u32,
    screen_h: u32,
) -> Option<Vec<[f32; 4]>> {
    let instance = instance();
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        })
        .await
        .expect("no GPU adapter");
    let (device, queue, p010) = request_device_p010(&adapter).await;
    if present.format.is_ten_bit() && !p010 {
        return None; // adapter can't do R16Unorm — the caller checks the CPU path
    }

    let pipelines = build_pipelines(&device, wgpu::TextureFormat::Rgba8Unorm);
    let mut upload = StagingUpload::new();
    let mut slot = None;
    let ReuseOutcome::Rebuilt(bind_group) = upload_planar_reusable(
        &device,
        &queue,
        &pipelines.planar_bgl,
        &mut upload,
        &mut slot,
        y,
        uv,
        img_w,
        img_h,
        &present.color,
        present.yuv,
        present.format,
        planar_mode(present.transfer),
        1.0,
    ) else {
        unreachable!("a fresh slot always rebuilds");
    };
    let vbuf = vertex_buffer(
        &device,
        &ViewTransform::default(),
        img_w,
        img_h,
        screen_w,
        screen_h,
    );
    let ibuf = index_buffer(&device);

    // The fp16 scene intermediate, this time with COPY_SRC so we can read it back
    // before the tone-map pass runs.
    let intermediate = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("scene-intermediate-readback"),
        size: wgpu::Extent3d {
            width: screen_w,
            height: screen_h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: INTERMEDIATE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let intermediate_view = intermediate.create_view(&wgpu::TextureViewDescriptor::default());

    let bpp = 8u32; // Rgba16Float
    let unpadded = screen_w * bpp;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("scene-readback"),
        size: (padded * screen_h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("offscreen-scene"),
    });
    draw_scene(
        &mut encoder,
        &intermediate_view,
        &pipelines.scene_planar,
        &bind_group,
        &vbuf,
        &ibuf,
        letterbox_linear([LETTERBOX[0], LETTERBOX[1], LETTERBOX[2]]),
    );
    encoder.copy_texture_to_buffer(
        wgpu::ImageCopyTexture {
            texture: &intermediate,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::ImageCopyBuffer {
            buffer: &readback,
            layout: wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(screen_h),
            },
        },
        wgpu::Extent3d {
            width: screen_w,
            height: screen_h,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(std::iter::once(encoder.finish()));

    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device.poll(wgpu::Maintain::Wait);
    rx.recv().expect("map channel").expect("map readback");

    let mapped = slice.get_mapped_range();
    let mut out = Vec::with_capacity((screen_w * screen_h) as usize);
    for row in 0..screen_h {
        let start = (row * padded) as usize;
        for col in 0..screen_w as usize {
            let px = start + col * 8;
            let ch =
                |o: usize| half::f16::from_le_bytes([mapped[px + o], mapped[px + o + 1]]).to_f32();
            out.push([ch(0), ch(2), ch(4), ch(6)]);
        }
    }
    drop(mapped);
    readback.unmap();
    Some(out)
}

/// A deterministic test/placeholder image: gray field, colored corner markers
/// (TL red, TR green, BL blue, BR yellow), and a white center block. Makes
/// letterboxing and orientation obvious on screen and exact in tests.
pub fn test_pattern(w: u32, h: u32) -> Vec<u8> {
    let mut px = vec![0u8; (w as usize) * (h as usize) * 4];
    let put = |px: &mut [u8], x: u32, y: u32, c: [u8; 4]| {
        let i = ((y * w + x) * 4) as usize;
        px[i..i + 4].copy_from_slice(&c);
    };
    let bg = [40, 40, 40, 255];
    for y in 0..h {
        for x in 0..w {
            put(&mut px, x, y, bg);
        }
    }
    let m = 120.min(w / 2).min(h / 2);
    let tl = [220, 30, 30, 255];
    let tr = [30, 200, 30, 255];
    let bl = [40, 80, 230, 255];
    let br = [230, 210, 30, 255];
    for y in 0..m {
        for x in 0..m {
            put(&mut px, x, y, tl);
            put(&mut px, w - 1 - x, y, tr);
            put(&mut px, x, h - 1 - y, bl);
            put(&mut px, w - 1 - x, h - 1 - y, br);
        }
    }
    let cw = (w / 4).max(2);
    let ch = (h / 4).max(2);
    let cx0 = w / 2 - cw / 2;
    let cy0 = h / 2 - ch / 2;
    for y in cy0..cy0 + ch {
        for x in cx0..cx0 + cw {
            put(&mut px, x, y, [255, 255, 255, 255]);
        }
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(buf: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn mip_levels_is_exact_on_odd_sizes() {
        assert_eq!(mip_levels(1, 1), 1);
        assert_eq!(mip_levels(2, 1), 2);
        assert_eq!(mip_levels(3, 3), 2); // 3→1
        assert_eq!(mip_levels(4, 4), 3); // 4→2→1
        assert_eq!(mip_levels(5, 2), 3); // 5→2→1
        assert_eq!(mip_levels(6000, 4000), 13); // 24 MP photo
    }

    /// The mip-gen fragment must average in **linear light**, not in encoded (sRGB) space — the
    /// correctness Codex insisted on. A 2×2 checkerboard of pure black + white sRGB texels has a
    /// LINEAR average of 0.5 (→ sRGB ≈ 0.735 ≈ 188), whereas a naïve encoded average is ~128. The
    /// gap is exactly what proves the linearize→average→re-encode path.
    #[test]
    fn mip_downsample_averages_in_linear_light() {
        let px = pollster::block_on(gen_mip_1x1_srgb([
            [0, 0, 0, 255],
            [255, 255, 255, 255],
            [255, 255, 255, 255],
            [0, 0, 0, 255],
        ]));
        assert!(
            (180..=192).contains(&px[0]),
            "linear-light average should be ~188, got {} (naïve encoded avg would be ~128)",
            px[0]
        );
        assert_eq!(px[3], 255, "opaque alpha preserved");
    }

    /// #110 plan §3b odd-dim caveat, pinned: `MIPGEN_WGSL`'s 2×2 box **drops** the trailing
    /// row/column of an odd-dimension level — it does NOT clamp-and-under-weight it (the comment
    /// used to claim that). A 5×3 L0 that is black everywhere except a white last column AND a
    /// white last row must produce a pure-black 2×1 mip 1: the box for dst (1,0) reads source
    /// columns 2–3 / rows 0–1 and never touches col 4 / row 2. If a future polyphase box starts
    /// weighting the orphans in (the correct NPOT refinement), this test's expectation flips —
    /// update the derive's mip-phase assumptions with it.
    #[test]
    fn odd_dims_drop_the_trailing_row_and_col() {
        const W: u32 = 5;
        const H: u32 = 3;
        let mut texels = vec![[0u8, 0, 0, 255]; (W * H) as usize];
        for y in 0..H {
            texels[(y * W + (W - 1)) as usize] = [255, 255, 255, 255]; // last column white
        }
        for x in 0..W {
            texels[((H - 1) * W + x) as usize] = [255, 255, 255, 255]; // last row white
        }
        let m1 = pollster::block_on(gen_mip1_srgb(W, H, &texels)); // mip 1 = 2×1
        assert_eq!(
            at(&m1, 2, 0, 0),
            [0, 0, 0, 255],
            "dst (0,0) averages src cols 0-1 / rows 0-1 — all black"
        );
        assert_eq!(
            at(&m1, 2, 1, 0),
            [0, 0, 0, 255],
            "dst (1,0) averages src cols 2-3 / rows 0-1: the white col 4 and row 2 are DROPPED \
             (any non-black here would mean the box now reaches the odd trailing texels)"
        );
    }

    // ---- #110 Phase 110a: the two-pass Lanczos derive (DERIVE_WGSL) ----

    /// `contain_dims` must reproduce `pb_decode::common::downscale_to_fit`'s sizing exactly —
    /// same scale rule, same 0.999 identity band, same rounding — or the derived Fit and the
    /// CPU Fit disagree on geometry.
    #[test]
    fn contain_dims_matches_the_cpu_decode_rule() {
        assert_eq!(contain_dims(6000, 4000, 1440, 2036), (1440, 960));
        assert_eq!(contain_dims(4000, 6000, 1440, 2036), (1357, 2036));
        assert_eq!(
            contain_dims(100, 100, 200, 200),
            (100, 100),
            "never upscale"
        );
        assert_eq!(
            contain_dims(1000, 1000, 999, 2000),
            (1000, 1000),
            "the >= 0.999 near-identity band returns the source unchanged (CPU parity)"
        );
    }

    /// Mip selection: the last (coarsest) level still >= the target on both axes, biased by
    /// `mip_bias` (−1 = one finer), clamped to the real chain.
    #[test]
    fn select_derive_mip_picks_the_last_level_at_or_above_target() {
        // 6000×4000: L1 = 3000×2000, L2 = 1500×1000 (≥ 1440×960), L3 = 750×500 (<) → L2.
        assert_eq!(select_derive_mip(6000, 4000, 13, 1440, 960, 0), 2);
        assert_eq!(
            select_derive_mip(6000, 4000, 13, 1440, 960, -1),
            1,
            "bias −1 = one finer"
        );
        assert_eq!(
            select_derive_mip(6000, 4000, 13, 6000, 4000, 0),
            0,
            "identity target → L0"
        );
        assert_eq!(
            select_derive_mip(6000, 4000, 13, 6000, 4000, -1),
            0,
            "bias clamps at L0"
        );
        assert_eq!(
            select_derive_mip(8, 8, 4, 1, 1, 0),
            3,
            "tiny target reaches chain end"
        );
        assert_eq!(
            select_derive_mip(8, 8, 4, 1, 1, 5),
            3,
            "bias clamps at the chain end"
        );
    }

    fn ref_s2l(x: f32) -> f32 {
        if x <= 0.04045 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        }
    }
    fn ref_l2s(x: f32) -> f32 {
        let c = x.clamp(0.0, 1.0);
        if c <= 0.0031308 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    }

    /// Pure-CPU reference of the exact mode-0 derive chain (linearize → premultiply → H → V →
    /// un-premultiply → encode), sharing `lanczos_axis_kernel` — so the GPU tests isolate the
    /// SHADER's application of the weights + colour chain (the kernel itself has exact CPU tests
    /// in `resample.rs`).
    fn cpu_derive_mode0(
        src: &[[u8; 4]],
        sw: u32,
        sh: u32,
        dw: u32,
        dh: u32,
        a: u32,
    ) -> Vec<[u8; 4]> {
        let kh = crate::resample::lanczos_axis_kernel(sw, dw, a);
        let kv = crate::resample::lanczos_axis_kernel(sh, dh, a);
        let lin: Vec<[f32; 4]> = src
            .iter()
            .map(|p| {
                let al = p[3] as f32 / 255.0;
                [
                    ref_s2l(p[0] as f32 / 255.0) * al,
                    ref_s2l(p[1] as f32 / 255.0) * al,
                    ref_s2l(p[2] as f32 / 255.0) * al,
                    al,
                ]
            })
            .collect();
        let mut mid = vec![[0f32; 4]; (dw * sh) as usize];
        for y in 0..sh {
            for x in 0..dw {
                let mut acc = [0f32; 4];
                for t in 0..kh.taps {
                    let cx = (kh.starts[x as usize] + t as i32).clamp(0, sw as i32 - 1) as u32;
                    let w = kh.weights[(x * kh.taps + t) as usize];
                    let s = lin[(y * sw + cx) as usize];
                    for c in 0..4 {
                        acc[c] += s[c] * w;
                    }
                }
                mid[(y * dw + x) as usize] = acc;
            }
        }
        let mut out = vec![[0u8; 4]; (dw * dh) as usize];
        for y in 0..dh {
            for x in 0..dw {
                let mut acc = [0f32; 4];
                for t in 0..kv.taps {
                    let cy = (kv.starts[y as usize] + t as i32).clamp(0, sh as i32 - 1) as u32;
                    let w = kv.weights[(y * kv.taps + t) as usize];
                    let s = mid[(cy * dw + x) as usize];
                    for c in 0..4 {
                        acc[c] += s[c] * w;
                    }
                }
                // Divide by the UNCLAMPED filtered alpha (the shader's rule): premult RGB and
                // alpha overshoot an alpha step by the same factor, so the true divisor
                // recovers the exact straight colour. Only the stored alpha clamps.
                let rgb = if acc[3] > 1e-4 {
                    [acc[0] / acc[3], acc[1] / acc[3], acc[2] / acc[3]]
                } else {
                    [0.0; 3]
                };
                let al = acc[3].clamp(0.0, 1.0);
                out[(y * dw + x) as usize] = [
                    (ref_l2s(rgb[0]) * 255.0).round() as u8,
                    (ref_l2s(rgb[1]) * 255.0).round() as u8,
                    (ref_l2s(rgb[2]) * 255.0).round() as u8,
                    (al * 255.0).round() as u8,
                ];
            }
        }
        out
    }

    /// Read `tex` (single level `w`×`h`, `bpp` bytes/texel) back to tightly-packed rows.
    async fn read_texture(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        tex: &wgpu::Texture,
        w: u32,
        h: u32,
        bpp: u32,
    ) -> Vec<u8> {
        let padded = (w * bpp).div_ceil(256) * 256;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("derive-readback"),
            size: (padded * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &readback,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(std::iter::once(encoder.finish()));
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.poll(wgpu::Maintain::Wait);
        rx.recv().expect("map channel").expect("map readback");
        let mapped = slice.get_mapped_range();
        let mut out = Vec::with_capacity((w * bpp * h) as usize);
        for row in 0..h {
            let s = (row * padded) as usize;
            out.extend_from_slice(&mapped[s..s + (w * bpp) as usize]);
        }
        drop(mapped);
        readback.unmap();
        out
    }

    /// Upload an RGBA8 source, run the mode-0 derive from `src_mip`, read back the RGBA8 final.
    async fn run_derive_mode0(
        src: &[[u8; 4]],
        sw: u32,
        sh: u32,
        dw: u32,
        dh: u32,
        a: u32,
    ) -> Vec<[u8; 4]> {
        let instance = instance();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
            })
            .await
            .expect("no GPU adapter");
        let (device, queue, _p010) = request_device_p010(&adapter).await;
        let derive = build_derive(&device);
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("derive-test-src"),
            size: wgpu::Extent3d {
                width: sw,
                height: sh,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let bytes: Vec<u8> = src.iter().flatten().copied().collect();
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &bytes,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(sw * 4),
                rows_per_image: Some(sh),
            },
            wgpu::Extent3d {
                width: sw,
                height: sh,
                depth_or_array_layers: 1,
            },
        );
        let out = derive_fit_texture(&device, &queue, &derive, &tex, 0, true, dw, dh, a);
        let raw = read_texture(&device, &queue, &out, dw, dh, 4).await;
        raw.chunks_exact(4)
            .map(|c| [c[0], c[1], c[2], c[3]])
            .collect()
    }

    /// Deterministic varied test pattern (colors + alpha classes incl. fully transparent).
    fn derive_test_pattern(n: usize) -> Vec<[u8; 4]> {
        let mut seed = 0x1234_5678u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as u8
        };
        (0..n)
            .map(|i| {
                let a = match i % 7 {
                    0 => 0,   // fully transparent (its RGB must never bleed)
                    3 => 96,  // partial
                    5 => 224, // near-opaque
                    _ => 255,
                };
                [next(), next(), next(), a]
            })
            .collect()
    }

    /// The GPU derive must reproduce the CPU reference chain within quantization tolerance
    /// (fp16 intermediate + u8 encode). RGB is compared only where alpha is non-negligible —
    /// straight RGB under near-zero alpha is numerically unstable (ε-branch) and visually
    /// meaningless; alpha itself is always compared.
    #[test]
    fn derive_matches_the_cpu_reference_mode0() {
        let (sw, sh, dw, dh, a) = (16u32, 12u32, 7u32, 5u32, 3u32);
        let src = derive_test_pattern((sw * sh) as usize);
        let gpu = pollster::block_on(run_derive_mode0(&src, sw, sh, dw, dh, a));
        let cpu = cpu_derive_mode0(&src, sw, sh, dw, dh, a);
        for (i, (g, c)) in gpu.iter().zip(cpu.iter()).enumerate() {
            assert!(
                (g[3] as i32 - c[3] as i32).abs() <= 2,
                "texel {i} alpha: gpu {} vs cpu {}",
                g[3],
                c[3]
            );
            if g[3] >= 8 && c[3] >= 8 {
                for ch in 0..3 {
                    assert!(
                        (g[ch] as i32 - c[ch] as i32).abs() <= 3,
                        "texel {i} ch {ch}: gpu {:?} vs cpu {:?}",
                        g,
                        c
                    );
                }
            }
        }
    }

    /// At 1:1 the derive is (near-)lossless: the kernel is an impulse, so the only error is the
    /// EOTF→fp16→OETF round-trip — within ±1/255 per channel.
    #[test]
    fn derive_identity_is_lossless_mode0() {
        let (sw, sh) = (9u32, 7u32);
        let src: Vec<[u8; 4]> = (0..(sw * sh) as usize)
            .map(|i| {
                [
                    (i * 7 % 256) as u8,
                    (i * 29 % 256) as u8,
                    (i * 53 % 256) as u8,
                    255,
                ]
            })
            .collect();
        let gpu = pollster::block_on(run_derive_mode0(&src, sw, sh, sw, sh, 3));
        for (i, (g, s)) in gpu.iter().zip(src.iter()).enumerate() {
            for ch in 0..4 {
                assert!(
                    (g[ch] as i32 - s[ch] as i32).abs() <= 1,
                    "texel {i} ch {ch}: got {:?} want {:?}",
                    g,
                    s
                );
            }
        }
    }

    /// Premultiplied filtering: a fully transparent RED region next to opaque GREEN must
    /// contribute NO red to the downscale (its RGB rides in as rgb·α = 0). Any red in the output
    /// means the shader filtered straight alpha — the fringe bug the §3b chain exists to prevent.
    #[test]
    fn derive_never_bleeds_transparent_color() {
        let (sw, sh) = (8u32, 8u32);
        let src: Vec<[u8; 4]> = (0..(sw * sh))
            .map(|i| {
                let x = i % sw;
                if x < sw / 2 {
                    [0, 255, 0, 255] // opaque green
                } else {
                    [255, 0, 0, 0] // fully transparent red
                }
            })
            .collect();
        let gpu = pollster::block_on(run_derive_mode0(&src, sw, sh, 4, 4, 3));
        for (i, g) in gpu.iter().enumerate() {
            assert!(
                g[0] <= 2,
                "texel {i} has red {} — transparent texels' RGB leaked into the filter (straight-alpha bug)",
                g[0]
            );
        }
    }

    /// Codex P1 regression: at an opaque/transparent ALPHA STEP the filtered alpha overshoots
    /// past 1.0, and premultiplied RGB overshoots by the same factor — so un-premultiplying by
    /// the UNCLAMPED filtered alpha must recover the exact uniform colour. The clamped-divisor
    /// bug brightens texels near the step (÷1.0 instead of ÷~1.08 ≈ +5 sRGB code values on
    /// mid-gray). Uniform mid-gray with an alpha step: every visible output texel must still be
    /// exactly mid-gray.
    #[test]
    fn derive_keeps_straight_color_constant_across_an_alpha_step() {
        let (sw, sh) = (16u32, 4u32);
        let src: Vec<[u8; 4]> = (0..(sw * sh))
            .map(|i| {
                let x = i % sw;
                [128, 128, 128, if x < sw / 2 { 255 } else { 0 }]
            })
            .collect();
        let gpu = pollster::block_on(run_derive_mode0(&src, sw, sh, 7, 4, 3));
        for (i, g) in gpu.iter().enumerate() {
            if g[3] < 8 {
                continue; // straight RGB under near-zero alpha is meaningless
            }
            for (ch, &v) in g.iter().take(3).enumerate() {
                assert!(
                    (v as i32 - 128).abs() <= 2,
                    "texel {i} ch {ch} = {v} (want 128): straight colour drifted across the \
                     alpha step — the un-premultiply divisor is wrong"
                );
            }
        }
    }

    /// Codex P2 regression: a non-finite texel in an fp16 source (a broken/hostile HDR decode)
    /// must not spread — sanitize-on-load pins it to fp16's finite range, so every output stays
    /// finite (an Inf riding a negative Lanczos lobe would otherwise turn neighbours NaN).
    #[test]
    fn derive_contains_a_nonfinite_hdr_texel() {
        use half::f16;
        let (sw, sh) = (8u32, 4u32);
        let mut src: Vec<f16> = (0..(sw * sh) as usize)
            .flat_map(|_| [f16::from_f32(0.5); 4])
            .collect();
        src[(sw as usize + 4) * 4] = f16::INFINITY; // one Inf red channel mid-image (row 1, col 4)
        let out = pollster::block_on(async {
            let instance = instance();
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    force_fallback_adapter: false,
                    compatible_surface: None,
                })
                .await
                .expect("no GPU adapter");
            let (device, queue, _p010) = request_device_p010(&adapter).await;
            let derive = build_derive(&device);
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("derive-inf-src"),
                size: wgpu::Extent3d {
                    width: sw,
                    height: sh,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::cast_slice(&src.iter().map(|h| h.to_bits()).collect::<Vec<u16>>()),
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(sw * 8),
                    rows_per_image: Some(sh),
                },
                wgpu::Extent3d {
                    width: sw,
                    height: sh,
                    depth_or_array_layers: 1,
                },
            );
            let out = derive_fit_texture(&device, &queue, &derive, &tex, 0, false, 4, 2, 3);
            read_texture(&device, &queue, &out, 4, 2, 8).await
        });
        for (i, c) in out.chunks_exact(2).enumerate() {
            let v = f16::from_bits(u16::from_le_bytes([c[0], c[1]])).to_f32();
            assert!(
                v.is_finite(),
                "component {i} is {v} — a non-finite source texel leaked through the derive"
            );
        }
    }

    /// Mode-2 (fp16 scene-linear) identity: HDR values above 1.0 and exact fractions survive the
    /// derive unclamped and unencoded — the linear final stores straight scene-linear fp16.
    #[test]
    fn derive_mode2_identity_preserves_hdr_values() {
        use half::f16;
        let (sw, sh) = (6u32, 4u32);
        let vals = [0.25f32, 0.5, 1.0, 2.0, 4.0, 0.75];
        let src: Vec<f16> = (0..(sw * sh) as usize)
            .flat_map(|i| {
                let v = vals[i % vals.len()];
                [
                    f16::from_f32(v),
                    f16::from_f32(v * 0.5),
                    f16::from_f32(v * 0.25),
                    f16::ONE,
                ]
            })
            .collect();
        let out = pollster::block_on(async {
            let instance = instance();
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    force_fallback_adapter: false,
                    compatible_surface: None,
                })
                .await
                .expect("no GPU adapter");
            let (device, queue, _p010) = request_device_p010(&adapter).await;
            let derive = build_derive(&device);
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("derive-test-src-f16"),
                size: wgpu::Extent3d {
                    width: sw,
                    height: sh,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::cast_slice(&src.iter().map(|h| h.to_bits()).collect::<Vec<u16>>()),
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(sw * 8),
                    rows_per_image: Some(sh),
                },
                wgpu::Extent3d {
                    width: sw,
                    height: sh,
                    depth_or_array_layers: 1,
                },
            );
            let out = derive_fit_texture(&device, &queue, &derive, &tex, 0, false, sw, sh, 3);
            read_texture(&device, &queue, &out, sw, sh, 8).await
        });
        let got: Vec<f32> = out
            .chunks_exact(2)
            .map(|c| f16::from_bits(u16::from_le_bytes([c[0], c[1]])).to_f32())
            .collect();
        for (i, (g, s)) in got.iter().zip(src.iter()).enumerate() {
            let want = s.to_f32();
            assert!(
                (g - want).abs() <= 1e-3 * want.max(1.0),
                "component {i}: got {g} want {want} — HDR linear values must pass through the \
                 identity derive (no clamp, no encode)"
            );
        }
    }

    /// The derive reads exactly the requested mip LEVEL through its single-level view: with L0
    /// white and L1 written mid-gray directly, an identity derive from mip 1 must return gray —
    /// sampling L0 by mistake would return white.
    #[test]
    fn derive_reads_the_requested_mip_level() {
        let out = pollster::block_on(async {
            let instance = instance();
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    force_fallback_adapter: false,
                    compatible_surface: None,
                })
                .await
                .expect("no GPU adapter");
            let (device, queue, _p010) = request_device_p010(&adapter).await;
            let derive = build_derive(&device);
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("derive-mip-src"),
                size: wgpu::Extent3d {
                    width: 4,
                    height: 4,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 2,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            for (level, dim, val) in [(0u32, 4u32, 255u8), (1, 2, 128)] {
                let px = vec![val; (dim * dim * 4) as usize];
                queue.write_texture(
                    wgpu::ImageCopyTexture {
                        texture: &tex,
                        mip_level: level,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &px,
                    wgpu::ImageDataLayout {
                        offset: 0,
                        bytes_per_row: Some(dim * 4),
                        rows_per_image: Some(dim),
                    },
                    wgpu::Extent3d {
                        width: dim,
                        height: dim,
                        depth_or_array_layers: 1,
                    },
                );
            }
            let out = derive_fit_texture(&device, &queue, &derive, &tex, 1, true, 2, 2, 3);
            read_texture(&device, &queue, &out, 2, 2, 4).await
        });
        assert!(
            (127..=129).contains(&out[0]),
            "identity derive from mip 1 must return its gray (got {}), not L0's white",
            out[0]
        );
    }

    // ---- 110c A/B/X harness: downscale-quality comparison across ScalePolicy variants ----
    //
    // Candidates: BoxMipTrilinear (today's instant frame — trilinear sample of the box mip
    // chain), GpuDeriveLanczos {L2,L3} × {bias 0,−1} (the #110 derive), against two references:
    // the LINEAR-LIGHT CPU Lanczos3 (correctness ground truth — `cpu_derive_mode0`) and the
    // incumbent ENCODED-space Lanczos3 (`fast_image_resize`, compat). Metrics: NVIDIA FLIP mean
    // (perceptual), linear-light RMSE, and a detail ratio (candidate stddev / reference stddev —
    // >1 reads as aliasing, <1 as blur; a zone plate alone would reward blur, per the plan §8).
    //
    // Run the full matrix report with:
    //   cargo test -p pb-render --release -- --ignored ab_report --nocapture
    // The always-run tests below pin the two load-bearing facts (derive ≥ trilinear on a zone
    // plate; derive ≡ linear Lanczos when sourcing L0) with adapter-tolerant margins.

    /// One shared GPU context for the harness (device + the real mipgen/derive pipelines + a
    /// trilinear-sampler pass that reproduces the present path's instant frame).
    struct AbCtx {
        device: wgpu::Device,
        queue: wgpu::Queue,
        mipgen: MipGen,
        derive: DeriveLanczos,
        tri_pipeline: wgpu::RenderPipeline,
        tri_bgl: wgpu::BindGroupLayout,
    }

    const TRI_WGSL: &str = r#"
struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VsOut {
    let uv = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    return VsOut(vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0), uv);
}
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct P { lod: f32, _p0: f32, _p1: f32, _p2: f32 };
@group(0) @binding(2) var<uniform> p: P;
@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSampleLevel(src, samp, in.uv, p.lod);
}
"#;

    impl AbCtx {
        async fn new() -> Self {
            let instance = instance();
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    force_fallback_adapter: false,
                    compatible_surface: None,
                })
                .await
                .expect("no GPU adapter");
            let (device, queue, _p010) = request_device_p010(&adapter).await;
            let mipgen = build_mipgen(&device);
            let derive = build_derive(&device);
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("ab-tri"),
                source: wgpu::ShaderSource::Wgsl(TRI_WGSL.into()),
            });
            let tri_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("ab-tri-bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("ab-tri-layout"),
                bind_group_layouts: &[&tri_bgl],
                push_constant_ranges: &[],
            });
            let tri_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("ab-tri-pipeline"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: "vs",
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: "fs",
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            });
            AbCtx {
                device,
                queue,
                mipgen,
                derive,
                tri_pipeline,
                tri_bgl,
            }
        }

        /// Upload an opaque RGBA8 source and build its full box mip chain (the real MIPGEN).
        fn mipped_source(&self, src: &[[u8; 4]], w: u32, h: u32) -> wgpu::Texture {
            let levels = mip_levels(w, h);
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ab-src"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: levels,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let bytes: Vec<u8> = src.iter().flatten().copied().collect();
            self.queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &bytes,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
            generate_mips(&self.device, &self.queue, &self.mipgen, &tex, levels, true);
            tex
        }

        /// Today's instant frame: trilinear-sample the encoded mip chain at LOD = log2(ratio).
        async fn box_trilinear(&self, src: &wgpu::Texture, dw: u32, dh: u32) -> Vec<[u8; 4]> {
            let lod = (src.width() as f32 / dw as f32).log2().max(0.0);
            let out = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ab-tri-out"),
                size: wgpu::Extent3d {
                    width: dw,
                    height: dh,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });
            let pbuf = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("ab-tri-p"),
                    contents: bytemuck::cast_slice(&[lod, 0.0, 0.0, 0.0]),
                    usage: wgpu::BufferUsages::UNIFORM,
                });
            let view = src.create_view(&wgpu::TextureViewDescriptor::default());
            let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ab-tri-bg"),
                layout: &self.tri_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: pbuf.as_entire_binding(),
                    },
                ],
            });
            let out_view = out.create_view(&wgpu::TextureViewDescriptor::default());
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("ab-tri-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &out_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                pass.set_pipeline(&self.tri_pipeline);
                pass.set_bind_group(0, &bg, &[]);
                pass.draw(0..3, 0..1);
            }
            self.queue.submit(Some(encoder.finish()));
            to_px(read_texture(&self.device, &self.queue, &out, dw, dh, 4).await)
        }

        /// The #110 derive at (kernel, bias), timed submit→completion (a rough GPU-cost bound).
        async fn gpu_derive(
            &self,
            src: &wgpu::Texture,
            dw: u32,
            dh: u32,
            a: u32,
            bias: i32,
        ) -> (Vec<[u8; 4]>, std::time::Duration) {
            let level = select_derive_mip(
                src.width(),
                src.height(),
                src.mip_level_count(),
                dw,
                dh,
                bias,
            );
            let t0 = std::time::Instant::now();
            let out = derive_fit_texture(
                &self.device,
                &self.queue,
                &self.derive,
                src,
                level,
                true,
                dw,
                dh,
                a,
            );
            self.device.poll(wgpu::Maintain::Wait);
            let elapsed = t0.elapsed();
            (
                to_px(read_texture(&self.device, &self.queue, &out, dw, dh, 4).await),
                elapsed,
            )
        }
    }

    fn to_px(bytes: Vec<u8>) -> Vec<[u8; 4]> {
        bytes
            .chunks_exact(4)
            .map(|c| [c[0], c[1], c[2], c[3]])
            .collect()
    }

    /// The incumbent encoded-space Lanczos3 (`fast_image_resize`, exactly what pb-decode ships).
    fn cpu_encoded_lanczos3(src: &[[u8; 4]], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<[u8; 4]> {
        use fast_image_resize::images::Image;
        use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
        let bytes: Vec<u8> = src.iter().flatten().copied().collect();
        let s = Image::from_vec_u8(sw, sh, bytes, PixelType::U8x4).unwrap();
        let mut d = Image::new(dw, dh, PixelType::U8x4);
        let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
        Resizer::new().resize(&s, &mut d, &opts).unwrap();
        to_px(d.into_vec())
    }

    fn luma_lin(p: &[u8; 4]) -> f64 {
        0.2126 * ref_s2l(p[0] as f32 / 255.0) as f64
            + 0.7152 * ref_s2l(p[1] as f32 / 255.0) as f64
            + 0.0722 * ref_s2l(p[2] as f32 / 255.0) as f64
    }

    /// Linear-light RGB RMSE (0..1 scale).
    fn rmse_linear(a: &[[u8; 4]], b: &[[u8; 4]]) -> f64 {
        let mut acc = 0.0f64;
        for (x, y) in a.iter().zip(b) {
            for c in 0..3 {
                let d = ref_s2l(x[c] as f32 / 255.0) as f64 - ref_s2l(y[c] as f32 / 255.0) as f64;
                acc += d * d;
            }
        }
        (acc / (a.len() as f64 * 3.0)).sqrt()
    }

    /// NVIDIA FLIP perceptual mean (0 = identical).
    fn flip_mean(a: &[[u8; 4]], b: &[[u8; 4]], w: u32, h: u32) -> f32 {
        let rgb =
            |v: &[[u8; 4]]| -> Vec<u8> { v.iter().flat_map(|p| [p[0], p[1], p[2]]).collect() };
        let fa = nv_flip::FlipImageRgb8::with_data(w, h, &rgb(a));
        let fb = nv_flip::FlipImageRgb8::with_data(w, h, &rgb(b));
        let map = nv_flip::flip(fa, fb, nv_flip::DEFAULT_PIXELS_PER_DEGREE);
        nv_flip::FlipPool::from_image(&map).mean()
    }

    /// Detail proxy: linear-luma standard deviation. candidate/reference > 1 ⇒ aliasing energy,
    /// < 1 ⇒ blur — the counterweight the plan demands so a zone plate can't reward blur alone.
    fn stddev_lin(v: &[[u8; 4]]) -> f64 {
        let n = v.len() as f64;
        let mean = v.iter().map(luma_lin).sum::<f64>() / n;
        (v.iter().map(|p| (luma_lin(p) - mean).powi(2)).sum::<f64>() / n).sqrt()
    }

    // Deterministic test patterns (opaque; alpha correctness is covered by the unit tests).
    fn pat_zone_plate(w: u32, h: u32) -> Vec<[u8; 4]> {
        let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
        // Sweep to ~Nyquist at the corners.
        let kmax = std::f64::consts::PI;
        let rmax2 = cx * cx + cy * cy;
        (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f64 - cx, (i / w) as f64 - cy);
                let r2 = x * x + y * y;
                let v = 0.5 + 0.5 * (kmax * r2 / rmax2.sqrt()).cos();
                let e = (ref_l2s(v as f32) * 255.0).round() as u8;
                [e, e, e, 255]
            })
            .collect()
    }
    fn pat_slanted_edge(w: u32, h: u32) -> Vec<[u8; 4]> {
        (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f64, (i / w) as f64);
                // ~1:3 slope edge, plus a coloured edge in the lower half (gamma check).
                let dark = x + y / 3.0 < w as f64 * 0.5;
                if y < h as f64 / 2.0 {
                    if dark {
                        [16, 16, 16, 255]
                    } else {
                        [240, 240, 240, 255]
                    }
                } else if dark {
                    [200, 30, 30, 255]
                } else {
                    [30, 200, 30, 255]
                }
            })
            .collect()
    }
    fn pat_diag_1px(w: u32, h: u32) -> Vec<[u8; 4]> {
        (0..w * h)
            .map(|i| {
                let v = if ((i % w) + (i / w)) % 2 == 0 {
                    235
                } else {
                    20
                };
                [v, v, v, 255]
            })
            .collect()
    }
    fn pat_foliage(w: u32, h: u32) -> Vec<[u8; 4]> {
        // Two octaves of hashed value noise — photographic-ish mid-frequency texture.
        let hash = |x: u32, y: u32, s: u32| -> f64 {
            let mut v = x
                .wrapping_mul(0x9E37_79B9)
                .wrapping_add(y.wrapping_mul(0x85EB_CA6B))
                .wrapping_add(s.wrapping_mul(0xC2B2_AE35));
            v ^= v >> 15;
            v = v.wrapping_mul(0x2545_F491);
            v ^= v >> 13;
            (v & 0xFFFF) as f64 / 65535.0
        };
        let value = |x: u32, y: u32, cell: u32, s: u32| -> f64 {
            let (gx, gy) = (x / cell, y / cell);
            let (fx, fy) = (
                (x % cell) as f64 / cell as f64,
                (y % cell) as f64 / cell as f64,
            );
            let lerp = |a: f64, b: f64, t: f64| a + (b - a) * (t * t * (3.0 - 2.0 * t));
            let top = lerp(hash(gx, gy, s), hash(gx + 1, gy, s), fx);
            let bot = lerp(hash(gx, gy + 1, s), hash(gx + 1, gy + 1, s), fx);
            lerp(top, bot, fy)
        };
        (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                let v =
                    0.35 * value(x, y, 13, 1) + 0.45 * value(x, y, 5, 2) + 0.2 * value(x, y, 2, 3);
                let g = (ref_l2s(v.clamp(0.0, 1.0) as f32) * 255.0).round() as u8;
                [g / 3, g, g / 4, 255]
            })
            .collect()
    }

    /// The full A/B/X matrix report (run explicitly with `--ignored ab_report --nocapture`).
    /// Prints FLIP / RMSE / detail-ratio for every candidate × pattern × ratio against the
    /// linear-light reference, plus the encoded compat reference and a rough derive cost.
    #[test]
    #[ignore = "110c measurement harness — run explicitly with --nocapture"]
    fn ab_report() {
        pollster::block_on(async {
            let ctx = AbCtx::new().await;
            let (sw, sh) = (1024u32, 768u32);
            let patterns: [(&str, Vec<[u8; 4]>); 4] = [
                ("zone-plate", pat_zone_plate(sw, sh)),
                ("slant-edge", pat_slanted_edge(sw, sh)),
                ("diag-1px", pat_diag_1px(sw, sh)),
                ("foliage", pat_foliage(sw, sh)),
            ];
            let ratios = [1.25f64, 1.5, 2.0, 2.2, 2.8, 3.7, 5.1, 6.9];
            println!("== 110c A/B/X: candidate vs LINEAR-light Lanczos3 reference ==");
            println!("(flip: lower=better · rmse: lower=better · detail: 1.0=reference, >1 alias, <1 blur)");
            for (pname, src) in &patterns {
                let tex = ctx.mipped_source(src, sw, sh);
                for &r in &ratios {
                    let (dw, dh) = (
                        ((sw as f64 / r).round() as u32).max(1),
                        ((sh as f64 / r).round() as u32).max(1),
                    );
                    let lin_ref = cpu_derive_mode0(src, sw, sh, dw, dh, 3);
                    let enc_ref = cpu_encoded_lanczos3(src, sw, sh, dw, dh);
                    let ref_sd = stddev_lin(&lin_ref).max(1e-9);
                    let tri = ctx.box_trilinear(&tex, dw, dh).await;
                    let mut rows: Vec<(String, Vec<[u8; 4]>, f64)> = vec![
                        ("box-trilinear".into(), tri, 0.0),
                        ("cpu-encoded-l3".into(), enc_ref, 0.0),
                    ];
                    for (a, bias) in [(3u32, 0i32), (3, -1), (2, 0), (2, -1)] {
                        let (px, cost) = ctx.gpu_derive(&tex, dw, dh, a, bias).await;
                        rows.push((format!("gpu-L{a}b{bias}"), px, cost.as_secs_f64() * 1e3));
                    }
                    for (name, px, cost_ms) in &rows {
                        println!(
                            "{pname:>10} x{r:<4} {name:<15} flip={:.4} rmse={:.4} detail={:.3}{}",
                            flip_mean(&lin_ref, px, dw, dh),
                            rmse_linear(&lin_ref, px),
                            stddev_lin(px) / ref_sd,
                            if *cost_ms > 0.0 {
                                format!("  (~{cost_ms:.2} ms)")
                            } else {
                                String::new()
                            }
                        );
                    }
                }
            }
        });
    }

    /// Always-run regression, load-bearing fact #1: on a zone plate at a non-power-of-two ratio
    /// the derive is perceptually CLOSER to ground-truth linear Lanczos than the trilinear
    /// instant frame — the whole reason #110 exists. Adapter-tolerant margin (strictly less,
    /// not a fixed threshold).
    #[test]
    fn derive_beats_trilinear_against_the_linear_reference() {
        pollster::block_on(async {
            let ctx = AbCtx::new().await;
            let (sw, sh) = (512u32, 384u32);
            let src = pat_zone_plate(sw, sh);
            let tex = ctx.mipped_source(&src, sw, sh);
            let r = 3.7f64;
            let (dw, dh) = (
                (sw as f64 / r).round() as u32,
                (sh as f64 / r).round() as u32,
            );
            let lin_ref = cpu_derive_mode0(&src, sw, sh, dw, dh, 3);
            let tri = ctx.box_trilinear(&tex, dw, dh).await;
            let (gpu, _) = ctx.gpu_derive(&tex, dw, dh, 3, 0).await;
            let f_tri = flip_mean(&lin_ref, &tri, dw, dh);
            let f_gpu = flip_mean(&lin_ref, &gpu, dw, dh);
            assert!(
                f_gpu < f_tri,
                "the derive (flip {f_gpu:.4}) must beat box-trilinear (flip {f_tri:.4}) at {r}x"
            );
        });
    }

    /// Always-run regression, load-bearing fact #2: when the derive sources L0 (ratio < 2 —
    /// no box-prefilter in the chain), it IS linear-light Lanczos3, so it must match the CPU
    /// reference almost exactly (fp16 intermediate quantization only).
    #[test]
    fn derive_from_l0_matches_linear_lanczos_exactly() {
        pollster::block_on(async {
            let ctx = AbCtx::new().await;
            let (sw, sh) = (512u32, 384u32);
            let src = pat_foliage(sw, sh);
            let tex = ctx.mipped_source(&src, sw, sh);
            let (dw, dh) = (341, 256); // 1.5x — select_derive_mip stays at L0
            assert_eq!(
                select_derive_mip(sw, sh, tex.mip_level_count(), dw, dh, 0),
                0,
                "test premise: 1.5x sources L0"
            );
            let lin_ref = cpu_derive_mode0(&src, sw, sh, dw, dh, 3);
            let (gpu, _) = ctx.gpu_derive(&tex, dw, dh, 3, 0).await;
            let rmse = rmse_linear(&lin_ref, &gpu);
            assert!(
                rmse < 0.004,
                "L0-sourced derive must equal linear Lanczos3 (rmse {rmse:.5})"
            );
        });
    }

    /// Generate mip 1 of a `w`×`h` sRGB L0 and read it back (rows tightly packed, RGBA8).
    async fn gen_mip1_srgb(w: u32, h: u32, texels: &[[u8; 4]]) -> Vec<u8> {
        let instance = instance();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
            })
            .await
            .expect("no GPU adapter");
        let (device, queue, _p010) = request_device_p010(&adapter).await;
        let mipgen = build_mipgen(&device);

        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("mip-odd-test"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 2,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let l0: Vec<u8> = texels.iter().flatten().copied().collect();
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &l0,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        generate_mips(&device, &queue, &mipgen, &tex, 2, true);

        let (mw, mh) = ((w / 2).max(1), (h / 2).max(1));
        let padded = (mw * 4).div_ceil(256) * 256; // wgpu: bytes_per_row multiple of 256
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mip-odd-readback"),
            size: (padded * mh) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("mip-odd-copy"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 1,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &readback,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(mh),
                },
            },
            wgpu::Extent3d {
                width: mw,
                height: mh,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(std::iter::once(encoder.finish()));

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.poll(wgpu::Maintain::Wait);
        rx.recv().expect("map channel").expect("map readback");
        let mapped = slice.get_mapped_range();
        let mut out = Vec::with_capacity((mw * mh * 4) as usize);
        for row in 0..mh {
            let s = (row * padded) as usize;
            out.extend_from_slice(&mapped[s..s + (mw * 4) as usize]);
        }
        drop(mapped);
        readback.unmap();
        out
    }

    /// Build a 2×2 sRGB texture, generate its single mip level, and read back the 1×1 result.
    async fn gen_mip_1x1_srgb(texels: [[u8; 4]; 4]) -> [u8; 4] {
        let instance = instance();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
            })
            .await
            .expect("no GPU adapter");
        let (device, queue, _p010) = request_device_p010(&adapter).await;
        let mipgen = build_mipgen(&device);

        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("mip-test"),
            size: wgpu::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 2,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let l0: Vec<u8> = texels.iter().flatten().copied().collect();
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &l0,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(8),
                rows_per_image: Some(2),
            },
            wgpu::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
        );
        generate_mips(&device, &queue, &mipgen, &tex, 2, true);

        let padded = 256u32; // wgpu requires bytes_per_row a multiple of 256
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mip-readback"),
            size: padded as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("mip-copy"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 1,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &readback,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(std::iter::once(encoder.finish()));

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.poll(wgpu::Maintain::Wait);
        rx.recv().expect("map channel").expect("map readback");
        let mapped = slice.get_mapped_range();
        let px = [mapped[0], mapped[1], mapped[2], mapped[3]];
        drop(mapped);
        readback.unmap();
        px
    }

    /// The subtitle quad is the one overlay placed at an absolute point rather than an
    /// inset, so its px→NDC conversion is the whole contract with `subtitle::place`.
    #[test]
    fn subtitle_quad_maps_pixels_to_ndc() {
        // A 100×20 block at (50, 80) on a 200×100 screen: x 50..150 → NDC -0.5..0.5,
        // y 80..100 → NDC -0.6..-1.0 (NDC +y is up, pixels count down).
        let v = subtitle_quad_vertices(100, 20, 200, 100, 50.0, 80.0);
        assert_eq!(v[0].pos, [-0.5, -0.6], "top-left");
        assert_eq!(v[1].pos, [0.5, -0.6], "top-right");
        assert_eq!(v[2].pos, [0.5, -1.0], "bottom-right");
        assert_eq!(v[3].pos, [-0.5, -1.0], "bottom-left");
        // UVs stay upright — a flipped subtitle is not a subtitle.
        assert_eq!(v[0].uv, [0.0, 0.0]);
        assert_eq!(v[2].uv, [1.0, 1.0]);
    }

    /// `place` clamps the *text*'s box on screen, not the bitmap's, so a block whose soft
    /// shadow bleeds off the top arrives with a negative `y`. It must extend past the edge
    /// and be clipped — not wrap, saturate to 0, or be rejected.
    #[test]
    fn subtitle_quad_accepts_a_negative_origin() {
        // A 20px block at y = -10 straddles the top edge: 10px off-screen, 10px on.
        let v = subtitle_quad_vertices(100, 20, 200, 100, 50.0, -10.0);
        assert_eq!(
            v[0].pos[1], 1.2,
            "top edge sits above NDC +1 (clipped away)"
        );
        assert_eq!(
            v[2].pos[1], 0.8,
            "bottom edge is 10px down the screen, still visible"
        );
    }

    /// Draw one premultiplied RGBA8 block over a black fp16 intermediate through the real
    /// subtitle pipeline, and read back the linear result.
    fn subtitle_over_black(src: [u8; 4]) -> [f32; 4] {
        let _guard = crate::gpu_test_lock();
        let (device, queue, _bgl) = test_device();
        let pipelines = build_pipelines(&device, wgpu::TextureFormat::Rgba8Unorm);
        let mut upload = StagingUpload::new();
        let (w, h) = (2u32, 2u32);
        let pixels: Vec<u8> = src
            .iter()
            .copied()
            .cycle()
            .take((w * h * 4) as usize)
            .collect();
        let bind_group = upload_image(
            &device,
            &queue,
            &pipelines.scene_bgl,
            &mut upload,
            &pixels,
            w,
            h,
            &ColorTransform::srgb(),
            false,
            1.0,
            None,
        );
        // The quad covers the whole 2×2 target, so every texel is the blend result.
        let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("subtitle-test-vbuf"),
            contents: bytemuck::cast_slice(&subtitle_quad_vertices(w, h, w, h, 0.0, 0.0)),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let ibuf = index_buffer(&device);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("subtitle-test-target"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: INTERMEDIATE_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let padded = (w * 8).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("subtitle-test-readback"),
            size: (padded * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            // Clear to opaque black, then composite the block over it — the same
            // Load-and-blend the real pass does over the photo.
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("subtitle-test"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&pipelines.subtitle);
            rp.set_bind_group(0, &bind_group, &[]);
            rp.set_vertex_buffer(0, vbuf.slice(..));
            rp.set_index_buffer(ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rp.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &readback,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(std::iter::once(encoder.finish()));
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.poll(wgpu::Maintain::Wait);
        rx.recv().expect("map channel").expect("map readback");
        let mapped = slice.get_mapped_range();
        let ch = |o: usize| half::f16::from_le_bytes([mapped[o], mapped[o + 1]]).to_f32();
        let out = [ch(0), ch(2), ch(4), ch(6)];
        drop(mapped);
        readback.unmap();
        out
    }

    /// **The subtitle layer must blend premultiplied, and this is what proves it.**
    ///
    /// `pb_hud::subtitle` emits premultiplied RGBA; the other CPU overlays are authored
    /// straight, so they share a pipeline whose `ALPHA_BLENDING` multiplies by alpha. Send
    /// a premultiplied bitmap through *that* and every value is multiplied by alpha twice —
    /// antialiased glyph edges and translucent backgrounds come out visibly dark. The two
    /// answers are far apart (0.216 vs 0.108), so this is a cheap, decisive check.
    ///
    /// The expected value is also what the **macOS** host produces: its `CGImage` is
    /// `.premultipliedLast` composited in sRGB, which is the same arithmetic as
    /// premultiplied-over on our sRGB-decoded texels. Same rasterizer, same look, both
    /// shells — which is the whole point of #90.5.
    #[test]
    fn the_subtitle_pipeline_blends_premultiplied_not_straight() {
        // 50%-alpha white, premultiplied in sRGB space (what the rasterizer emits).
        let got = subtitle_over_black([128, 128, 128, 128]);
        let premultiplied = srgb_to_linear(128) as f32; // ≈ 0.2158 — src + dst*(1-a)
        let straight = premultiplied * (128.0 / 255.0); // ≈ 0.1083 — the double-multiply bug
        assert!(
            (got[0] - premultiplied).abs() < 0.01,
            "expected premultiplied-over ({premultiplied:.4}), got {:.4}",
            got[0]
        );
        assert!(
            (got[0] - straight).abs() > 0.05,
            "this is the double-multiplied value ({straight:.4}) — the subtitle layer is on \
             the straight-alpha overlay pipeline"
        );
    }

    /// A fully opaque cue must land exactly on its own color: no alpha term anywhere, no
    /// dependence on what was underneath. Guards the other end of the blend equation.
    #[test]
    fn an_opaque_subtitle_pixel_replaces_the_background() {
        let got = subtitle_over_black([255, 255, 255, 255]);
        assert!(
            (got[0] - 1.0).abs() < 0.01,
            "opaque white must stay white, got {:.4}",
            got[0]
        );
    }

    /// A resize re-derives NDC from the same px origin (the core re-places it a tick
    /// later). Same pixels on a wider screen must mean a narrower NDC span, or the block
    /// would stretch with the window.
    #[test]
    fn subtitle_quad_rederives_ndc_on_resize() {
        let narrow = subtitle_quad_vertices(100, 20, 200, 100, 50.0, 80.0);
        let wide = subtitle_quad_vertices(100, 20, 400, 100, 50.0, 80.0);
        let span = |v: &[Vertex; 4]| v[1].pos[0] - v[0].pos[0];
        assert_eq!(span(&narrow), 1.0, "100px of 200 = half the screen");
        assert_eq!(span(&wide), 0.5, "100px of 400 = a quarter");
    }

    fn close(a: [u8; 4], b: [u8; 4], tol: i32) -> bool {
        (0..4).all(|k| (a[k] as i32 - b[k] as i32).abs() <= tol)
    }

    #[test]
    fn choose_draw_source_prefers_ring_then_held_then_single() {
        use DrawSource::*;
        // blank overrides everything, whatever else is set.
        assert_eq!(choose_draw_source(true, true, true), Blank);
        assert_eq!(choose_draw_source(true, false, false), Blank);
        // A live presented ring slot wins over a held frame.
        assert_eq!(choose_draw_source(false, true, true), RingSlot);
        assert_eq!(choose_draw_source(false, true, false), RingSlot);
        // No live slot but a held frame (the geometry-change gap): draw the hold, NOT the
        // stale single-image bind group — this is the task #18 finding #5 fix.
        assert_eq!(choose_draw_source(false, false, true), Held);
        // No slot, no hold: the single-image path (post-startup / after set_image).
        assert_eq!(choose_draw_source(false, false, false), Single);
    }

    /// Headless device + the image bind-group layout, for the reuse-slot tests.
    fn test_device() -> (wgpu::Device, wgpu::Queue, wgpu::BindGroupLayout) {
        pollster::block_on(async {
            let instance = instance();
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    force_fallback_adapter: false,
                    compatible_surface: None,
                })
                .await
                .expect("no GPU adapter");
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor::default(), None)
                .await
                .expect("request device");
            let bgl = tex_sampler_uniform_bgl(&device, "test-img-bgl");
            (device, queue, bgl)
        })
    }

    /// Task #79 phase 3: same-geometry frames — the animation/video steady state —
    /// must reuse the slot's texture and bind group (zero creations per frame);
    /// a size change must rebuild them.
    #[test]
    fn reuse_slot_keeps_resources_across_same_size_frames_and_rebuilds_on_resize() {
        let _gpu = crate::gpu_test_lock();
        let (device, queue, bgl) = test_device();
        let mut uploader = crate::upload::StagingUpload::new();
        let mut slot: Option<ReuseSlot> = None;
        let color = ColorTransform::srgb();

        let frame = vec![100u8; 320 * 180 * 4];
        let first = upload_image_reusable(
            &device,
            &queue,
            &bgl,
            &mut uploader,
            &mut slot,
            &frame,
            320,
            180,
            &color,
            false,
            1.0,
        );
        assert!(
            matches!(first, ReuseOutcome::Rebuilt(_)),
            "first frame builds the slot"
        );
        let tex0 = slot.as_ref().unwrap().tex.global_id();
        let second = upload_image_reusable(
            &device,
            &queue,
            &bgl,
            &mut uploader,
            &mut slot,
            &frame,
            320,
            180,
            &color,
            false,
            1.0,
        );
        assert!(
            matches!(second, ReuseOutcome::Reused),
            "same geometry must reuse"
        );
        assert_eq!(
            slot.as_ref().unwrap().tex.global_id(),
            tex0,
            "same geometry must keep the texture"
        );

        // A different color transform still reuses (uniform rewritten in place).
        let wide = ColorTransform {
            matrix: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            trc: [2.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            enabled: true,
        };
        let recolored = upload_image_reusable(
            &device,
            &queue,
            &bgl,
            &mut uploader,
            &mut slot,
            &frame,
            320,
            180,
            &wide,
            false,
            1.0,
        );
        assert!(
            matches!(recolored, ReuseOutcome::Reused),
            "a color change reuses too"
        );

        // Resize rebuilds.
        let bigger = vec![50u8; 640 * 360 * 4];
        let resized = upload_image_reusable(
            &device,
            &queue,
            &bgl,
            &mut uploader,
            &mut slot,
            &bigger,
            640,
            360,
            &color,
            false,
            1.0,
        );
        assert!(
            matches!(resized, ReuseOutcome::Rebuilt(_)),
            "resize must rebuild"
        );
        assert_ne!(
            slot.as_ref().unwrap().tex.global_id(),
            tex0,
            "resize must rebuild the texture"
        );
        device.poll(wgpu::Maintain::Wait);
    }

    /// Task 79.10 golden: the `fs_scene_nv12` two-plane GPU convert must match the
    /// CPU reference (`crate::yuv::nv12_to_rgba`) within quantization, for every
    /// matrix × range. Uniform chroma per frame keeps bilinear-vs-nearest chroma
    /// sampling out of the comparison (Y is full-res and 1:1, so it's exact).
    #[test]
    fn offscreen_nv12_matches_the_cpu_reference() {
        let _guard = crate::gpu_test_lock();
        // 4×4 luma gradient blocks over one uniform UV sample per case.
        let y: Vec<u8> = [
            60u8, 60, 120, 120, 60, 60, 120, 120, 180, 180, 235, 235, 180, 180, 235, 235,
        ]
        .to_vec();
        for matrix in [
            crate::YuvMatrix::Bt601,
            crate::YuvMatrix::Bt709,
            crate::YuvMatrix::Bt2020,
        ] {
            for full_range in [false, true] {
                for (u, v) in [(128u8, 128u8), (90, 240), (200, 80)] {
                    let uv = vec![u, v, u, v, u, v, u, v]; // 2×2 UV texels
                    let params = crate::YuvParams { matrix, full_range };
                    let got = render_offscreen_nv12(&y, &uv, 4, 4, 4, 4, params);
                    let want = crate::yuv::nv12_to_rgba(&y, &uv, 4, 4, params);
                    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
                        assert!(
                            (*g as i32 - *w as i32).abs() <= 3,
                            "{matrix:?} full={full_range} uv=({u},{v}) byte {i}: gpu {g} cpu {w}"
                        );
                    }
                }
            }
        }
    }

    /// Task 79.10: the UV plane's orientation — left half blue-leaning chroma,
    /// right half red-leaning — must come out on the correct sides (catches a
    /// swapped/transposed chroma plane that uniform-UV goldens can't see).
    #[test]
    fn offscreen_nv12_chroma_plane_orientation() {
        let _guard = crate::gpu_test_lock();
        let y = vec![128u8; 16];
        // 2×2 UV texels: columns 0 = blue-ish (U high), 1 = red-ish (V high).
        let uv = vec![220u8, 128, 128, 220, 220, 128, 128, 220];
        let params = crate::YuvParams {
            matrix: crate::YuvMatrix::Bt709,
            full_range: true,
        };
        let got = render_offscreen_nv12(&y, &uv, 4, 4, 4, 4, params);
        let px = |x: usize, y_: usize| {
            let o = (y_ * 4 + x) * 4;
            (got[o] as i32, got[o + 2] as i32) // (r, b)
        };
        let (r_left, b_left) = px(0, 0);
        let (r_right, b_right) = px(3, 0);
        assert!(
            b_left > r_left + 40,
            "left is blue-leaning: r={r_left} b={b_left}"
        );
        assert!(
            r_right > b_right + 40,
            "right is red-leaning: r={r_right} b={b_right}"
        );
    }

    // ── Task #91 Phase 2: planar GPU color path golden, vs an INDEPENDENT reference ──
    //
    // The reference below re-derives everything from spec — YUV matrix from Kr/Kb,
    // range from explicit 10-bit code formulas (not the `65535/64` normalized-sample
    // path `planar_range` uses), PQ/HLG from raw SMPTE/ARIB constants, and a
    // hardcoded BT.2020→709 primaries matrix — so a bug in the production math or
    // shader can't hide by matching itself (Codex: independent reference mandatory).

    /// BT.2020 → BT.709 linear primaries matrix (Rec. BT.2087), a well-known
    /// independent constant.
    #[allow(clippy::excessive_precision)] // deliberate reference constants (Rec. BT.2087)
    const BT2020_TO_709: [[f32; 3]; 3] = [
        [1.660491, -0.587641, -0.072850],
        [-0.124550, 1.132900, -0.008349],
        [-0.018151, -0.100579, 1.118730],
    ];

    fn ref_srgb_to_linear(x: f32) -> f32 {
        if x <= 0.04045 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        }
    }

    fn ref_pq(e: f32) -> f32 {
        let (m1, m2) = (2610.0 / 16384.0f32, 2523.0 / 4096.0 * 128.0);
        let (c1, c2, c3) = (
            3424.0 / 4096.0f32,
            2413.0 / 4096.0 * 32.0,
            2392.0 / 4096.0 * 32.0,
        );
        let e = e.clamp(0.0, 1.0);
        let ep = e.powf(1.0 / m2);
        let num = (ep - c1).max(0.0);
        let den = c2 - c3 * ep;
        if den <= 0.0 {
            return 0.0;
        }
        (num / den).powf(1.0 / m1) * 10000.0 / 203.0
    }

    fn ref_hlg(e: f32) -> f32 {
        let (a, b, c) = (0.17883277f32, 0.28466892, 0.5599107);
        let e = e.clamp(0.0, 1.0);
        let ys = if e <= 0.5 {
            e * e / 3.0
        } else {
            (((e - c) / a).exp() + b) / 12.0
        };
        1000.0 * ys.max(0.0).powf(1.2) / 203.0
    }

    /// Independent scene-linear scRGB for one planar code triple.
    fn ref_scene(
        yc: u32,
        uc: u32,
        vc: u32,
        ten: bool,
        m: crate::YuvMatrix,
        full: bool,
        tr: crate::PlanarTransfer,
    ) -> [f32; 3] {
        let (yf, uf, vf) = (yc as f32, uc as f32, vc as f32);
        let (yn, un, vn) = match (ten, full) {
            (true, true) => (yf / 1023.0, (uf - 512.0) / 1023.0, (vf - 512.0) / 1023.0),
            (true, false) => (
                (yf - 64.0) / 876.0,
                (uf - 512.0) / 896.0,
                (vf - 512.0) / 896.0,
            ),
            (false, true) => (yf / 255.0, (uf - 128.0) / 255.0, (vf - 128.0) / 255.0),
            (false, false) => (
                (yf - 16.0) / 219.0,
                (uf - 128.0) / 224.0,
                (vf - 128.0) / 224.0,
            ),
        };
        let (kr, kb) = match m {
            crate::YuvMatrix::Bt601 => (0.299f32, 0.114),
            crate::YuvMatrix::Bt709 => (0.2126, 0.0722),
            crate::YuvMatrix::Bt2020 => (0.2627, 0.0593),
        };
        let kg = 1.0 - kr - kb;
        let er = (yn + 2.0 * (1.0 - kr) * vn).clamp(0.0, 1.0);
        let eg = (yn - (2.0 * kb * (1.0 - kb) / kg) * un - (2.0 * kr * (1.0 - kr) / kg) * vn)
            .clamp(0.0, 1.0);
        let eb = (yn + 2.0 * (1.0 - kb) * un).clamp(0.0, 1.0);
        let mat = |l: [f32; 3]| {
            let m = BT2020_TO_709;
            [
                m[0][0] * l[0] + m[0][1] * l[1] + m[0][2] * l[2],
                m[1][0] * l[0] + m[1][1] * l[1] + m[1][2] * l[2],
                m[2][0] * l[0] + m[2][1] * l[1] + m[2][2] * l[2],
            ]
        };
        match tr {
            crate::PlanarTransfer::SrgbLike => [
                ref_srgb_to_linear(er),
                ref_srgb_to_linear(eg),
                ref_srgb_to_linear(eb),
            ],
            crate::PlanarTransfer::Pq => mat([ref_pq(er), ref_pq(eg), ref_pq(eb)]),
            crate::PlanarTransfer::Hlg => mat([ref_hlg(er), ref_hlg(eg), ref_hlg(eb)]),
            crate::PlanarTransfer::Parametric => unreachable!("not exercised by the golden"),
        }
    }

    /// A `w×h` uniform planar frame with the given native-bit-depth code triple.
    fn uniform_planar(w: u32, h: u32, ten: bool, yc: u32, uc: u32, vc: u32) -> (Vec<u8>, Vec<u8>) {
        let (w, h) = (w as usize, h as usize);
        let push = |buf: &mut Vec<u8>, code: u32| {
            if ten {
                buf.extend_from_slice(&(((code << 6) as u16).to_le_bytes()));
            } else {
                buf.push(code as u8);
            }
        };
        let mut y = Vec::new();
        for _ in 0..w * h {
            push(&mut y, yc);
        }
        let mut uv = Vec::new();
        for _ in 0..(w / 2) * (h / 2) {
            push(&mut uv, uc);
            push(&mut uv, vc);
        }
        (y, uv)
    }

    /// The golden: the GPU `fs_scene_planar` path (read back from the fp16 scene
    /// intermediate) matches the independent reference across bit depth × range ×
    /// matrix × transfer, for a spread of code triples. On an adapter without
    /// `TEXTURE_FORMAT_16BIT_NORM`, P010 renders `None` and the CPU fallback
    /// (`planar_to_scene`) is asserted against the same reference instead.
    #[test]
    fn planar_scene_matches_independent_reference() {
        let _guard = crate::gpu_test_lock();
        // (name, transfer, ten_bit-required, matrix). 10-bit codes; 8-bit = /4.
        struct Case {
            tr: crate::PlanarTransfer,
            ten: bool,
            m: crate::YuvMatrix,
        }
        let cases = [
            Case {
                tr: crate::PlanarTransfer::SrgbLike,
                ten: false,
                m: crate::YuvMatrix::Bt601,
            },
            Case {
                tr: crate::PlanarTransfer::SrgbLike,
                ten: false,
                m: crate::YuvMatrix::Bt709,
            },
            Case {
                tr: crate::PlanarTransfer::SrgbLike,
                ten: false,
                m: crate::YuvMatrix::Bt2020,
            },
            Case {
                tr: crate::PlanarTransfer::SrgbLike,
                ten: true,
                m: crate::YuvMatrix::Bt709,
            },
            Case {
                tr: crate::PlanarTransfer::Pq,
                ten: true,
                m: crate::YuvMatrix::Bt2020,
            },
            Case {
                tr: crate::PlanarTransfer::Hlg,
                ten: true,
                m: crate::YuvMatrix::Bt2020,
            },
        ];
        // 10-bit code triples (y,u,v).
        let codes10 = [
            (512, 512, 512),
            (900, 512, 512),
            (600, 400, 700),
            (600, 700, 400),
            (120, 512, 512),
        ];
        for case in cases {
            for full in [false, true] {
                for (y10, u10, v10) in codes10 {
                    let (yc, uc, vc) = if case.ten {
                        (y10, u10, v10)
                    } else {
                        (y10 / 4, u10 / 4, v10 / 4)
                    };
                    let (y, uv) = uniform_planar(4, 4, case.ten, yc, uc, vc);
                    let want = ref_scene(yc, uc, vc, case.ten, case.m, full, case.tr);
                    let color = match case.tr {
                        crate::PlanarTransfer::Pq | crate::PlanarTransfer::Hlg => ColorTransform {
                            matrix: BT2020_TO_709,
                            trc: [1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                            enabled: true,
                        },
                        _ => ColorTransform::srgb(),
                    };
                    let present = PlanarPresentation {
                        format: if case.ten {
                            crate::PlanarFormat::P010
                        } else {
                            crate::PlanarFormat::Nv12
                        },
                        transfer: case.tr,
                        yuv: crate::YuvParams {
                            matrix: case.m,
                            full_range: full,
                        },
                        color,
                        peak: 1.0,
                    };
                    let tol = |w: f32| 0.02 + 0.02 * w.abs();
                    let ctx = format!(
                        "{:?} ten={} full={full} m={:?} codes=({yc},{uc},{vc})",
                        case.tr, case.ten, case.m
                    );
                    match render_offscreen_planar_scene(&y, &uv, present, 4, 4, 4, 4) {
                        Some(px) => {
                            let got = px[2 * 4 + 2]; // center pixel
                            for c in 0..3 {
                                assert!(
                                    (got[c] - want[c]).abs() <= tol(want[c]),
                                    "GPU {ctx}: ch{c} got {} want {} (±{})",
                                    got[c],
                                    want[c],
                                    tol(want[c])
                                );
                            }
                        }
                        None => {
                            // Adapter lacks 16-bit-norm: assert the CPU fallback matches.
                            assert!(case.ten, "only P010 can be unsupported");
                            let f = crate::yuv::planar_to_scene(
                                &y,
                                &uv,
                                4,
                                4,
                                present.format,
                                present.yuv,
                                present.transfer,
                                &present.color,
                                present.peak,
                            );
                            if f.hdr {
                                let px = &f.bytes[(2 * 4 + 2) * 8..];
                                let ch = |o: usize| {
                                    half::f16::from_le_bytes([px[o], px[o + 1]]).to_f32()
                                };
                                let got = [ch(0), ch(2), ch(4)];
                                for c in 0..3 {
                                    assert!(
                                        (got[c] - want[c]).abs() <= tol(want[c]),
                                        "CPU {ctx}: ch{c} got {} want {}",
                                        got[c],
                                        want[c]
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Task 79.10: the NV12 reuse slot — same geometry reuses both plane textures
    /// (zero per-frame creation), and an RGBA↔NV12 flip at the same size rebuilds.
    #[test]
    fn nv12_reuse_slot_reuses_planes_and_format_flips_rebuild() {
        let _guard = crate::gpu_test_lock();
        let (device, queue, _bgl) = test_device();
        let planar_layout = super::planar_bgl(&device);
        let rgba_layout = tex_sampler_uniform_bgl(&device, "test-rgba");
        let mut uploader = StagingUpload::new();
        let mut slot: Option<ReuseSlot> = None;
        let color = ColorTransform::srgb();
        let params = crate::YuvParams {
            matrix: crate::YuvMatrix::Bt709,
            full_range: false,
        };
        let y = vec![100u8; 64 * 32];
        let uv = vec![128u8; 64 * 16];

        let first = upload_planar_reusable(
            &device,
            &queue,
            &planar_layout,
            &mut uploader,
            &mut slot,
            &y,
            &uv,
            64,
            32,
            &color,
            params,
            crate::PlanarFormat::Nv12,
            0.0,
            1.0,
        );
        assert!(matches!(first, ReuseOutcome::Rebuilt(_)));
        let y0 = slot.as_ref().unwrap().tex.global_id();
        assert!(slot.as_ref().unwrap().uv_tex.is_some(), "two-plane slot");

        let second = upload_planar_reusable(
            &device,
            &queue,
            &planar_layout,
            &mut uploader,
            &mut slot,
            &y,
            &uv,
            64,
            32,
            &color,
            params,
            crate::PlanarFormat::Nv12,
            0.0,
            1.0,
        );
        assert!(
            matches!(second, ReuseOutcome::Reused),
            "steady state reuses"
        );
        assert_eq!(slot.as_ref().unwrap().tex.global_id(), y0);

        // Same dimensions, RGBA frame: the format flip must rebuild (an RGBA bind
        // group over an R8 luma texture would be wrong).
        let rgba = vec![0u8; 64 * 32 * 4];
        let flipped = upload_image_reusable(
            &device,
            &queue,
            &rgba_layout,
            &mut uploader,
            &mut slot,
            &rgba,
            64,
            32,
            &color,
            false,
            1.0,
        );
        assert!(
            matches!(flipped, ReuseOutcome::Rebuilt(_)),
            "format flip rebuilds"
        );
        assert!(
            slot.as_ref().unwrap().uv_tex.is_none(),
            "back to single-plane"
        );
        device.poll(wgpu::Maintain::Wait);
    }

    /// Opt-in measurement (task #79 phase 3 acceptance): per-frame CPU cost of the
    /// present upload, old path (create everything per frame) vs the reuse slot.
    /// `PB_PRESENT_BENCH=1 cargo test -p pb-render --release present_path_churn -- --nocapture --ignored`
    #[test]
    #[ignore = "opt-in measurement; run with --ignored"]
    fn present_path_churn_before_vs_after() {
        let _gpu = crate::gpu_test_lock();
        let (device, queue, bgl) = test_device();
        let color = ColorTransform::srgb();
        const N: usize = 240;

        let pct = |mut v: Vec<f64>, p: f64| {
            v.sort_by(f64::total_cmp);
            v[((v.len() - 1) as f64 * p).round() as usize]
        };

        for (label, w, h) in [("1080p", 1920u32, 1080u32), ("4K", 3840, 2160)] {
            let frame = vec![128u8; (w * h * 4) as usize];

            // Old path: full per-frame creation (what set_image did before).
            let mut old_uploader = crate::upload::StagingUpload::new();
            let mut old_ms = Vec::with_capacity(N);
            for _ in 0..N {
                let t = std::time::Instant::now();
                let _bg = upload_image(
                    &device,
                    &queue,
                    &bgl,
                    &mut old_uploader,
                    &frame,
                    w,
                    h,
                    &color,
                    false,
                    1.0,
                    None,
                );
                old_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            device.poll(wgpu::Maintain::Wait);

            // New path: the reuse slot.
            let mut uploader = crate::upload::StagingUpload::new();
            let mut slot: Option<ReuseSlot> = None;
            let mut new_ms = Vec::with_capacity(N);
            for _ in 0..N {
                let t = std::time::Instant::now();
                let _bg = upload_image_reusable(
                    &device,
                    &queue,
                    &bgl,
                    &mut uploader,
                    &mut slot,
                    &frame,
                    w,
                    h,
                    &color,
                    false,
                    1.0,
                );
                new_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            device.poll(wgpu::Maintain::Wait);

            eprintln!(
                "present upload {label} x{N}: old p50={:.3}ms p95={:.3}ms | reuse p50={:.3}ms p95={:.3}ms",
                pct(old_ms.clone(), 0.5),
                pct(old_ms, 0.95),
                pct(new_ms.clone(), 0.5),
                pct(new_ms, 0.95),
            );
        }
    }

    #[test]
    fn offscreen_letterboxes_and_draws_image() {
        let _gpu = crate::gpu_test_lock();
        let (iw, ih) = (1600u32, 1000u32);
        let img = test_pattern(iw, ih);
        let (sw, sh) = (1920u32, 1080u32);
        let out = render_offscreen(&img, iw, ih, sw, sh);
        assert_eq!(out.len(), (sw * sh * 4) as usize);

        // 1600x1000 into 1920x1080 is height-bound -> 1728x1080, pillarboxed.
        // Left/right columns are letterbox background.
        assert!(
            close(at(&out, sw, 0, sh / 2), LETTERBOX, 2),
            "left bar = {:?}",
            at(&out, sw, 0, sh / 2)
        );
        assert!(
            close(at(&out, sw, sw - 1, sh / 2), LETTERBOX, 2),
            "right bar = {:?}",
            at(&out, sw, sw - 1, sh / 2)
        );
        // Center is the white block.
        assert!(
            close(at(&out, sw, sw / 2, sh / 2), [255, 255, 255, 255], 4),
            "center = {:?}",
            at(&out, sw, sw / 2, sh / 2)
        );
        // The image fills the full height, so a top-center pixel is image, not bar.
        assert!(
            !close(at(&out, sw, sw / 2, 2), LETTERBOX, 2),
            "top-center should be image, got {:?}",
            at(&out, sw, sw / 2, 2)
        );
    }

    #[test]
    fn transparent_image_blends_over_letterbox() {
        let _gpu = crate::gpu_test_lock();
        let img = [250, 0, 0, 0];
        let out = render_offscreen(&img, 1, 1, 4, 4);
        assert!(
            close(at(&out, 4, 2, 2), LETTERBOX, 2),
            "transparent image should reveal letterbox, got {:?}",
            at(&out, 4, 2, 2)
        );
    }

    fn srgb_oetf_ref(c: f32) -> f32 {
        let x = c.clamp(0.0, 1.0);
        if x <= 0.0031308 {
            12.92 * x
        } else {
            1.055 * x.powf(1.0 / 2.4) - 0.055
        }
    }

    #[test]
    fn disabled_color_transform_is_bit_exact_passthrough() {
        let _gpu = crate::gpu_test_lock();
        // A solid opaque pixel rendered with the (disabled) sRGB transform must
        // come back unchanged — the common case stays exact and free.
        let img = [200u8, 50, 90, 255];
        let out = render_offscreen_color(&img, 1, 1, 4, 4, ColorTransform::srgb());
        assert!(
            close(at(&out, 4, 2, 2), [200, 50, 90, 255], 1),
            "passthrough altered the pixel: {:?}",
            at(&out, 4, 2, 2)
        );
    }

    #[test]
    fn enabled_color_transform_applies_curve_and_reencode() {
        let _gpu = crate::gpu_test_lock();
        // Identity primaries + a gamma-2.0 source curve, re-encoded to sRGB: the
        // shader must linearize (x^2), pass the matrix (identity), then sRGB-encode.
        let g = 2.0f32;
        let color = ColorTransform {
            matrix: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            trc: [g, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            enabled: true,
        };
        let img = [128u8, 128, 128, 255];
        let out = render_offscreen_color(&img, 1, 1, 4, 4, color);
        let got = at(&out, 4, 2, 2);

        let lin = (128.0f32 / 255.0).powf(g);
        let exp = (srgb_oetf_ref(lin) * 255.0).round() as u8;
        assert!(
            close(got, [exp, exp, exp, 255], 2),
            "got {got:?}, expected ~{exp}"
        );
        // And it must actually differ from the input — proof the path ran.
        assert!(
            (got[0] as i32 - 128).abs() > 3,
            "conversion was a no-op: {got:?}"
        );
    }

    #[test]
    fn test_pattern_is_well_formed() {
        let p = test_pattern(64, 40);
        assert_eq!(p.len(), 64 * 40 * 4);
        // top-left marker is red-ish
        assert_eq!(&p[0..4], &[220, 30, 30, 255]);
    }
    #[test]
    fn clamp_leaves_fitting_images_untouched() {
        let img = vec![5u8; 4 * 4 * 4];
        let (out, w, h) = clamp_to_max(&img, 4, 4, 8);
        assert_eq!((w, h), (4, 4));
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
        assert_eq!(out.len(), img.len());
    }

    #[test]
    fn clamp_downscales_oversized_preserving_aspect() {
        let (w, h) = (20u32, 10u32);
        let img = vec![7u8; (w * h * 4) as usize];
        let (out, ow, oh) = clamp_to_max(&img, w, h, 8);
        assert!(ow <= 8 && oh <= 8, "got {ow}x{oh}");
        assert_eq!((ow, oh), (8, 4)); // 2:1 aspect preserved
        assert_eq!(out.len(), (ow * oh * 4) as usize);
    }

    #[test]
    fn original_mode_is_one_to_one_centered() {
        let view = ViewTransform {
            mode: crate::ScaleMode::Original,
            ..Default::default()
        };
        // Image exactly the screen size -> full-screen quad (top-left at -1,+1).
        let v = quad_vertices(&view, 100, 100, 100, 100, 0);
        assert!((v[0].pos[0] + 1.0).abs() < 1e-5, "x0 = {}", v[0].pos[0]);
        assert!((v[0].pos[1] - 1.0).abs() < 1e-5, "y_top = {}", v[0].pos[1]);

        // Image half the screen -> centered, top-left at -0.5,+0.5.
        let v = quad_vertices(&view, 50, 50, 100, 100, 0);
        assert!((v[0].pos[0] + 0.5).abs() < 1e-5, "x0 = {}", v[0].pos[0]);
        assert!((v[0].pos[1] - 0.5).abs() < 1e-5, "y_top = {}", v[0].pos[1]);
    }
}
