//! Our own dialog windows (Settings, About), rendered with **egui** in a second
//! winit window with its own small wgpu surface.
//!
//! Why ours instead of a native dialog: a native Win32 TaskDialog can't show a
//! large custom icon or follow the OS dark theme. egui gives both for free (and
//! ports to macOS later). The dialog only runs while open — off the photo hot path.
//! egui follows the OS light/dark setting via `ThemePreference::System`, matching
//! the native menu + title bar.

use std::sync::Arc;

use egui_wgpu::wgpu;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};

use pb_decode::{decode_bytes, FitBox};

use crate::icon::assets as icon_assets;

/// Which dialog a [`DialogWindow`] is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    About,
    Settings,
    /// A themed Yes/No confirmation (e.g. permanent delete). The prompt text is
    /// carried on the [`DialogWindow`]; the answer surfaces via
    /// [`DialogWindow::take_confirm_result`].
    Confirm,
    /// A one-button informational / error notice (e.g. an archive-open failure):
    /// a warning icon + the body text + a single OK button. The body is carried on
    /// the [`DialogWindow`]; OK / Esc close it (OK surfaces as
    /// [`take_confirm_result`] `Some(true)`). Opened via [`crate::App::open_message`].
    ///
    /// [`take_confirm_result`]: DialogWindow::take_confirm_result
    Message,
}

/// Skeleton state for the Settings form. Not yet wired to persistence or live
/// apply — it just drives the controls so we can see the layout. Defaults mirror
/// the current in-code values (Task 19/20 etc.).
struct SettingsDraft {
    refresh_hz: u32,
    start_speed: f32,
    ramp_secs: f32,
    max_fps: u32,
    hold_delay_ms: u32,
    recursive: bool,
    scale_mode: usize, // 0 = Fit, 1 = Fill, 2 = Original
    letterbox: [f32; 3],
    start_fullscreen: bool,
}

impl SettingsDraft {
    fn new(refresh_hz: u32) -> Self {
        let hz = refresh_hz.max(1);
        Self {
            refresh_hz: hz,
            start_speed: 3.0,
            ramp_secs: 4.0,
            max_fps: hz,
            hold_delay_ms: 400,
            recursive: true,
            scale_mode: 0,
            letterbox: [0.05, 0.05, 0.06],
            start_fullscreen: false,
        }
    }
}

/// A second OS window rendering an egui dialog (its own wgpu device/surface).
pub struct DialogWindow {
    window: Arc<Window>,
    kind: DialogKind,
    // wgpu (kept alive together; `_instance` must outlive the surface).
    _instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    // egui
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
    icon: Option<egui::TextureHandle>,
    /// Confirm-dialog warning-triangle icon (sits inline with the "cannot be undone"
    /// line); `None` for other kinds.
    warn_icon: Option<egui::TextureHandle>,
    draft: SettingsDraft,
    /// The prompt for a [`DialogKind::Confirm`] dialog (unused otherwise).
    confirm_msg: String,
    /// Set when the user answers a Confirm dialog: `Some(true)` = confirmed,
    /// `Some(false)` = cancelled. Polled + cleared by [`take_confirm_result`].
    ///
    /// [`take_confirm_result`]: DialogWindow::take_confirm_result
    confirm_result: Option<bool>,
}

