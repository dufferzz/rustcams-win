use super::icons;
use super::ViewerApp;
use crate::config::{AppConfig, NvrConfig, StreamType};
use crate::layout::FitMode;
use eframe::egui;
use egui::{Color32, Stroke};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum SettingsTab {
    #[default]
    Nvr,
    Gates,
    Anpr,
    Display,
    Diagnostics,
    About,
}

/// Default selection / outline blue (`rgb(140, 200, 255)`).
pub const DEFAULT_ACCENT: Color32 = Color32::from_rgb(140, 200, 255);
pub const PANEL_BG: Color32 = Color32::from_rgb(14, 16, 20);
pub const CANVAS_BG: Color32 = Color32::from_rgb(8, 9, 12);
pub const STATUS_BG: Color32 = Color32::from_rgb(16, 18, 22);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryGroup {
    pub name: String,
    #[serde(default = "default_true")]
    pub open: bool,
    /// Camera ids in display order within this group.
    #[serde(default)]
    pub cameras: Vec<String>,
}

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
    /// Camera library side panel width (pixels).
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    /// Name of the last selected view; restored on launch if it still exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_view: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_aux_view: Option<String>,
    /// Contain / cover / fill. If omitted, `cameras.toml` `[viewer] default_fit` is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<String>,
    /// Append `stutter-stats.log` next to cameras.toml (~2s). Off by default.
    #[serde(default)]
    pub stutter_log_file: bool,
    /// Linux AppImage: query GitHub Releases on launch; confirm before download.
    #[serde(default = "default_true")]
    pub check_updates: bool,
    /// Last skipped update version (`0.3.0`); stay quiet until a newer tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_update: Option<String>,
    /// Named camera-library groups with manual order (drag-and-drop).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub library_groups: Vec<LibraryGroup>,
    #[serde(default = "default_w1")]
    pub decode_1: i32,
    #[serde(default = "default_w1_hd")]
    pub decode_1_hd: i32,
    #[serde(default = "default_w2")]
    pub decode_2: i32,
    #[serde(default = "default_w2_hd")]
    pub decode_2_hd: i32,
    #[serde(default = "default_w2x2")]
    pub decode_2x2: i32,
    #[serde(default = "default_w2x2_hd")]
    pub decode_2x2_hd: i32,
    #[serde(default = "default_w3")]
    pub decode_3x3: i32,
    #[serde(default = "default_w4")]
    pub decode_4x4: i32,
    #[serde(default = "default_w5")]
    pub decode_5x5: i32,
    #[serde(default = "default_w6")]
    pub decode_6x6: i32,
    /// software | nvdec | hardware. `RUSTCAMS_DECODE` overrides when set.
    #[serde(default)]
    pub decode_backend: crate::gst_link::DecodeBackend,
    /// Play RTSP audio for the selected camera (toolbar speaker). Off by default.
    #[serde(default)]
    pub audio_enabled: bool,
}

fn default_accent_hex() -> String {
    color_to_hex(DEFAULT_ACCENT)
}

fn default_true() -> bool {
    true
}

fn default_sidebar_width() -> f32 {
    220.0
}

pub fn clamp_sidebar_width(w: f32) -> f32 {
    w.clamp(160.0, 360.0)
}

fn default_outline_width() -> f32 {
    3.0
}

pub fn default_w1() -> i32 {
    640
}
pub fn default_w1_hd() -> i32 {
    1280
}
pub fn default_w2() -> i32 {
    640
}
pub fn default_w2_hd() -> i32 {
    960
}
pub fn default_w2x2() -> i32 {
    640
}
pub fn default_w2x2_hd() -> i32 {
    640
}
pub fn default_w3() -> i32 {
    480
}
pub fn default_w4() -> i32 {
    400
}
pub fn default_w5() -> i32 {
    352
}
pub fn default_w6() -> i32 {
    288
}

pub fn clamp_decode_width(w: i32) -> i32 {
    w.clamp(160, 1920)
}

