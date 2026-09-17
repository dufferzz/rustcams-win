use super::ViewerApp;
use crate::config::{AppConfig, CameraEntry, NvrConfig, StreamType, ViewerConfig};
use eframe::egui;
use egui::Color32;

#[derive(Debug, Clone)]
pub(super) struct SettingsState {
    pub open: bool,
    pub draft: AppConfig,
    pub nvr_enabled: bool,
    pub status: Option<(bool, String)>,
    pub remove_idx: Option<usize>,
}

impl SettingsState {
    pub fn from_config(cfg: AppConfig) -> Self {
        let nvr_enabled = cfg.nvr.is_some();
        let mut draft = cfg;
        if draft.nvr.is_none() {
            draft.nvr = Some(NvrConfig::default());
        }
        Self {
            open: false,
            draft,
            nvr_enabled,
            status: None,
            remove_idx: None,
        }
    }

    pub fn open_editor(&mut self, cfg: AppConfig) {
        *self = Self::from_config(cfg);
        self.open = true;
    }

    /// Build the AppConfig to save/apply (drops empty NVR when disabled).
    pub fn to_save(&self) -> AppConfig {
        let mut cfg = self.draft.clone();
        if !self.nvr_enabled {
            cfg.nvr = None;
        }
        cfg.cameras.retain(|c| !c.id.trim().is_empty() || !c.url.trim().is_empty());
        cfg
    }
}

impl ViewerApp {
    pub(super) fn open_settings(&mut self) {
        let cfg = self.editable_config();
        self.settings.open_editor(cfg);
    }

    fn editable_config(&self) -> AppConfig {
        match AppConfig::read(&self.config_path) {
            Ok(cfg) => cfg,
            Err(_) => AppConfig {
                nvr: None,
                cameras: Vec::new(),
                viewer: ViewerConfig {
                    app_name: self.app_name.clone(),
                    default_layout: self.active_layout().as_str().into(),
                    default_fit: self.fit.as_str().into(),
                    pause_when_unfocused: self.pause_when_unfocused,
                },
            },
        }
    }

    pub(super) fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings.open {
            return;
        }

        let mut open = self.settings.open;
        egui::Window::new("⚙ Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([720.0, 560.0])
            .show(ctx, |ui| {
                self.draw_settings_body(ui);
            });
        self.settings.open = open;

        if let Some(idx) = self.settings.remove_idx.take() {
            if idx < self.settings.draft.cameras.len() {
                self.settings.draft.cameras.remove(idx);
            }
        }
    }

    fn draw_settings_body(&mut self, ui: &mut egui::Ui) {
        ui.label(format!("Config file: {}", self.config_path.display()));
        ui.separator();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.heading("NVR");
                ui.checkbox(&mut self.settings.nvr_enabled, "Use Hikvision NVR");
                if self.settings.nvr_enabled {
                    let nvr = self
                        .settings
                        .draft
                        .nvr
                        .get_or_insert_with(NvrConfig::default);
                    ui.horizontal(|ui| {
                        ui.label("Host");
                        ui.add(
                            egui::TextEdit::singleline(&mut nvr.host).desired_width(180.0),
                        );
                        ui.label("HTTP");
                        ui.add(egui::DragValue::new(&mut nvr.http_port).range(1..=65535));
                        ui.label("RTSP");
                        ui.add(egui::DragValue::new(&mut nvr.rtsp_port).range(1..=65535));
                    });
                    ui.horizontal(|ui| {
                        ui.label("Username");
                        ui.add(
                            egui::TextEdit::singleline(&mut nvr.username).desired_width(120.0),
                        );
                        ui.label("Password");
                        ui.add(
                            egui::TextEdit::singleline(&mut nvr.password)
                                .password(true)
                                .desired_width(160.0),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("Stream");
                        egui::ComboBox::from_id_salt("nvr_stream")
                            .selected_text(nvr.stream.as_str())
                            .show_ui(ui, |ui| {
                                for s in [StreamType::Main, StreamType::Sub, StreamType::Third] {
                                    ui.selectable_value(&mut nvr.stream, s, s.as_str());
                                }
                            });
                        ui.label("Protocols");
                        let mut protocols = nvr.protocols.clone().unwrap_or_default();
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut protocols)
                                    .hint_text("udp | tcp | udp+tcp")
                                    .desired_width(140.0),
                            )
                            .changed()
                        {
                            let trimmed = protocols.trim();
                            nvr.protocols = if trimmed.is_empty() {
                                None
                            } else {
                                Some(trimmed.to_string())
                            };
                        }
                    });
                }

