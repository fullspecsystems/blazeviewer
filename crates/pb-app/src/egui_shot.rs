//! Hidden dev command (`--egui-shot [out.png]`): render the egui rich panels (Help /
//! Inspector / folder tree) **headlessly** to a PNG, composited over a gray gradient the
//! way they sit over a photo — the egui-overlay equivalent of `--hud-gallery`. Lets the
//! panels be previewed and iterated without the event loop or a live window (screen
//! capture is unreliable on HDR / borderless-Metal surfaces). Dev-only; never a user path.
//!
//! The readback is faithful to the real pipeline: egui renders premultiplied into an
//! `Rgba8UnormSrgb` texture (the exact format [`crate::egui_overlay`] uses), and the
//! software composite here linearizes, blends premultiplied-over the background, and
//! re-encodes sRGB — mirroring `pb-render`'s `fs_egui` pass + tone-map, so what the PNG
//! shows is what the app composites.

use std::path::Path;

use egui_wgpu::wgpu;
use pb_app_core::fs_tree;
use pb_app_core::panels::{
    DescribeBody, DescribePanel, DetailRow, DetailsPanel, HelpPanel, HelpSection,
    InspectorSnapshot, TextBody, TextPanel,
};
use pb_app_core::InspectorTab;
use resvg::tiny_skia;

use crate::panels_ui::{self, InspectorFrame, PanelFrame, TreeFrame};

/// Render the panels to `out` as a PNG. `dark` picks the theme; `tab` the Inspector tab.
/// `welcome` renders the empty-state welcome screen alone instead of the full panel set.
pub fn write_shot(out: &Path, dark: bool, tab: InspectorTab, welcome: bool) -> Result<(), String> {
    pollster::block_on(run(out, dark, tab, welcome))
}

/// Render a **Settings** tab's card stack to `out` as a PNG (`--settings-shot`) — the
/// Settings equivalent of [`write_shot`], for previewing the dialog's cards headlessly
/// (screen capture is unreliable on this host). `appearance` picks the Appearance tab over
/// General. The page fills its own opaque background (`Palette::page` via a `CentralPanel`),
/// so — unlike the panel shot — there's nothing to composite: the readback is written straight.
pub fn write_settings_shot(out: &Path, dark: bool, tab: &str) -> Result<(), String> {
    pollster::block_on(run_settings(out, dark, tab))
}

async fn run_settings(out: &Path, dark: bool, tab: &str) -> Result<(), String> {
    let ppp = 2.0f32;
    // 560pt wide = the real Settings window; tall enough that the General tab lays out
    // without scrolling (cropped afterward).
    let (w, h) = (1120u32, 2600u32);

    let instance = wgpu::Instance::default();
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions::default())
        .await
        .ok_or("no wgpu adapter")?;
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor::default(), None)
        .await
        .map_err(|e| e.to_string())?;

    let ctx = egui::Context::default();
    ctx.set_theme(if dark {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    });
    pb_ui::install_fonts(&ctx);
    pb_ui::apply_style(&ctx, dark);
    ctx.set_pixels_per_point(ppp);
    let mut renderer =
        egui_wgpu::Renderer::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb, None, 1, false);

    let page = pb_ui::Palette::new(dark).page;
    let build = |ctx: &egui::Context| {
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(page))
            .show(ctx, |ui| {
                crate::dialog::settings_shot_body(ui, dark, tab);
            });
    };
    let raw = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(w as f32 / ppp, h as f32 / ppp),
        )),
        ..Default::default()
    };
    // Two warm-up frames settle the responsive card_row layout at the real width (the first
    // run reports a default screen_rect); the third is the one we read back.
    for _ in 0..2 {
        let warm = ctx.run(raw(), build);
        for (id, d) in &warm.textures_delta.set {
            renderer.update_texture(&device, &queue, *id, d);
        }
    }
    let full = ctx.run(raw(), build);
    let jobs = ctx.tessellate(full.shapes, full.pixels_per_point);
    let desc = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [w, h],
        pixels_per_point: ppp,
    };
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("settings-shot"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    for (id, d) in &full.textures_delta.set {
        renderer.update_texture(&device, &queue, *id, d);
    }
    let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("settings-shot"),
    });
    let cmd = renderer.update_buffers(&device, &queue, &mut enc, &jobs, &desc);
    {
        let mut rp = enc
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("settings-shot"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // The CentralPanel fills the opaque page, so a transparent clear is fine.
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            })
            .forget_lifetime();
        renderer.render(&mut rp, &jobs, &desc);
    }

    let bpp = 4u32;
    let padded = (w * bpp).div_ceil(256) * 256;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("settings-shot-readback"),
        size: (padded * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    enc.copy_texture_to_buffer(
        wgpu::ImageCopyTexture {
            texture: &tex,
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
    queue.submit(cmd.into_iter().chain(std::iter::once(enc.finish())));

    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device.poll(wgpu::Maintain::Wait);
    rx.recv()
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:?}"))?;
    let data = slice.get_mapped_range();

    // Opaque page → the sRGB readback is the final image; just drop the row padding + force
    // alpha (any 1px transparent seam at the very edge reads as page).
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let si = (y * padded + x * bpp) as usize;
            let di = ((y * w + x) * 4) as usize;
            px[di] = data[si];
            px[di + 1] = data[si + 1];
            px[di + 2] = data[si + 2];
            px[di + 3] = 255;
        }
    }
    drop(data);
    readback.unmap();

    let size = tiny_skia::IntSize::from_wh(w, h).ok_or("invalid size")?;
    let pixmap = tiny_skia::Pixmap::from_vec(px, size).ok_or("could not build pixmap")?;
    let png = pixmap
        .encode_png()
        .map_err(|e| format!("PNG encode: {e}"))?;
    std::fs::write(out, png).map_err(|e| e.to_string())?;
    Ok(())
}

