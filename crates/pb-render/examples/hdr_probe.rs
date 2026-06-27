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

#[cfg(not(windows))]
fn main() {
    println!("hdr_probe is Windows-only (DXGI). On macOS the equivalent is CAMetalLayer EDR.");
}
