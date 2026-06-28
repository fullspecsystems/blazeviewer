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
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};

use pb_decode::{decode_bytes, FitBox};

/// Which dialog a [`DialogWindow`] is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    About,
    Settings,
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
    draft: SettingsDraft,
}

impl DialogWindow {
    /// Create and show the dialog window. `refresh_hz` caps the Settings "max
    /// photos/sec" slider. Returns `None` if window/GPU setup fails (best-effort —
    /// a failed dialog must never take down the viewer).
    pub fn open(
        kind: DialogKind,
        event_loop: &ActiveEventLoop,
        refresh_hz: u32,
    ) -> Option<DialogWindow> {
        let (w, h, resizable, title) = match kind {
            DialogKind::About => (254.0, 307.0, false, "About PhotoBlaze"),
            DialogKind::Settings => (560.0, 660.0, true, "PhotoBlaze Settings"),
        };
        let attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(LogicalSize::new(w, h))
            .with_resizable(resizable);
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

        window.request_redraw();
        Some(DialogWindow {
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
            draft: SettingsDraft::new(refresh_hz),
        })
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    pub fn kind(&self) -> DialogKind {
        self.kind
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
        let draft = &mut self.draft;
        let full_output = ctx.run(raw_input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| match kind {
                DialogKind::About => about_ui(ui, icon.as_ref()),
                DialogKind::Settings => settings_ui(ui, draft),
            });
        });

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