async fn run(out: &Path, dark: bool, tab: InspectorTab, welcome: bool) -> Result<(), String> {
    let ppp = 2.0f32;
    let (w, h) = (2400u32, 1400u32); // physical pixels

    // Headless device (the golden-test path).
    let instance = wgpu::Instance::default();
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions::default())
        .await
        .ok_or("no wgpu adapter")?;
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor::default(), None)
        .await
        .map_err(|e| e.to_string())?;

    // egui context + design system (same setup the overlay uses).
    let ctx = egui::Context::default();
    ctx.set_theme(if dark {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    });
    pb_ui::install_fonts(&ctx);
    pb_ui::apply_style(&ctx, dark);
    ctx.set_pixels_per_point(ppp);
    let mut renderer =
        egui_wgpu::Renderer::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb, None, 1, false);

    let frame = sample_frame(dark, tab, welcome);
    let mut actions = Vec::new();
    let raw = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(w as f32 / ppp, h as f32 / ppp),
        )),
        ..Default::default()
    };
    // egui is immediate-mode: the first frame lays out (Window sizes/positions settle);
    // render the second so anchored panels are placed. (The dialog window primes two
    // frames for the same reason.) The first frame also emits the font-atlas texture
    // delta, so its `textures_delta` must be applied — else egui's draws reference a
    // texture the renderer never got and render nothing.
    // Two warm-up frames: the first run reports a bogus default `screen_rect`, so the
    // anchored windows would cache a wrong size; a second settles them at the real size.
    for _ in 0..2 {
        let warm = ctx.run(raw(), |ctx| panels_ui::build(ctx, &frame, &mut actions));
        for (id, d) in &warm.textures_delta.set {
            renderer.update_texture(&device, &queue, *id, d);
        }
    }
    let full = ctx.run(raw(), |ctx| panels_ui::build(ctx, &frame, &mut actions));
    let jobs = ctx.tessellate(full.shapes, full.pixels_per_point);
    let desc = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [w, h],
        pixels_per_point: ppp,
    };

    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("egui-shot"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    for (id, d) in &full.textures_delta.set {
        renderer.update_texture(&device, &queue, *id, d);
    }
    let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("egui-shot"),
    });
    let cmd = renderer.update_buffers(&device, &queue, &mut enc, &jobs, &desc);
    {
        let mut rp = enc
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui-shot"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            })
            .forget_lifetime();
        renderer.render(&mut rp, &jobs, &desc);
    }

    // Read the texture back (rows padded to 256 bytes).
    let bpp = 4u32;
    let padded = (w * bpp).div_ceil(256) * 256;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("egui-shot-readback"),
        size: (padded * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    enc.copy_texture_to_buffer(
        wgpu::ImageCopyTexture {
            texture: &tex,
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
    queue.submit(cmd.into_iter().chain(std::iter::once(enc.finish())));

    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device.poll(wgpu::Maintain::Wait);
    rx.recv()
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:?}"))?;
    let data = slice.get_mapped_range();

    // Composite premultiplied-over a diagonal gray gradient, in linear light (the
    // pipeline: sRGB-decode → blend → sRGB-encode). Opaque output → premultiplied ==
    // straight, which is what tiny_skia's Pixmap wants.
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let si = (y * padded + x * bpp) as usize;
            let (pr, pg, pb, pa) = (
                srgb_to_lin(data[si]),
                srgb_to_lin(data[si + 1]),
                srgb_to_lin(data[si + 2]),
                data[si + 3] as f32 / 255.0,
            );
            let t = (x as f32 / w as f32 + y as f32 / h as f32) * 0.5;
            let bg = srgb_to_lin((60.0 + t * 90.0) as u8);
            let out = |p: f32| lin_to_srgb(p + bg * (1.0 - pa));
            let di = ((y * w + x) * 4) as usize;
            px[di] = out(pr);
            px[di + 1] = out(pg);
            px[di + 2] = out(pb);
            px[di + 3] = 255;
        }
    }
    drop(data);
    readback.unmap();

    let size = tiny_skia::IntSize::from_wh(w, h).ok_or("invalid size")?;
    let pixmap = tiny_skia::Pixmap::from_vec(px, size).ok_or("could not build pixmap")?;
    let png = pixmap
        .encode_png()
        .map_err(|e| format!("PNG encode: {e}"))?;
    std::fs::write(out, png).map_err(|e| e.to_string())?;
    Ok(())
}