impl Default for UiPrefs {
    fn default() -> Self {
        Self {
            accent: default_accent_hex(),
            outline_width: default_outline_width(),
            camera_list_open: true,
            sidebar_open: true,
            sidebar_width: default_sidebar_width(),
            last_view: None,
            last_aux_view: None,
            fit: None,
            stutter_log_file: false,
            check_updates: true,
            skipped_update: None,
            library_groups: Vec::new(),
            decode_1: default_w1(),
            decode_1_hd: default_w1_hd(),
            decode_2: default_w2(),
            decode_2_hd: default_w2_hd(),
            decode_2x2: default_w2x2(),
            decode_2x2_hd: default_w2x2_hd(),
            decode_3x3: default_w3(),
            decode_4x4: default_w4(),
            decode_5x5: default_w5(),
            decode_6x6: default_w6(),
            decode_backend: crate::gst_link::DecodeBackend::Software,
            audio_enabled: false,
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
            let _ = crate::config::write_secret_file(path, text.as_bytes());
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
        let mut v = egui::Visuals::dark();
        v.dark_mode = true;
        v.panel_fill = PANEL_BG;
        v.window_fill = Color32::from_rgb(20, 22, 26);
        v.extreme_bg_color = CANVAS_BG;
        v.faint_bg_color = Color32::from_rgb(28, 30, 36);
        v.widgets.noninteractive.bg_fill = Color32::from_rgb(20, 22, 26);
        v.widgets.inactive.bg_fill = Color32::from_rgb(32, 36, 42);
        v.widgets.hovered.bg_fill = Color32::from_rgb(42, 48, 56);
        v.widgets.active.bg_fill = Color32::from_rgb(48, 54, 64);
        v.widgets.open.bg_fill = Color32::from_rgb(32, 36, 42);
        v.selection.bg_fill = fill;
        v.selection.stroke = Stroke::new(1.0_f32, accent);
        v.hyperlink_color = accent;
        v.widgets.hovered.bg_stroke.color = accent;
        v.widgets.active.bg_stroke.color = accent;
        ctx.set_visuals(v);
    }

    pub(super) fn save_ui_prefs(&self) {
        let aux_name = self.views.views.get(self.aux_view).map(|v| v.name.clone());
        UiPrefs {
            accent: color_to_hex(self.accent),
            outline_width: self.outline_width,
            camera_list_open: self.camera_list_open,
            sidebar_open: self.sidebar_open,
            sidebar_width: clamp_sidebar_width(self.sidebar_width),
            last_view: Some(self.views.active_view().name.clone()),
            last_aux_view: aux_name,
            fit: Some(self.fit.as_str().to_string()),
            stutter_log_file: self.stutter_log_file,
            check_updates: self.check_updates,
            skipped_update: self.skipped_update.clone(),
            library_groups: self.library_groups.clone(),
            decode_1: self.decode_1,
            decode_1_hd: self.decode_1_hd,
            decode_2: self.decode_2,
            decode_2_hd: self.decode_2_hd,
            decode_2x2: self.decode_2x2,
            decode_2x2_hd: self.decode_2x2_hd,
            decode_3x3: self.decode_3x3,
            decode_4x4: self.decode_4x4,
            decode_5x5: self.decode_5x5,
            decode_6x6: self.decode_6x6,
            decode_backend: self.decode_backend,
            audio_enabled: self.audio_enabled,
        }
        .save(&self.ui_prefs_path);
    }

    fn apply_nvr_from_settings(&mut self) {
        let mut nvr = self.nvr_draft.clone();
        nvr.enabled = self.nvr_enabled;
        if nvr.host.trim().is_empty() {
            if self.nvr_enabled {
                self.nvr_status = Some("NVR host is empty — not saved.".into());
                return;
            }
            self.app_config.nvr = None;
        } else {
            if let Err(err) = crate::config::validate_host(&nvr.host) {
                self.nvr_status = Some(format!("Invalid NVR host: {err:#}"));
                return;
            }
            let proto = self.nvr_protocols.trim();
            nvr.protocols = if proto.is_empty() || proto == "default" {
                None
            } else {
                Some(proto.to_string())
            };
            self.app_config.nvr = Some(nvr);
        }
        self.sync_gate_into_app_config();
        self.sync_anpr_into_app_config();

        if let Err(err) = self.app_config.save(&self.config_path) {
            self.nvr_status = Some(format!("Failed to save cameras.toml: {err:#}"));
            warn!("failed to save cameras.toml: {err:#}");
            return;
        }
        self.note_config_mtime();

        match self.app_config.clone().resolve() {
            Ok(resolved) => {
                let n = resolved.cameras.len();
                self.apply_resolved_cameras(resolved.cameras);
                if self.cameras.is_empty() {
                    self.config_warning = Some(if self.nvr_enabled {
                        "NVR saved but no cameras resolved.".into()
                    } else {
                        "No cameras resolved — add [[cameras]] URLs or enable NVR.".into()
                    });
                } else {
                    self.config_warning = None;
                }
                self.nvr_status = Some(format!("Saved. {n} camera(s) resolved."));
                self.nvr_retry = false;
                self.nvr_job_gen
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                info!(
                    cameras = n,
                    nvr = self.nvr_enabled,
                    "reloaded cameras after settings save"
                );
            }
            Err(err) => {
                self.nvr_status = Some(format!("Saved file, but resolve failed: {err:#}"));
                warn!("resolve after NVR save failed: {err:#}");
                self.nvr_retry = true;
                self.kick_nvr_resolve();
            }
        }
    }

    fn sync_gate_into_app_config(&mut self) {
        self.app_config.gate = if self.gate_draft.host.trim().is_empty() {
            None
        } else {
            Some(self.gate_draft.clone())
        };
    }

    fn sync_anpr_into_app_config(&mut self) {
        self.anpr_draft.plates.retain(|p| !p.plate.trim().is_empty());
        self.app_config.anpr = if self.anpr_draft.host.trim().is_empty() {
            None
        } else {
            Some(self.anpr_draft.clone())
        };
    }

    fn apply_gate_from_settings(&mut self) {
        if self.gate_draft.host.trim().is_empty() {
            *self.gate_status.lock() = Some("Gate host is empty — not saved.".into());
            return;
        }
        if self.gate_draft.username.trim().is_empty() {
            *self.gate_status.lock() = Some("Gate username is empty — not saved.".into());
            return;
        }
        if let Err(err) = crate::config::validate_host(&self.gate_draft.host) {
            *self.gate_status.lock() = Some(format!("Invalid gate host: {err:#}"));
            return;
        }
        self.gate_draft.door_id = "1".into();
        self.gate_draft.action = "open".into();
        self.sync_gate_into_app_config();
        if let Err(err) = self.app_config.save(&self.config_path) {
            *self.gate_status.lock() = Some(format!("Failed to save cameras.toml: {err:#}"));
            warn!("failed to save cameras.toml: {err:#}");
            return;
        }
        self.note_config_mtime();
        *self.gate_status.lock() = Some("Gate settings saved.".into());
        info!(host = %self.gate_draft.host, "saved gate settings");
    }

    fn apply_anpr_from_settings(&mut self) {
        if self.anpr_draft.host.trim().is_empty() {
            self.anpr_status = Some("ANPR host is empty — cleared.".into());
            self.app_config.anpr = None;
        } else {
            if self.anpr_draft.username.trim().is_empty() {
                self.anpr_status = Some("ANPR username is empty — not saved.".into());
                return;
            }
            if let Err(err) = crate::config::validate_host(&self.anpr_draft.host) {
                self.anpr_status = Some(format!("Invalid ANPR host: {err:#}"));
                return;
            }
            if self.anpr_draft.channel == 0 {
                self.anpr_draft.channel = 1;
            }
            self.sync_anpr_into_app_config();
        }
        self.sync_gate_into_app_config();
        if let Err(err) = self.app_config.save(&self.config_path) {
            self.anpr_status = Some(format!("Failed to save cameras.toml: {err:#}"));
            warn!("failed to save cameras.toml: {err:#}");
            return;
        }
        self.note_config_mtime();
        self.restart_anpr_from_config();
        let n = self
            .app_config
            .anpr
            .as_ref()
            .map(|a| a.plates.len())
            .unwrap_or(0);
        self.anpr_status = Some(format!("ANPR settings saved ({n} plate(s))."));
        info!(
            host = %self.anpr_draft.host,
            plates = n,
            enabled = self.anpr_draft.enabled,
            "saved ANPR settings"
        );
    }

    pub(super) fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }

        let mut open = self.show_settings;
        let mut save_nvr = false;
        let mut save_gate = false;
        let mut save_anpr = false;
        let mut save_ui = false;
        egui::Window::new(icons::labeled(icons::GEAR, "Settings"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(480.0)
            .default_height(560.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.settings_tab, SettingsTab::Nvr, "NVR");
                    ui.selectable_value(&mut self.settings_tab, SettingsTab::Gates, "Gates");
                    ui.selectable_value(&mut self.settings_tab, SettingsTab::Anpr, "ANPR");
                    ui.selectable_value(&mut self.settings_tab, SettingsTab::Display, "Display");
                    ui.selectable_value(
                        &mut self.settings_tab,
                        SettingsTab::Diagnostics,
                        "Diagnostics",
                    );
                    ui.selectable_value(&mut self.settings_tab, SettingsTab::About, "About");
                });
                ui.separator();
                ui.add_space(6.0);

                match self.settings_tab {
                    SettingsTab::Diagnostics => {
                ui.heading("Diagnostics");
                ui.add_space(4.0);
                if ui
                    .selectable_label(self.debug_overlay, icons::labeled(icons::BUG, "Perf overlay"))
                    .on_hover_text("Stream metrics. Also: RUSTCAMS_DEBUG=1")
                    .clicked()
                {
                    self.debug_overlay = !self.debug_overlay;
                    info!(debug = self.debug_overlay, "debug overlay toggled");
                }
                if ui
                    .selectable_label(
                        self.show_log,
                        icons::labeled(icons::TERMINAL_WINDOW, "Log console"),
                    )
                    .on_hover_text("In-app log — replaces the hidden Windows console")
                    .clicked()
                {
                    self.show_log = !self.show_log;
                }
                if ui
                    .checkbox(&mut self.stutter_log_file, "Write stutter-stats.log")
                    .on_hover_text(
                        "Append hitch samples next to cameras.toml every ~2s. Off by default.",
                    )
                    .changed()
                {
                    save_ui = true;
                }
                    }
                    SettingsTab::Nvr => {
                ui.heading("NVR");
                ui.label(
                    egui::RichText::new(
                        "Hikvision discovery + RTSP proxy. Uncheck Use NVR to keep credentials but stream from [[cameras]] URLs. Saved to cameras.toml.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(4.0);
                ui.checkbox(&mut self.nvr_enabled, "Use NVR")
                    .on_hover_text(
                        "Off: do not discover/stream via the NVR. [nvr] stays in cameras.toml when a host is set.",
                    );
                ui.add_enabled_ui(self.nvr_enabled, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Host");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.nvr_draft.host)
                                .desired_width(220.0)
                                .hint_text("192.168.x.x"),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("HTTP port");
                        ui.add(egui::DragValue::new(&mut self.nvr_draft.http_port).range(1..=65535))
                            .on_hover_text("NVR HTTP / ISAPI (this app). Default 80, often remapped e.g. 49000.");
                        ui.label("RTSP port");
                        ui.add(egui::DragValue::new(&mut self.nvr_draft.rtsp_port).range(1..=65535))
                            .on_hover_text("NVR RTSP for grid streams. Default 554, often remapped e.g. 49002.");
                    });
                    ui.horizontal(|ui| {
                        ui.label("HTTPS port");
                        ui.add(egui::DragValue::new(&mut self.nvr_draft.https_port).range(1..=65535))
                            .on_hover_text("NVR HTTPS (mapped 443, e.g. 49003). Same ISAPI as HTTP.");
                        ui.label("Server port");
                        ui.add(egui::DragValue::new(&mut self.nvr_draft.server_port).range(1..=65535))
                            .on_hover_text("Hikvision SDK port (mapped 8000, e.g. 49001). iVMS uses this; this app does not.");
                    });
                    ui.horizontal(|ui| {
                        ui.label("User");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.nvr_draft.username)
                                .desired_width(120.0),
                        );
                        ui.label("Password");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.nvr_draft.password)
                                .password(true)
                                .desired_width(140.0),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("Stream");
                        for (label, st) in [
                            ("sub", StreamType::Sub),
                            ("main", StreamType::Main),
                            ("third", StreamType::Third),
                        ] {
                            if ui
                                .selectable_label(self.nvr_draft.stream == st, label)
                                .clicked()
                            {
                                self.nvr_draft.stream = st;
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("RTSP");
                        let current = if self.nvr_protocols.is_empty() {
                            "default".to_string()
                        } else {
                            self.nvr_protocols.clone()
                        };
                        egui::ComboBox::from_id_salt("nvr_proto")
                            .selected_text(&current)
                            .show_ui(ui, |ui| {
                                for p in ["default", "udp", "tcp", "udp+tcp"] {
                                    if ui.selectable_label(current.as_str() == p, p).clicked() {
                                        self.nvr_protocols = if p == "default" {
                                            String::new()
                                        } else {
                                            p.to_string()
                                        };
                                    }
                                }
                            });
                    });
                });
                if ui
                    .button("Save NVR & rediscover")
                    .on_hover_text("Write [nvr] to cameras.toml and reload the camera list")
                    .clicked()
                {
                    save_nvr = true;
                }
                if let Some(msg) = &self.nvr_status {
                    ui.label(egui::RichText::new(msg).small().weak());
                }
                    }
                    SettingsTab::Gates => {
                ui.heading("Gates");
                ui.label(
                    egui::RichText::new(
                        "Hikvision Access Control. Toolbar Gates / DualShock Share, then confirm (Enter or ✕).",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Host");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.gate_draft.host)
                            .desired_width(220.0)
                            .hint_text("192.168.x.x"),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("HTTP port");
                    ui.add(
                        egui::DragValue::new(&mut self.gate_draft.http_port).range(1..=65535),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("User");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.gate_draft.username)
                            .desired_width(120.0),
                    );
                    ui.label("Password");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.gate_draft.password)
                            .password(true)
                            .desired_width(140.0),
                    );
                });
                if ui
                    .button("Save gate")
                    .on_hover_text("Write [gate] to cameras.toml")
                    .clicked()
                {
                    save_gate = true;
                }
                if let Some(msg) = self.gate_status_text() {
                    ui.label(egui::RichText::new(msg).small().weak());
                }
                    }
                    SettingsTab::Anpr => {
                ui.heading("ANPR");
                ui.label(
                    egui::RichText::new(
                        "Hikvision license-plate events (alertStream). Watchlist hits play a sound and show a popup — no files are saved.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(4.0);
                ui.checkbox(&mut self.anpr_draft.enabled, "Enable ANPR listener")
                    .on_hover_text("Off keeps credentials/watchlist in cameras.toml but stops listening.");
                ui.horizontal(|ui| {
                    ui.label("Host");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.anpr_draft.host)
                            .desired_width(220.0)
                            .hint_text("192.168.x.x"),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("HTTP port");
                    ui.add(
                        egui::DragValue::new(&mut self.anpr_draft.http_port).range(1..=65535),
                    );
                    ui.label("Channel");
                    ui.add(
                        egui::DragValue::new(&mut self.anpr_draft.channel).range(1..=64),
                    )
                    .on_hover_text("Snapshot channel for the popup still (ISAPI picture).");
                });
                ui.horizontal(|ui| {
                    ui.label("User");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.anpr_draft.username)
                            .desired_width(120.0),
                    );
                    ui.label("Password");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.anpr_draft.password)
                            .password(true)
                            .desired_width(140.0),
                    );
                });
                if ui
                    .checkbox(&mut self.anpr_draft.silent, "Mute alert audio")
                    .changed()
                {
                    self.anpr.set_silent(self.anpr_draft.silent);
                }

                ui.add_space(10.0);
                ui.separator();
                ui.heading("Watchlist");
                ui.label(
                    egui::RichText::new(
                        "Sound: alert (default) or kim — both shipped in the app. Or a path to an .mp3 next to cameras.toml.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(4.0);

                let mut remove_idx: Option<usize> = None;
                egui::ScrollArea::vertical()
                    .max_height(220.0)
                    .show(ui, |ui| {
                        for (i, plate) in self.anpr_draft.plates.iter_mut().enumerate() {
                            ui.group(|ui| {
                                ui.horizontal(|ui| {
                                    ui.checkbox(&mut plate.enabled, "");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut plate.plate)
                                            .desired_width(100.0)
                                            .hint_text("Plate"),
                                    );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut plate.name)
                                            .desired_width(120.0)
                                            .hint_text("Name"),
                                    );
                                    if ui.small_button("✕").on_hover_text("Remove").clicked() {
                                        remove_idx = Some(i);
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Sound");
                                    let current = {
                                        let s = plate.sound.trim();
                                        if s.is_empty() {
                                            "alert".to_string()
                                        } else {
                                            s.to_string()
                                        }
                                    };
                                    egui::ComboBox::from_id_salt(("anpr_sound", i))
                                        .selected_text(&current)
                                        .width(100.0)
                                        .show_ui(ui, |ui| {
                                            for key in ["alert", "kim"] {
                                                if ui
                                                    .selectable_label(current.as_str() == key, key)
                                                    .clicked()
                                                {
                                                    plate.sound = key.to_string();
                                                }
                                            }
                                        });
                                    ui.add(
                                        egui::TextEdit::singleline(&mut plate.sound)
                                            .desired_width(160.0)
                                            .hint_text("alert / kim / path"),
                                    );
                                });
                            });
                            ui.add_space(4.0);
                        }
                    });
                if let Some(i) = remove_idx {
                    self.anpr_draft.plates.remove(i);
                }
                if ui.button("Add plate").clicked() {
                    self.anpr_draft.plates.push(crate::config::AnprPlate::default());
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .button("Save ANPR")
                        .on_hover_text("Write [anpr] to cameras.toml and reconnect")
                        .clicked()
                    {
                        save_anpr = true;
                    }
                    let testing = self
                        .anpr_test_inflight
                        .load(std::sync::atomic::Ordering::SeqCst);
                    if ui
                        .add_enabled(!testing, egui::Button::new(if testing {
                            "Testing…"
                        } else {
                            "Test snapshot"
                        }))
                        .on_hover_text(
                            "Fetch a still and play the default alert sound (unless muted). Not saved to disk.",
                        )
                        .clicked()
                    {
                        self.test_anpr_snapshot();
                    }
                });
                if let Some(msg) = self.anpr_status.clone().or_else(|| self.anpr.status()) {
                    ui.label(egui::RichText::new(msg).small().weak());
                }
                    }
                    SettingsTab::Display => {
                ui.heading("Decoder");
                ui.label(
                    egui::RichText::new(
                        "How video is decoded. Changing this restarts live streams.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    for mode in [
                        crate::gst_link::DecodeBackend::Software,
                        crate::gst_link::DecodeBackend::Nvdec,
                        crate::gst_link::DecodeBackend::Hardware,
                    ] {
                        if ui
                            .selectable_label(self.decode_backend == mode, mode.label())
                            .on_hover_text(mode.hint())
                            .clicked()
                        {
                            if self.decode_backend != mode {
                                self.decode_backend = mode;
                                save_ui = true;
                            }
                        }
                    }
                });
                let nv_ok = crate::gst_env::nvdec_available();
                let env_override = crate::gst_link::DecodeBackend::from_env();
                let status = if let Some(env) = env_override {
                    format!(
                        "RUSTCAMS_DECODE={} overrides Settings (effective: {}).",
                        env.as_str(),
                        env.label()
                    )
                } else if self.decode_backend == crate::gst_link::DecodeBackend::Nvdec && !nv_ok {
                    "NVDEC plugin not found — install gst-plugin-nvcodec (falls back to software)."
                        .to_string()
                } else if nv_ok {
                    "NVIDIA NVDEC plugin found (nvh264dec).".to_string()
                } else {
                    "NVIDIA NVDEC plugin not found on this machine.".to_string()
                };
                ui.label(egui::RichText::new(status).small().weak());
                ui.add_space(12.0);
                ui.separator();
                ui.heading("Grid decode width");
                ui.label(
                    egui::RichText::new(
                        "Max pixels on the long edge after scale. Dense grids stay on substreams.",
                    )
                    .small()
                    .weak(),
                );
                let mut wchg = false;
                wchg |= decode_slider(ui, "1×1", &mut self.decode_1);
                wchg |= decode_slider(ui, "1×1 HD", &mut self.decode_1_hd);
                wchg |= decode_slider(ui, "2 stacked", &mut self.decode_2);
                wchg |= decode_slider(ui, "2 stacked HD", &mut self.decode_2_hd);
                wchg |= decode_slider(ui, "2×2", &mut self.decode_2x2);
                wchg |= decode_slider(ui, "2×2 HD", &mut self.decode_2x2_hd);
                wchg |= decode_slider(ui, "3×3", &mut self.decode_3x3);
                wchg |= decode_slider(ui, "4×4", &mut self.decode_4x4);
                wchg |= decode_slider(ui, "5×5", &mut self.decode_5x5);
                wchg |= decode_slider(ui, "6×6", &mut self.decode_6x6);
                if wchg {
                    save_ui = true;
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
                        save_ui = true;
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
                            save_ui = true;
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
                    save_ui = true;
                }
                ui.add_space(6.0);
                if ui.button("Reset appearance").clicked() {
                    self.accent = DEFAULT_ACCENT;
                    self.outline_width = default_outline_width();
                    self.decode_1 = default_w1();
                    self.decode_1_hd = default_w1_hd();
                    self.decode_2 = default_w2();
                    self.decode_2_hd = default_w2_hd();
                    self.decode_2x2 = default_w2x2();
                    self.decode_2x2_hd = default_w2x2_hd();
                    self.decode_3x3 = default_w3();
                    self.decode_4x4 = default_w4();
                    self.decode_5x5 = default_w5();
                    self.decode_6x6 = default_w6();
                    save_ui = true;
                }
                    }
                    SettingsTab::About => {
                ui.heading("About");
                ui.add_space(4.0);
                ui.label("Made by Sam Duff");
                ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                ui.add_space(12.0);

                let is_appimage = crate::update::appimage_path().is_some();
                if is_appimage {
                    if ui
                        .checkbox(&mut self.check_updates, "Check for updates on launch")
                        .changed()
                    {
                        save_ui = true;
                        if self.check_updates {
                            self.update_later = false;
                            self.kick_update_check(false);
                        } else {
                            self.update_later = true;
                            if !matches!(
                                self.update_banner,
                                super::UpdateBanner::Downloading { .. }
                            ) {
                                self.update_banner = super::UpdateBanner::Hidden;
                            }
                        }
                    }
                    ui.label(
                        egui::RichText::new(
                            "Looks up the latest GitHub release. You confirm before download.",
                        )
                        .small()
                        .weak(),
                    );
                    ui.add_space(8.0);
                } else {
                    ui.label(
                        egui::RichText::new(
                            "Update checks are available when running the Linux AppImage.",
                        )
                        .small()
                        .weak(),
                    );
                    ui.add_space(8.0);
                }

                let checking =
                    matches!(self.update_check_status, super::UpdateCheckStatus::Checking)
                        || self
                            .update_check_inflight
                            .load(std::sync::atomic::Ordering::SeqCst);
                if ui
                    .add_enabled(!checking, egui::Button::new(if checking {
                        "Checking…"
                    } else {
                        "Check for update"
                    }))
                    .clicked()
                {
                    self.kick_update_check(true);
                }

                ui.add_space(6.0);
                match &self.update_check_status {
                    super::UpdateCheckStatus::Idle => {}
                    super::UpdateCheckStatus::Checking => {
                        ui.label(egui::RichText::new("Checking for updates…").small().weak());
                    }
                    super::UpdateCheckStatus::UpToDate => {
                        ui.label(
                            egui::RichText::new("You're up to date.")
                                .small()
                                .color(Color32::from_rgb(140, 220, 160)),
                        );
                    }
                    super::UpdateCheckStatus::Available { version } => {
                        ui.label(
                            egui::RichText::new(format!("Update {version} available"))
                                .small()
                                .color(Color32::from_rgb(220, 200, 120)),
                        );
                    }
                    super::UpdateCheckStatus::Failed { message } => {
                        ui.label(
                            egui::RichText::new(message)
                                .small()
                                .color(Color32::from_rgb(255, 160, 120)),
                        );
                    }
                }
                    }
                }
            });
        self.show_settings = open;
        if save_ui {
            self.save_ui_prefs();
        }
        if save_nvr {
            self.apply_nvr_from_settings();
        }
        if save_gate {
            self.apply_gate_from_settings();
        }
        if save_anpr {
            self.apply_anpr_from_settings();
        }
    }
}

fn decode_slider(ui: &mut egui::Ui, label: &str, value: &mut i32) -> bool {
    let mut v = *value as f32;
    let resp = ui.add(
        egui::Slider::new(&mut v, 160.0..=1920.0)
            .text(label)
            .suffix(" px")
            .integer(),
    );
    if resp.changed() {
        *value = clamp_decode_width(v as i32);
        true
    } else {
        false
    }
}

/// Seed NVR edit fields from loaded `cameras.toml`.
pub fn nvr_edit_state(cfg: &AppConfig) -> (bool, NvrConfig, String) {
    match &cfg.nvr {
        Some(nvr) => (
            nvr.enabled,
            nvr.clone(),
            nvr.protocols.clone().unwrap_or_default(),
        ),
        None => (false, NvrConfig::default(), String::new()),
    }
}
