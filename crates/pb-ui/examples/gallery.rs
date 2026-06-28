//! `pb_ui` component gallery — the egui equivalent of a Storybook page.
//!
//! Renders every design token (color roles, type scale) and every component with a
//! **Light / Dark / Both** switch, so the whole system can be eyeballed in one place —
//! and light vs dark compared **side by side**, which the real single-theme dialogs
//! can't show. This is also the place to A/B the theme identity: tweak `pb_ui`'s
//! palette and watch both columns update.
//!
//! Dev-only: `cargo run -p pb-ui --example gallery`. Never compiled into the app.

use eframe::egui;
use pb_ui::icon::{Icon, Tone};
use pb_ui::Palette;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Light,
    Dark,
    Both,
}

/// Demo state for the live controls (shared across columns so toggling reads naturally).
struct Gallery {
    mode: Mode,
    start_speed: f32,
    recursive: bool,
    fullscreen: bool,
    scale_mode: usize,
    letterbox: [f32; 3],
    text: String,
    password: String,
}

impl Default for Gallery {
    fn default() -> Self {
        Self {
            mode: Mode::Both,
            start_speed: 8.0,
            recursive: true,
            fullscreen: false,
            scale_mode: 0,
            letterbox: [0.05, 0.05, 0.06],
            text: String::new(),
            password: String::new(),
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 840.0])
            .with_title("pb_ui — component gallery"),
        ..Default::default()
    };
    eframe::run_native(
        "pb_ui gallery",
        options,
        Box::new(|cc| {
            pb_ui::install_fonts(&cc.egui_ctx);
            Ok(Box::new(Gallery::default()))
        }),
    )
}

impl eframe::App for Gallery {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The gallery's own shell uses the dark theme; each column overrides per-theme.
        pb_ui::apply_style(ctx, true);

        egui::TopBottomPanel::top("bar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("pb_ui");
                ui.label(
                    egui::RichText::new("design-system gallery")
                        .color(Palette::new(true).text_secondary),
                );
                ui.add_space(20.0);
                ui.selectable_value(&mut self.mode, Mode::Light, "Light");
                ui.selectable_value(&mut self.mode, Mode::Dark, "Dark");
                ui.selectable_value(&mut self.mode, Mode::Both, "Both");
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(ctx.style().visuals.panel_fill))
            .show(ctx, |ui| match self.mode {
                Mode::Light => self.theme_column(ui, false),
                Mode::Dark => self.theme_column(ui, true),
                Mode::Both => {
                    ui.columns(2, |cols| {
                        self.theme_column(&mut cols[0], false);
                        self.theme_column(&mut cols[1], true);
                    });
                }
            });
    }
}