fn srgb_to_lin(u: u8) -> f32 {
    let x = u as f32 / 255.0;
    if x <= 0.04045 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

fn lin_to_srgb(x: f32) -> u8 {
    let x = x.clamp(0.0, 1.0);
    let s = if x <= 0.0031308 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round() as u8
}

/// Representative panel data so every panel + row kind shows.
fn sample_frame(dark: bool, tab: InspectorTab, welcome: bool) -> PanelFrame {
    // Welcome preview: the empty-state buttons alone (no photo, no other panels).
    if welcome {
        return PanelFrame {
            help: None,
            inspector: None,
            tree: None,
            info: None,
            scan: None,
            welcome: Some(panels_ui::WelcomePanel {
                file_key: "O".into(),
                folder_key: "\u{21e7}O".into(),
            }),
            dark,
            panel_alpha: 242,
        };
    }
    let help = HelpPanel {
        sections: vec![
            HelpSection {
                title: "Navigation".into(),
                rows: vec![
                    ("Next photo".into(), "Space / →".into()),
                    ("Previous photo".into(), "⌫ / ←".into()),
                    ("Random photo".into(), "Enter".into()),
                    ("Quit".into(), "Esc".into()),
                ],
            },
            HelpSection {
                title: "View & Zoom".into(),
                rows: vec![
                    ("Zoom out / in".into(), "- / =".into()),
                    ("Pan".into(), "\u{2190} \u{2191} \u{2193} \u{2192}".into()),
                    // Chords (mac thin-space glyphs) — should each render as ONE keycap.
                    ("Rotate right / left".into(), "R / \u{21e7}\u{2009}R".into()),
                    ("Flip / pin compare".into(), "Y / \u{21e7}\u{2009}Y".into()),
                    ("Slideshow faster / slower".into(), "[ / ]".into()),
                    ("Detailed info panel".into(), "\u{2318}\u{2009}I".into()),
                ],
            },
        ],
    };

    let snapshot = match tab {
        InspectorTab::Details => InspectorSnapshot::Details(DetailsPanel {
            rows: vec![
                DetailRow::Span {
                    text: "84F7A1BB-5877-49C7.jpeg".into(),
                    bold: true,
                },
                DetailRow::Pair {
                    label: "Dimensions".into(),
                    value: "6048 × 4024".into(),
                },
                DetailRow::Pair {
                    label: "Codec".into(),
                    value: "HEIC → JPEG".into(),
                },
                DetailRow::Pair {
                    label: "Camera".into(),
                    value: "Apple iPhone 15 Pro".into(),
                },
                DetailRow::Pair {
                    label: "Lens".into(),
                    value: "Main Camera — 6.86mm f/1.78".into(),
                },
                DetailRow::Span {
                    text: "Exposure".into(),
                    bold: true,
                },
                DetailRow::Pair {
                    label: "Shutter".into(),
                    value: "1/120 s".into(),
                },
                DetailRow::Pair {
                    label: "ISO".into(),
                    value: "64".into(),
                },
                // Medium-long labels that should now fit on one line at the wider column.
                DetailRow::Pair {
                    label: "DateTimeDigitized".into(),
                    value: "2005:09:21 13:58:57".into(),
                },
                DetailRow::Pair {
                    label: "MaxApertureValue".into(),
                    value: "2 EV".into(),
                },
                DetailRow::Pair {
                    label: "ExposureBiasValue".into(),
                    value: "0 EV".into(),
                },
                DetailRow::Pair {
                    label: "PhotographicSensitivity".into(),
                    value: "64".into(),
                },
                DetailRow::Pair {
                    label: "FocalLengthIn35mmFilm".into(),
                    value: "24 mm".into(),
                },
                // A long value (wraps) + a long label (wraps in its column) — the #3 blow-out.
                DetailRow::Pair {
                    label: "Flash".into(),
                    value: "not fired, no return light detection function, auto mode 0 (unknown)"
                        .into(),
                },
                DetailRow::Pair {
                    label: "ComponentsConfiguration".into(),
                    value: "YCbCr".into(),
                },
            ],
        }),
        InspectorTab::Text => InspectorSnapshot::Text(TextPanel {
            body: TextBody::Ready {
                qr: vec!["https://photoblaze.app".into()],
                paragraphs: vec![
                    "OPEN HOUSE".into(),
                    "Saturday 1–4 PM · 123 Condo Lane".into(),
                ],
                ocr_error: None,
            },
        }),
        InspectorTab::Describe => InspectorSnapshot::Describe(DescribePanel {
            body: DescribeBody::Ready(
                "Here are some interesting details from the image:\n\n\
                 1. **Bed Design**: The bed features a unique black headboard with rectangular \
                 panels, creating a geometric and modern look.\n\
                 2. **Color Contrast**: The yellow pillow adds a vibrant pop of color against \
                 the neutral tones of the bedding (gray with floral patterns).\n\
                 3. **Lighting**: A tall floor lamp with an arched neck provides warm lighting \
                 to the room, enhancing its cozy atmosphere.\n\n\
                 These details collectively create a balanced, cozy bedroom design."
                    .into(),
            ),
        }),
    };
    let inspector = InspectorFrame { tab, snapshot };

    let row = |name: &str, depth: u32, cur: bool, exp: bool, kids: bool, count: Option<u64>| {
        fs_tree::Row {
            path: format!("/Users/jdlien/Pictures/{name}").into(),
            name: name.into(),
            depth,
            expanded: exp,
            loading: false,
            has_children: kids,
            is_current: cur,
            count,
        }
    };
    // Mirrors a real deck: nested years, long dated folder names (truncation), and a deep
    // current folder — enough rows to overflow so scroll/height show.
    let tree = TreeFrame {
        parent_name: Some("jdlien".into()),
        rows: vec![
            row("Pictures", 0, false, true, true, None),
            row("$RECYCLE.BIN", 1, false, false, true, None),
            row("1990s", 1, false, true, true, None),
            row(
                "1990-12-24 - Christmas1990",
                2,
                false,
                false,
                false,
                Some(11),
            ),
            row(
                "1991-09-20 - Grade 5 School Photos",
                2,
                false,
                false,
                false,
                Some(1),
            ),
            row(
                "1999-06-01 - CCHS Grad 1999 Ceremony",
                2,
                true,
                false,
                false,
                Some(1),
            ),
            row("2000", 1, false, false, true, Some(842)),
            row("2001", 1, false, false, true, Some(1203)),
            row("2002", 1, false, false, true, Some(978)),
            row("2003", 1, false, false, true, Some(1544)),
            row("2004", 1, false, false, true, Some(2011)),
            row("2005", 1, false, false, true, Some(1766)),
            row("2006", 1, false, false, true, Some(2450)),
            row("2007", 1, false, false, true, Some(1889)),
            row("2008", 1, false, false, true, Some(3102)),
            row("2009", 1, false, false, true, Some(2733)),
            row("2010", 1, false, false, true, Some(1998)),
            row("2011", 1, false, false, true, Some(2560)),
            row("2012", 1, false, false, true, Some(1580)),
            row("2013", 1, false, false, true, Some(3989)),
            row("Screenshots", 1, false, false, false, Some(27)),
        ],
    };

    PanelFrame {
        help: Some(help),
        inspector: Some(inspector),
        tree: Some(tree),
        info: Some(panels_ui::InfoLine {
            main: "1990s/1990-12-24 · 6048 × 4024".into(),
            codec: "HEIC → JPEG".into(),
            is_live: true,
            is_animated: false,
            align: pb_app_core::settings::InfoLineAlign::Right,
        }),
        // The ambient scan pill (top-center) — a long sub-folder to exercise truncation.
        scan: Some(panels_ui::ScanPill {
            name: "Pictures".into(),
            found: 1232945,
            current: "2013/2013-08-14 - Summer Road Trip Photos and Videos".into(),
        }),
        welcome: None,
        dark,
        panel_alpha: 242, // ≈95% — the shot previews the panels near-opaque
    }
}
