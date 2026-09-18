use super::ViewerApp;
use crate::layout::FitMode;
use eframe::egui;
use egui::{Color32, Stroke};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::info;

/// Default selection / outline blue (`rgb(140, 200, 255)`).
pub const DEFAULT_ACCENT: Color32 = Color32::from_rgb(140, 200, 255);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiPrefs {
    #[serde(default = "default_accent_hex")]
    pub accent: String,
    #[serde(default = "default_outline_width")]
    pub outline_width: f32,
    #[serde(default = "default_true")]
    pub camera_list_open: bool,
    #[serde(default = "default_true")]
    pub sidebar_open: bool,
    /// Name of the last selected view; restored on launch if it still exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_view: Option<String>,
    /// Contain / cover / fill. If omitted, `cameras.toml` `[viewer] default_fit` is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<String>,
}

fn default_accent_hex() -> String {
    color_to_hex(DEFAULT_ACCENT)
}

fn default_true() -> bool {
    true
}

fn default_outline_width() -> f32 {
    3.0
}

impl Default for UiPrefs {
    fn default() -> Self {
        Self {
            accent: default_accent_hex(),
            outline_width: default_outline_width(),
            camera_list_open: true,
            sidebar_open: true,
            last_view: None,
            fit: None,
        }
    }
}

impl UiPrefs {
    pub fn path_next_to(config_path: &Path) -> PathBuf {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("ui.toml")
    }

    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Ok(text) = toml::to_string_pretty(self) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, text);
        }
    }

    pub fn accent_color(&self) -> Color32 {
        parse_hex_color(&self.accent).unwrap_or(DEFAULT_ACCENT)
    }

    pub fn outline_width(&self) -> f32 {
        self.outline_width.clamp(1.0, 16.0)
    }
}

pub fn parse_hex_color(s: &str) -> Option<Color32> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color32::from_rgb(r, g, b))
}

pub fn color_to_hex(c: Color32) -> String {
    let [r, g, b, _] = c.to_array();
    format!("#{r:02X}{g:02X}{b:02X}")
}

pub fn mix_rgb(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let [ar, ag, ab, _] = a.to_array();
    let [br, bg, bb, _] = b.to_array();
    let m = |x: u8, y: u8| (f32::from(x) * (1.0 - t) + f32::from(y) * t).round() as u8;
    Color32::from_rgb(m(ar, br), m(ag, bg), m(ab, bb))
}

impl ViewerApp {
    pub(super) fn apply_accent_visuals(&self, ctx: &egui::Context) {
        let accent = self.accent;
        let [r, g, b, _] = accent.to_array();
        let fill = Color32::from_rgba_unmultiplied(r, g, b, 70);
        ctx.style_mut(|style| {
            style.visuals.selection.bg_fill = fill;
            style.visuals.selection.stroke = Stroke::new(1.0_f32, accent);
            style.visuals.hyperlink_color = accent;
            style.visuals.widgets.hovered.bg_stroke.color = accent;
            style.visuals.widgets.active.bg_stroke.color = accent;
        });
    }

    pub(super) fn save_ui_prefs(&self) {
        UiPrefs {
            accent: color_to_hex(self.accent),
            outline_width: self.outline_width,
            camera_list_open: self.camera_list_open,
            sidebar_open: self.sidebar_open,
            last_view: Some(self.views.active_view().name.clone()),
            fit: Some(self.fit.as_str().to_string()),
        }
        .save(&self.ui_prefs_path);
    }

    pub(super) fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }

        let mut open = self.show_settings;
        egui::Window::new("⚙ Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.heading("Diagnostics");
                ui.add_space(4.0);
                if ui
                    .selectable_label(self.debug_overlay, "🐛  Perf overlay")
                    .on_hover_text("Stream metrics (D). Also: RUSTCAMS_DEBUG=1")
                    .clicked()
                {
                    self.debug_overlay = !self.debug_overlay;
                    info!(debug = self.debug_overlay, "debug overlay toggled");
                }
                if ui
                    .selectable_label(self.show_log, "📋  Log console")
                    .on_hover_text("In-app log (L) — replaces the hidden Windows console")
                    .clicked()
                {
                    self.show_log = !self.show_log;
                }

                ui.add_space(12.0);
                ui.separator();
                ui.heading("Appearance");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Accent");
                    let [r, g, b, _] = self.accent.to_array();
                    let mut rgb = [r, g, b];
                    if ui.color_edit_button_srgb(&mut rgb).changed() {
                        self.accent = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
                        self.save_ui_prefs();
                    }
                    ui.monospace(color_to_hex(self.accent));
                });
                ui.label(
                    egui::RichText::new("Selection outline, highlights, and playing cameras")
                        .small()
                        .weak(),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("Fit");
                    for mode in FitMode::all() {
                        let hint = match mode {
                            FitMode::Contain => "Letterbox — keep aspect ratio",
                            FitMode::Cover => "Crop — fill the cell, keep aspect",
                            FitMode::Fill => "Stretch — fill the cell",
                        };
                        if ui
                            .selectable_label(self.fit == *mode, mode.label())
                            .on_hover_text(hint)
                            .clicked()
                        {
                            self.fit = *mode;
                            self.save_ui_prefs();
                        }
                    }
                });
                ui.label(
                    egui::RichText::new("How video sits in each cell")
                        .small()
                        .weak(),
                );
                ui.add_space(8.0);
                if ui
                    .add(
                        egui::Slider::new(&mut self.outline_width, 1.0..=16.0)
                            .text("Outline")
                            .suffix(" px")
                            .step_by(0.5),
                    )
                    .changed()
                {
                    self.outline_width = self.outline_width.clamp(1.0, 16.0);
                    self.save_ui_prefs();
                }
                ui.add_space(6.0);
                if ui.button("Reset appearance").clicked() {
                    self.accent = DEFAULT_ACCENT;
                    self.outline_width = default_outline_width();
                    self.save_ui_prefs();
                }
            });
        self.show_settings = open;
    }
}