impl Gallery {
    /// One themed column: scopes the design system to `dark`, paints the page
    /// background, and renders the token + component catalog.
    fn theme_column(&mut self, ui: &mut egui::Ui, dark: bool) {
        let p = Palette::new(dark);
        pb_ui::apply_to_ui(ui, dark);
        egui::Frame::none()
            .fill(p.page)
            .inner_margin(egui::Margin::same(16.0))
            .show(ui, |ui| {
                ui.set_min_height(ui.available_height());
                egui::ScrollArea::vertical()
                    .id_salt(if dark { "dark" } else { "light" })
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.catalog(ui, &p));
            });
    }

    /// The token + component catalog, rendered with palette `p`.
    fn catalog(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.label(
            egui::RichText::new(if p.dark { "Dark" } else { "Light" })
                .size(20.0)
                .strong()
                .color(p.text),
        );

        pb_ui::section_label(ui, p, "Color roles");
        swatches(ui, p);

        pb_ui::section_label(ui, p, "Type scale");
        ui.label(egui::RichText::new("Heading").size(26.0).color(p.text));
        ui.label(egui::RichText::new("Body — the quick brown fox").size(14.5).color(p.text));
        ui.label(
            egui::RichText::new("Small / secondary text")
                .size(12.5)
                .color(p.text_secondary),
        );

        pb_ui::section_label(ui, p, "Buttons");
        ui.horizontal(|ui| {
            let _ = pb_ui::primary_button(ui, p, "Primary");
            let _ = pb_ui::secondary_button(ui, "Secondary");
        });

        pb_ui::section_label(ui, p, "Toggle");
        ui.horizontal(|ui| {
            pb_ui::toggle(ui, p, &mut self.recursive);
            ui.add_space(SPACE);
            pb_ui::toggle(ui, p, &mut self.fullscreen);
        });

        // The icon set (neutral tone), then one icon across every tone — all white
        // sprites tinted at draw, theme-aware via the palette.
        pb_ui::section_label(ui, p, &format!("Icons ({})", pb_ui::icon::family_name()));
        ui.horizontal_wrapped(|ui| {
            for ic in [
                Icon::Lock,
                Icon::Warning,
                Icon::Error,
                Icon::Info,
                Icon::Success,
                Icon::Help,
                Icon::Trash,
            ] {
                pb_ui::icon::image(ui, ic, 22.0, Tone::Neutral, p);
                ui.add_space(SPACE);
            }
        });
        ui.add_space(SPACE);
        ui.horizontal(|ui| {
            for tone in [
                Tone::Neutral,
                Tone::Accent,
                Tone::Warning,
                Tone::Danger,
                Tone::Success,
            ] {
                pb_ui::icon::image(ui, Icon::Warning, 22.0, tone, p);
                ui.add_space(SPACE);
            }
        });

        pb_ui::section_label(ui, p, "Setting cards");
        pb_ui::card(ui, p, |ui| {
            pb_ui::card_row(
                ui,
                p,
                None,
                "Start speed",
                Some("Photos per second when you first hold a key"),
                |ui| {
                    pb_ui::slider(ui, &mut self.start_speed, 1.0..=30.0, "/s");
                },
            );
        });
        ui.add_space(SPACE);
        pb_ui::card(ui, p, |ui| {
            pb_ui::card_row(
                ui,
                p,
                None,
                "Open folders recursively",
                Some("Include photos in subfolders"),
                |ui| {
                    pb_ui::toggle_with_label(ui, p, &mut self.recursive);
                },
            );
        });
        ui.add_space(SPACE);
        pb_ui::card(ui, p, |ui| {
            pb_ui::card_row(
                ui,
                p,
                None,
                "Default scale mode",
                Some("How a photo fits the window"),
                |ui| {
                    egui::ComboBox::from_id_salt(("scale", p.dark))
                        .width(150.0)
                        .selected_text(["Fit", "Fill", "Original"][self.scale_mode])
                        .show_ui(ui, |ui| {
                            // The popup opens as a top-level Area on the *global* ctx
                            // style; re-scope it to this column's theme so the options
                            // aren't mis-colored in the non-global column.
                            pb_ui::apply_to_ui(ui, p.dark);
                            ui.selectable_value(&mut self.scale_mode, 0, "Fit");
                            ui.selectable_value(&mut self.scale_mode, 1, "Fill");
                            ui.selectable_value(&mut self.scale_mode, 2, "Original");
                        });
                },
            );
        });
        ui.add_space(SPACE);
        pb_ui::card(ui, p, |ui| {
            pb_ui::card_row(
                ui,
                p,
                None,
                "Letterbox color",
                Some("Around a photo that doesn't fill the screen"),
                |ui| {
                    ui.color_edit_button_rgb(&mut self.letterbox);
                },
            );
        });

        pb_ui::section_label(ui, p, "Text field");
        ui.add(pb_ui::text_field(&mut self.text, "Type here").desired_width(240.0));

        // Dialog patterns — composed from the same pb_ui buttons/fields as the real app,
        // so editing a button here updates every dialog and they can't drift apart.
        pb_ui::section_label(ui, p, "Dialogs");

        // Confirm (destructive): a message + note, then [Delete] [Cancel].
        mini_dialog(ui, p, |ui| {
            ui.label(egui::RichText::new("Delete this photo?").size(16.0).color(p.text));
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("This operation cannot be undone.").color(p.text_secondary),
            );
            dialog_buttons(ui, |ui| {
                let _ = pb_ui::secondary_button(ui, "Cancel");
                ui.add_space(8.0);
                let _ = pb_ui::danger_button(ui, "Delete");
            });
        });
        ui.add_space(12.0);

        // Message (notification): a line of text, then a single [OK].
        mini_dialog(ui, p, |ui| {
            ui.label(egui::RichText::new("Could not open the archive.").color(p.text));
            dialog_buttons(ui, |ui| {
                let _ = pb_ui::primary_button(ui, p, "OK");
            });
        });
        ui.add_space(12.0);

        // Password entry: a prompt, a masked field, then [Unlock] [Cancel].
        mini_dialog(ui, p, |ui| {
            ui.label(
                egui::RichText::new("Enter the password for \u{201c}photos.7z\u{201d}").color(p.text),
            );
            ui.add_space(10.0);
            ui.add(
                pb_ui::text_field(&mut self.password, "Password")
                    .password(true)
                    .desired_width(f32::INFINITY),
            );
            dialog_buttons(ui, |ui| {
                let _ = pb_ui::secondary_button(ui, "Cancel");
                ui.add_space(8.0);
                let _ = pb_ui::primary_button(ui, p, "Unlock");
            });
        });
        ui.add_space(16.0);
    }
}

/// A dialog-window-like frame (page fill, hairline border, rounded, padded, fixed
/// width) for previewing a dialog body inline in the gallery.
fn mini_dialog(ui: &mut egui::Ui, p: &Palette, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(p.page)
        .stroke(egui::Stroke::new(1.0, p.card_stroke))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::same(18.0))
        .show(ui, |ui| {
            ui.set_width(340.0);
            body(ui);
        });
}

/// The bottom action row of a dialog: a divider, then right-aligned buttons (added
/// right-to-left, so e.g. `Cancel` then `Delete` reads `[Delete] [Cancel]`).
fn dialog_buttons(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(16.0);
    ui.separator();
    ui.add_space(12.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add);
}

/// One swatch per color role, with its name underneath.
fn swatches(ui: &mut egui::Ui, p: &Palette) {
    let roles = [
        ("page", p.page),
        ("card", p.card),
        ("control", p.control),
        ("text", p.text),
        ("secondary", p.text_secondary),
        ("accent", p.accent),
        ("toggle off", p.toggle_off),
    ];
    ui.horizontal_wrapped(|ui| {
        for (name, color) in roles {
            ui.vertical(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(58.0, 38.0), egui::Sense::hover());
                ui.painter().rect(
                    rect,
                    egui::Rounding::same(6.0),
                    color,
                    egui::Stroke::new(1.0, p.card_stroke),
                );
                ui.label(egui::RichText::new(name).size(11.0).color(p.text_secondary));
            });
        }
    });
}

/// Local alias for the standard inter-item gap (the example doesn't import the tokens
/// namespace wholesale).
const SPACE: f32 = 8.0;