                ui.add_space(8.0);
                ui.heading("Viewer");
                {
                    let viewer = &mut self.settings.draft.viewer;
                    ui.horizontal(|ui| {
                        ui.label("App name");
                        ui.add(
                            egui::TextEdit::singleline(&mut viewer.app_name)
                                .desired_width(220.0)
                                .hint_text("Citadel CCTV"),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("Default layout");
                        egui::ComboBox::from_id_salt("viewer_layout")
                            .selected_text(viewer.default_layout.as_str())
                            .show_ui(ui, |ui| {
                                for layout in ["1", "2", "2x2", "3x3", "4x4", "5x5", "6x6"] {
                                    ui.selectable_value(
                                        &mut viewer.default_layout,
                                        layout.to_string(),
                                        layout,
                                    );
                                }
                            });
                        ui.label("Fit");
                        egui::ComboBox::from_id_salt("viewer_fit")
                            .selected_text(viewer.default_fit.as_str())
                            .show_ui(ui, |ui| {
                                for fit in ["contain", "cover", "fill"] {
                                    ui.selectable_value(
                                        &mut viewer.default_fit,
                                        fit.to_string(),
                                        fit,
                                    );
                                }
                            });
                    });
                    ui.checkbox(
                        &mut viewer.pause_when_unfocused,
                        "Pause streams when unfocused",
                    );
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.heading("Cameras");
                    if ui.button("Add camera").clicked() {
                        let n = self.settings.draft.cameras.len() + 1;
                        self.settings.draft.cameras.push(CameraEntry {
                            id: format!("cam_{n}"),
                            name: Some(format!("Camera {n}")),
                            url: String::new(),
                            channel: None,
                            stream: None,
                            protocols: None,
                        });
                    }
                });
                ui.label(
                    "Each camera needs id + RTSP url (rtsp://user:pass@host/.../Channels/N).",
                );

                let mut remove_idx = None;
                for (i, cam) in self.settings.draft.cameras.iter_mut().enumerate() {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label("id");
                            ui.add(
                                egui::TextEdit::singleline(&mut cam.id).desired_width(120.0),
                            );
                            ui.label("name");
                            let mut name = cam.name.clone().unwrap_or_default();
                            if ui
                                .add(egui::TextEdit::singleline(&mut name).desired_width(160.0))
                                .changed()
                            {
                                cam.name = if name.trim().is_empty() {
                                    None
                                } else {
                                    Some(name)
                                };
                            }
                            if ui.small_button("Remove").clicked() {
                                remove_idx = Some(i);
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("url");
                            ui.add(
                                egui::TextEdit::singleline(&mut cam.url)
                                    .desired_width(f32::INFINITY)
                                    .hint_text(
                                        "rtsp://admin:pass@192.168.x.x:554/Streaming/Channels/101",
                                    ),
                            );
                        });
                    });
                }
                self.settings.remove_idx = remove_idx;
            });

        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                self.settings_save_only();
            }
            if ui.button("Save & Apply").clicked() {
                self.settings_save_and_apply(ui.ctx());
            }
            if ui.button("Close").clicked() {
                self.settings.open = false;
            }
        });

        if let Some((ok, msg)) = &self.settings.status {
            let color = if *ok {
                Color32::from_rgb(120, 200, 120)
            } else {
                Color32::from_rgb(240, 120, 100)
            };
            ui.colored_label(color, msg);
        }
    }

    fn settings_save_only(&mut self) {
        let cfg = self.settings.to_save();
        match cfg.save(&self.config_path) {
            Ok(()) => {
                self.settings.status = Some((
                    true,
                    format!("Saved {}", self.config_path.display()),
                ));
                self.config_warning = None;
            }
            Err(err) => {
                self.settings.status = Some((false, format!("Save failed: {err:#}")));
            }
        }
    }

    fn settings_save_and_apply(&mut self, ctx: &egui::Context) {
        let cfg = self.settings.to_save();
        if let Err(err) = cfg.save(&self.config_path) {
            self.settings.status = Some((false, format!("Save failed: {err:#}")));
            return;
        }
        match cfg.resolve() {
            Ok(resolved) => {
                self.apply_resolved(resolved);
                self.apply_app_title(ctx);
                self.config_warning = None;
                self.settings.status = Some((true, "Saved and applied.".into()));
            }
            Err(err) => {
                self.settings.status = Some((
                    false,
                    format!("Saved, but apply failed: {err:#}"),
                ));
            }
        }
    }
}