impl DialogWindow {
    /// Create and show the dialog window, centered over `parent` (the main viewer
    /// window) when given. `refresh_hz` caps the Settings "max photos/sec" slider.
    /// Returns `None` if window/GPU setup fails (best-effort — a failed dialog must
    /// never take down the viewer).
    pub fn open(
        kind: DialogKind,
        event_loop: &ActiveEventLoop,
        refresh_hz: u32,
        message: &str,
        parent: Option<&Window>,
    ) -> Option<DialogWindow> {
        let (w, h, resizable, title) = match kind {
            DialogKind::About => (254.0, 307.0, false, "About PhotoBlaze"),
            DialogKind::Settings => (560.0, 660.0, true, "PhotoBlaze Settings"),
            DialogKind::Confirm => (450.0, 172.0, false, "Confirm Delete"),
            DialogKind::Message => (470.0, 185.0, false, "PhotoBlaze"),
        };
        // Created HIDDEN: we render the first (themed) frame before revealing, so the
        // OS never flashes the default white window before our dark frame lands.
        let mut attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(LogicalSize::new(w, h))
            .with_resizable(resizable)
            .with_visible(false);
        // Center over the parent's outer rect (so it lands on the viewer, not the OS
        // cascade position). Falls back to the default position if it can't be read.
        if let Some(p) = parent {
            if let Ok(ppos) = p.outer_position() {
                let psize = p.outer_size();
                let scale = p.scale_factor();
                let (dw, dh) = (w * scale, h * scale);
                let x = ppos.x as f64 + (psize.width as f64 - dw) / 2.0;
                let y = ppos.y as f64 + (psize.height as f64 - dh) / 2.0;
                attrs = attrs.with_position(PhysicalPosition::new(x, y));
            }
        }
        let window = Arc::new(event_loop.create_window(attrs).ok()?);
        let size = window.inner_size();

        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone()).ok()?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
                .ok()?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let egui_ctx = egui::Context::default();
        // Follow the OS light/dark setting (egui-winit feeds the system theme).
        egui_ctx.set_theme(egui::ThemePreference::System);
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        let egui_renderer = egui_wgpu::Renderer::new(&device, format, None, 1, false);
        let icon = load_icon_texture(&egui_ctx);
        // The amber warning triangle (rasterized large for crisp scaling): shown
        // inline on the Confirm dialog and as the lead icon on a Message notice.
        let warn_icon = if matches!(kind, DialogKind::Confirm | DialogKind::Message) {
            svg_texture(
                &egui_ctx,
                icon_assets::WARNING,
                72,
                [232, 172, 46],
                "dialog-warn",
            )
        } else {
            None
        };

        let mut dlg = DialogWindow {
            window,
            kind,
            _instance: instance,
            surface,
            device,
            queue,
            config,
            egui_ctx,
            egui_state,
            egui_renderer,
            icon,
            warn_icon,
            draft: SettingsDraft::new(refresh_hz),
            confirm_msg: message.to_string(),
            confirm_result: None,
        };
        // Paint the first themed frame while hidden, then reveal — no white flash.
        dlg.render();
        dlg.window.set_visible(true);
        // Grab keyboard focus so Esc / Enter act on the dialog (not the viewer, which
        // would otherwise treat Esc as quit).
        dlg.window.focus_window();
        dlg.window.request_redraw();
        Some(dlg)
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    pub fn kind(&self) -> DialogKind {
        self.kind
    }

    /// Take the answer to a [`DialogKind::Confirm`] dialog, if the user has answered
    /// this frame: `Some(true)` = confirmed, `Some(false)` = cancelled. `None` until
    /// a button is clicked. The caller closes the dialog and acts on the result.
    pub fn take_confirm_result(&mut self) -> Option<bool> {
        self.confirm_result.take()
    }

    pub fn focus(&self) {
        self.window.focus_window();
    }

    pub fn request_redraw(&self) {
        self.window.request_redraw();
    }

    /// Feed a winit event to egui; returns whether egui wants a repaint.
    pub fn on_event(&mut self, event: &WindowEvent) -> bool {
        self.egui_state.on_window_event(&self.window, event).repaint
    }

    pub fn resize(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Run egui for one frame and present it.
    pub fn render(&mut self) {
        let raw_input = self.egui_state.take_egui_input(&self.window);
        let ctx = self.egui_ctx.clone();
        let kind = self.kind;
        let icon = self.icon.clone();
        let warn_icon = self.warn_icon.clone();
        let msg = self.confirm_msg.clone();
        let draft = &mut self.draft;
        let mut confirm_click: Option<bool> = None;
        let full_output = ctx.run(raw_input, |ctx| match kind {
            DialogKind::About => {
                egui::CentralPanel::default().show(ctx, |ui| about_ui(ui, icon.as_ref()));
            }
            DialogKind::Settings => {
                egui::CentralPanel::default().show(ctx, |ui| settings_ui(ui, draft));
            }
            DialogKind::Confirm => {
                confirm_click = confirm_dialog(ctx, &msg, warn_icon.as_ref());
            }
            DialogKind::Message => {
                confirm_click = message_dialog(ctx, &msg, warn_icon.as_ref());
            }
        });
        if confirm_click.is_some() {
            self.confirm_result = confirm_click;
        }

        self.egui_state
            .handle_platform_output(&self.window, full_output.platform_output);
        let paint_jobs = self
            .egui_ctx
            .tessellate(full_output.shapes, full_output.pixels_per_point);
        let desc = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point: full_output.pixels_per_point,
        };
        for (id, delta) in &full_output.textures_delta.set {
            self.egui_renderer
                .update_texture(&self.device, &self.queue, *id, delta);
        }

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("egui"),
            });
        let cmd_bufs = self.egui_renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &paint_jobs,
            &desc,
        );

        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(_) => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        {
            let mut rpass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui-dialog"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            // egui's CentralPanel fills the window, so this is just a
                            // backstop clear.
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .forget_lifetime();
            self.egui_renderer.render(&mut rpass, &paint_jobs, &desc);
        }
        self.queue.submit(
            cmd_bufs
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        frame.present();
        for id in &full_output.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }
    }
}

