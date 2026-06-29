//! SPIKE: probe the display's HDR / wide-gamut capabilities via DXGI.
//!
//! Decides the native present path's output color space (scRGB fp16 vs HDR10) by
//! reporting, per monitor: whether the desktop is in HDR mode, the panel's gamut
//! primaries (→ sRGB / P3 / BT.2020 coverage), peak luminance (nits), and bit
//! depth. Windowless — no swapchain, just `IDXGIOutput6::GetDesc1`.
//!
//!   cargo run -q --example hdr_probe -p pb-render

#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    };
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6};

    /// Rough gamut label from the green primary x,y (the most distinguishing one).
    fn gamut(green: [f32; 2]) -> &'static str {
        let (x, y) = (green[0], green[1]);
        if (x - 0.17).abs() < 0.05 && y > 0.75 {
            "~BT.2020 (very wide)"
        } else if x < 0.28 && y > 0.66 {
            "~Display-P3 (wide)"
        } else if (x - 0.30).abs() < 0.04 && (y - 0.60).abs() < 0.04 {
            "~sRGB / BT.709"
        } else {
            "non-standard"
        }
    }

    /// Triangle area of the RGB primaries — a coarse gamut-size proxy (sRGB ≈ 0.112).
    fn gamut_area(r: [f32; 2], g: [f32; 2], b: [f32; 2]) -> f32 {
        0.5 * ((g[0] - r[0]) * (b[1] - r[1]) - (b[0] - r[0]) * (g[1] - r[1])).abs()
    }

    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut found = false;
        let mut ai = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            let ad = adapter.GetDesc1()?;
            let name = String::from_utf16_lossy(&ad.Description);
            println!("Adapter {ai}: {}", name.trim_end_matches('\0'));
            let mut oi = 0u32;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                oi += 1;
                let Ok(o6) = output.cast::<IDXGIOutput6>() else {
                    println!("  Output {}: no IDXGIOutput6 (pre-Win10?)", oi - 1);
                    continue;
                };
                let d = o6.GetDesc1()?;
                found = true;
                let dev = String::from_utf16_lossy(&d.DeviceName);
                let hdr_on = d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
                let cs = if hdr_on {
                    "HDR10 — desktop HDR is ON"
                } else if d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709 {
                    "SDR sRGB — desktop HDR is OFF"
                } else {
                    "other"
                };
                let area = gamut_area(d.RedPrimary, d.GreenPrimary, d.BluePrimary);
                println!("  Output: {}", dev.trim_end_matches('\0'));
                println!("    Desktop color space : {cs} (raw {})", d.ColorSpace.0);
                println!("    Bits per color      : {}", d.BitsPerColor);
                println!(
                    "    Panel gamut         : {}  (area {:.3}, sRGB≈0.112, P3≈0.152, 2020≈0.212)",
                    gamut(d.GreenPrimary),
                    area
                );
                println!(
                    "    Primaries xy        : R{:?} G{:?} B{:?} W{:?}",
                    d.RedPrimary, d.GreenPrimary, d.BluePrimary, d.WhitePoint
                );
                println!(
                    "    Luminance (nits)    : min {:.4}, max {:.0}, max-full-frame {:.0}",
                    d.MinLuminance, d.MaxLuminance, d.MaxFullFrameLuminance
                );
            }
            ai += 1;
        }
        if !found {
            println!("(no outputs reported IDXGIOutput6)");
        }
    }
    Ok(())
}

/// macOS: enumerate every `NSScreen` and report its EDR headroom + P3 gamut, then
/// show what `display::primary_hdr()` (the value the renderer actually uses) decides.
#[cfg(target_os = "macos")]
fn main() {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{class, msg_send};
    use std::ffi::{c_char, CStr};

    unsafe {
        let screens: Retained<AnyObject> = msg_send![class!(NSScreen), screens];
        let count: usize = msg_send![&*screens, count];
        println!("{count} display(s):\n");
        for i in 0..count {
            let screen: Retained<AnyObject> = msg_send![&*screens, objectAtIndex: i];
            let s = &*screen;
            let max_edr: f64 =
                msg_send![s, maximumPotentialExtendedDynamicRangeColorComponentValue];
            let cur_edr: f64 = msg_send![s, maximumExtendedDynamicRangeColorComponentValue];
            let p3: Bool = msg_send![s, canRepresentDisplayGamut: 1isize];
            let name_obj: Retained<AnyObject> = msg_send![s, localizedName];
            let utf8: *const c_char = msg_send![&*name_obj, UTF8String];
            let name = CStr::from_ptr(utf8).to_string_lossy();

            let gamut = if p3.as_bool() { "P3 (wide)" } else { "sRGB" };
            let hdr = if max_edr > 1.01 {
                format!(
                    "EDR ×{max_edr:.1} (≈{:.0} nits, currently ×{cur_edr:.1})",
                    max_edr * 100.0
                )
            } else {
                "SDR (no EDR headroom)".to_string()
            };
            println!("  [{i}] {name}\n      gamut: {gamut}\n      hdr:   {hdr}\n");
        }
    }

    let d = pb_render::display::primary_hdr();
    println!("primary_hdr() → the renderer will use:");
    println!(
        "  hdr_on={}  wide_gamut={}  max_nits={:.0}  sdr_white_nits={:.0}",
        d.hdr_on, d.wide_gamut, d.max_nits, d.sdr_white_nits
    );
    println!(
        "  → {} surface",
        if d.hdr_on || d.wide_gamut {
            "fp16 wide-gamut/EDR"
        } else {
            "SDR 8-bit"
        }
    );
}

#[cfg(not(any(windows, target_os = "macos")))]
fn main() {
    println!("hdr_probe needs DXGI (Windows) or NSScreen (macOS).");
}