/// Decode the embedded app icon into an egui texture (for the About card).
fn load_icon_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    const PNG: &[u8] = include_bytes!("../icons/photoblaze.png");
    let fit = FitBox {
        max_width: 256,
        max_height: 256,
    };
    let img = decode_bytes(PNG, Some(fit), false).ok()?;
    let color = egui::ColorImage::from_rgba_unmultiplied(
        [img.width as usize, img.height as usize],
        &img.pixels,
    );
    Some(ctx.load_texture("about-icon", color, egui::TextureOptions::LINEAR))
}

/// The About card: big centered icon, name, version, tagline, copyright, link.
fn about_ui(ui: &mut egui::Ui, icon: Option<&egui::TextureHandle>) {
    ui.vertical_centered(|ui| {
        ui.add_space(20.0);
        if let Some(tex) = icon {
            ui.image(egui::load::SizedTexture::new(
                tex.id(),
                egui::vec2(100.0, 100.0),
            ));
        }
        ui.add_space(12.0);
        ui.heading("PhotoBlaze");
        ui.add_space(2.0);
        ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
        ui.add_space(10.0);
        ui.label("An ultra-fast photo viewer");
        ui.add_space(8.0);
        ui.label("\u{00a9} JD Lien 2026");
        ui.add_space(12.0);
        ui.hyperlink_to(
            "github.com/jdlien/photoblaze",
            "https://github.com/jdlien/photoblaze",
        );
    });
}

/// Uniform inset for dialog content — the same value drives the text margins, the
/// gap below the divider, and the gap to the bottom/right edges, so the padding
/// reads as balanced on every side (Confirm + Message both use it).
const DIALOG_PAD: f32 = 22.0;

/// A panel frame filled to match the window background, inset by `DIALOG_PAD` on
/// all sides. Used for both the content panel and the bottom button bar so their
/// padding lines up.
fn dialog_frame(ctx: &egui::Context) -> egui::Frame {
    egui::Frame::default()
        .fill(ctx.style().visuals.panel_fill)
        .inner_margin(egui::Margin::same(DIALOG_PAD))
}

/// A themed confirm dialog modelled on Directory Opus's "Confirm File Delete": the
/// prompt + a ⚠ "cannot be undone" line, and a bottom-right button bar with a
/// prominent red Delete (default-focused) and Cancel. Returns `Some(true)` on
/// Delete, `Some(false)` on Cancel, else `None`. (Esc / the window close button also
/// cancel it, from the event router.) The shared `DIALOG_PAD` margin balances the
/// spacing around the text and around the buttons (equal to the divider + edges).
fn confirm_dialog(
    ctx: &egui::Context,
    message: &str,
    warn_icon: Option<&egui::TextureHandle>,
) -> Option<bool> {
    let mut result = None;
    // Bottom button bar (auto-height = button + the uniform margin above/below).
    egui::TopBottomPanel::bottom("confirm_bar")
        .frame(dialog_frame(ctx))
        .show(ctx, |ui| {
            // Right-to-left: Cancel added first (rightmost), Delete to its left — so
            // the visual order is [Delete] [Cancel], matching Directory Opus.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_sized([100.0, 32.0], egui::Button::new("Cancel"))
                    .clicked()
                {
                    result = Some(false);
                }
                ui.add_space(12.0);
                let delete =
                    egui::Button::new(egui::RichText::new("Delete").color(egui::Color32::WHITE))
                        .fill(egui::Color32::from_rgb(200, 55, 55));
                let resp = ui.add_sized([100.0, 32.0], delete);
                if resp.clicked() {
                    result = Some(true);
                }
                // Default focus on Delete (matches Directory Opus): Enter confirms.
                if ui.memory(|m| m.focused().is_none()) {
                    resp.request_focus();
                }
            });
        });
    // Message + ⚠ line, left-aligned in the area above the button bar.
    egui::CentralPanel::default()
        .frame(dialog_frame(ctx))
        .show(ctx, |ui| {
            ui.label(egui::RichText::new(message).size(16.0));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if let Some(t) = warn_icon {
                    ui.image(egui::load::SizedTexture::new(
                        t.id(),
                        egui::vec2(18.0, 18.0),
                    ));
                    ui.add_space(8.0);
                }
                ui.label("This operation cannot be undone.");
            });
        });
    result
}

/// A one-button informational / error notice (e.g. an archive-open failure): a lead
/// warning icon + the body text, and a bottom-right OK button (default-focused).
/// Returns `Some(true)` when OK is clicked; Esc / the close button dismiss it via the
/// event router. Shares `DIALOG_PAD` with the confirm dialog for matching margins.
fn message_dialog(
    ctx: &egui::Context,
    message: &str,
    icon: Option<&egui::TextureHandle>,
) -> Option<bool> {
    let mut ok = None;
    egui::TopBottomPanel::bottom("message_bar")
        .frame(dialog_frame(ctx))
        .show(ctx, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let resp = ui.add_sized([100.0, 32.0], egui::Button::new("OK"));
                if resp.clicked() {
                    ok = Some(true);
                }
                if ui.memory(|m| m.focused().is_none()) {
                    resp.request_focus();
                }
            });
        });
    egui::CentralPanel::default()
        .frame(dialog_frame(ctx))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(t) = icon {
                    ui.image(egui::load::SizedTexture::new(
                        t.id(),
                        egui::vec2(34.0, 34.0),
                    ));
                    ui.add_space(16.0);
                }
                ui.label(egui::RichText::new(message).size(15.0));
            });
        });
    ok
}

/// Rasterize a vendored SVG icon to an egui texture (tinted `rgb`), for dialog
/// chrome. Reuses the app's `icon::rasterize` (resvg). `None` if it can't rasterize.
fn svg_texture(
    ctx: &egui::Context,
    svg: &str,
    px: u32,
    rgb: [u8; 3],
    name: &str,
) -> Option<egui::TextureHandle> {
    let (rgba, w, h) = crate::icon::rasterize(svg, px, rgb)?;
    let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
    Some(ctx.load_texture(name, img, egui::TextureOptions::LINEAR))
}

/// The Settings form skeleton (controls aren't wired to persistence yet).
fn settings_ui(ui: &mut egui::Ui, d: &mut SettingsDraft) {
    let cap = d.refresh_hz;
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.add_space(4.0);
        ui.heading("Settings");
        ui.add_space(6.0);

        egui::CollapsingHeader::new("Blaze (navigation feel)")
            .default_open(true)
            .show(ui, |ui| {
                ui.add(
                    egui::Slider::new(&mut d.start_speed, 1.0..=30.0)
                        .text("Start speed (photos/sec)"),
                );
                ui.add(egui::Slider::new(&mut d.ramp_secs, 0.5..=10.0).text("Ramp-up time (s)"));
                ui.horizontal(|ui| {
                    ui.add(egui::Slider::new(&mut d.max_fps, 1..=cap).text("Max photos/sec"));
                    ui.add(egui::DragValue::new(&mut d.max_fps).range(1..=cap));
                });
                ui.add(
                    egui::Slider::new(&mut d.hold_delay_ms, 0..=1000)
                        .text("Initial hold delay (ms)"),
                );
            });

        egui::CollapsingHeader::new("Browsing")
            .default_open(true)
            .show(ui, |ui| {
                ui.checkbox(&mut d.recursive, "Open folders recursively by default");
            });

        egui::CollapsingHeader::new("Display")
            .default_open(true)
            .show(ui, |ui| {
                egui::ComboBox::from_label("Default scale mode")
                    .selected_text(["Fit", "Fill", "Original"][d.scale_mode])
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut d.scale_mode, 0, "Fit");
                        ui.selectable_value(&mut d.scale_mode, 1, "Fill");
                        ui.selectable_value(&mut d.scale_mode, 2, "Original");
                    });
                ui.horizontal(|ui| {
                    ui.label("Letterbox color");
                    ui.color_edit_button_rgb(&mut d.letterbox);
                });
                ui.checkbox(&mut d.start_fullscreen, "Start in fullscreen");
            });

        egui::CollapsingHeader::new("Keyboard")
            .default_open(false)
            .show(ui, |ui| {
                ui.label("Keybinding editor — coming soon.");
            });

        egui::CollapsingHeader::new("System")
            .default_open(false)
            .show(ui, |ui| {
                let _ = ui.button("Set as default photo viewer…");
                let _ = ui.button("Reset to defaults");
            });

        ui.add_space(10.0);
        ui.separator();
        ui.horizontal(|ui| {
            let _ = ui.button("Save");
            let _ = ui.button("Cancel");
        });
        ui.add_space(4.0);
        ui.weak("Skeleton — controls aren't wired to settings yet.");
    });
}
