use super::{DragPayload, SidebarControls, ViewerApp};
use crate::config::CameraConfig;
use crate::layout::{FitMode, Layout};
use crate::ptz::{PresetListStatus, PtzVector, PTZ_MOVE_SPEED, PTZ_ZOOM_SPEED};
use crate::stream::log_stream_debug_rows;
use crate::views::View;
use eframe::egui;
use egui::{Color32, Id, Rect, Sense, Vec2};
use std::time::Instant;
use tracing::info;

impl ViewerApp {
    pub(super) fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(&self.app_name);
            ui.separator();

            let view_names: Vec<String> = self.views.views.iter().map(|v| v.name.clone()).collect();
            let mut active = self.views.active;
            egui::ComboBox::from_id_salt("view_select")
                .selected_text(
                    view_names
                        .get(active)
                        .cloned()
                        .unwrap_or_else(|| "View".into()),
                )
                .show_ui(ui, |ui| {
                    for (i, name) in view_names.iter().enumerate() {
                        if ui.selectable_label(i == active, name).clicked() {
                            active = i;
                        }
                    }
                });
            if active != self.views.active {
                self.views.active = active;
                self.exit_fullscreen();
                self.views.mark_dirty();
                let _ = self.views.save_if_dirty();
            }

            if ui
                .small_button("➕")
                .on_hover_text("New view")
                .clicked()
            {
                let name = format!("View {}", self.views.views.len() + 1);
                let layout = self.active_layout();
                self.views.views.push(View::new(name, layout));
                self.views.active = self.views.views.len() - 1;
                self.exit_fullscreen();
                self.views.mark_dirty();
                let _ = self.views.save_if_dirty();
            }

            if ui
                .small_button("✎")
                .on_hover_text("Rename view")
                .clicked()
            {
                self.rename_buffer = self.views.active_view().name.clone();
                self.show_rename = true;
            }

            let can_delete = self.views.views.len() > 1;
            if ui
                .add_enabled(can_delete, egui::Button::new("🗑").small())
                .on_hover_text("Delete view")
                .clicked()
            {
                let idx = self.views.active;
                self.views.views.remove(idx);
                if self.views.active >= self.views.views.len() {
                    self.views.active = self.views.views.len() - 1;
                }
                self.exit_fullscreen();
                self.views.mark_dirty();
                let _ = self.views.save_if_dirty();
            }

            ui.separator();

            let current_layout = self.active_layout();
            for layout in Layout::all() {
                if ui
                    .selectable_label(
                        current_layout == *layout && self.fullscreen_slot.is_none(),
                        layout.label(),
                    )
                    .on_hover_text(match layout {
                        Layout::One => "Single camera (1)",
                        Layout::Two => "Two cameras stacked (vertical split) (2)",
                        Layout::Grid2 => "2×2 grid",
                        Layout::Grid3 => "3×3 grid (3)",
                        Layout::Grid4 => "4×4 grid (4)",
                        Layout::Grid5 => "5×5 grid (5)",
                        Layout::Grid6 => "6×6 grid (6)",
                    })
                    .clicked()
                {
                    self.set_layout(*layout);
                }
            }

            ui.separator();
            let hd_allowed = self.hd_allowed();
            let hd_response = ui
                .add_enabled(
                    hd_allowed,
                    egui::SelectableLabel::new(self.hd && hd_allowed, "HD"),
                )
                .on_hover_text(if self.fullscreen_slot.is_some() {
                    "HD: main stream at higher decode width. Off: sub stream. Available in fullscreen."
                } else if hd_allowed {
                    "HD: main stream (…01 / 101). Off: sub stream (…02 / 102). Available on 1, 2, and 2×2."
                } else {
                    "HD disabled on dense grids (3×3+) — always use substreams to save CPU. Fullscreen to enable."
                });
            if hd_allowed && hd_response.clicked() {
                self.hd = !self.hd;
                info!(hd = self.hd, "stream quality toggled");
            }

            ui.separator();
            if ui
                .button(format!("Fit: {}", self.fit.label()))
                .on_hover_text("Cycle fit mode (F): Contain → Cover → Fill")
                .clicked()
            {
                self.fit = self.fit.cycle();
            }

            ui.separator();
            if ui
                .selectable_label(self.window_fullscreen, "⛶")
                .on_hover_text("OS / monitor fullscreen (Esc to exit)")
                .clicked()
            {
                let on = !self.window_fullscreen;
                self.set_window_fullscreen(ui.ctx(), on);
            }

            ui.separator();
            if ui
                .selectable_label(self.debug_overlay, "🐛")
                .on_hover_text("Perf overlay + stream metrics (D). Also: RUSTCAMS_DEBUG=1")
                .clicked()
            {
                self.debug_overlay = !self.debug_overlay;
                info!(debug = self.debug_overlay, "debug overlay toggled");
            }

            if ui
                .selectable_label(self.show_log, "📋")
                .on_hover_text("Log console (L) — replaces the hidden Windows console")
                .clicked()
            {
                self.show_log = !self.show_log;
            }

            ui.separator();
            if ui
                .button("⚙")
                .on_hover_text("Settings — edit cameras.toml")
                .clicked()
            {
                self.open_settings();
            }

            if self.fullscreen_slot.is_some() {
                ui.separator();
                if ui
                    .button("↩")
                    .on_hover_text("Exit camera fullscreen")
                    .clicked()
                {
                    self.exit_fullscreen();
                }
                if let Some(slot) = self.fullscreen_slot {
                    let label = self
                        .views
                        .active_view()
                        .slots
                        .get(slot)
                        .and_then(|s| s.as_ref())
                        .and_then(|id| self.camera_by_id(id))
                        .map(|c| {
                            if c.ptz.is_some() {
                                format!("PTZ: {}", c.name)
                            } else {
                                format!("{} (no PTZ)", c.name)
                            }
                        })
                        .unwrap_or_else(|| "PTZ: —".into());
                    ui.colored_label(Color32::from_rgb(255, 190, 70), label);
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.paused {
                    ui.colored_label(Color32::from_rgb(255, 180, 80), "⏸ Paused (background)");
                }
            });
        });

        if self.show_rename {
            egui::Window::new("Rename view")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.text_edit_singleline(&mut self.rename_buffer);
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            let name = self.rename_buffer.trim().to_string();
                            if !name.is_empty() {
                                self.views.active_view_mut().name = name;
                                self.views.mark_dirty();
                                let _ = self.views.save_if_dirty();
                            }
                            self.show_rename = false;
                        }
                        if ui.button("Cancel").clicked() {
                            self.show_rename = false;
                        }
                    });
                });
        }
    }

    pub(super) fn camera_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("📷 Cameras");
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.sidebar_filter)
                .hint_text("Filter…")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(4.0);

        let filter = self.sidebar_filter.to_ascii_lowercase();
        let active_ids = self.view_camera_ids();
        let selected_ptz = self.sidebar_ptz_cam.clone();
        let cameras: Vec<(String, String, bool, bool)> = self
            .cameras
            .iter()
            .filter(|c| {
                filter.is_empty()
                    || c.name.to_ascii_lowercase().contains(&filter)
                    || c.id.to_ascii_lowercase().contains(&filter)
            })
            .map(|c| {
                (
                    c.id.clone(),
                    c.name.clone(),
                    active_ids.contains(&c.id),
                    c.ptz.is_some(),
                )
            })
            .collect();

        // Top: camera library. Bottom: PTZ / Presets (fixed-ish share).
        let available = ui.available_height();
        let controls_h = (available * 0.42).clamp(160.0, 280.0);
        let list_h = (available - controls_h - 8.0).max(80.0);

        egui::ScrollArea::vertical()
            .id_salt("camera_library")
            .max_height(list_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (id, name, on_view, has_ptz) in cameras {
                    let item_id = Id::new("lib_cam").with(&id);
                    let payload = DragPayload::FromLibrary(id.clone());
                    let is_ptz_sel = selected_ptz.as_deref() == Some(id.as_str());
                    let response = ui.dnd_drag_source(item_id, payload, |ui| {
                        let mark = if on_view { "●" } else { "○" };
                        let label = if has_ptz {
                            format!("{mark}  {name}  ⌖")
                        } else {
                            format!("{mark}  {name}")
                        };
                        let color = if is_ptz_sel {
                            Color32::from_rgb(255, 200, 120)
                        } else if on_view {
                            Color32::from_rgb(140, 200, 255)
                        } else {
                            Color32::from_rgb(210, 210, 210)
                        };
                        ui.add(
                            egui::Label::new(egui::RichText::new(label).color(color))
                                .sense(Sense::click_and_drag()),
                        )
                    });
                    let hover = if has_ptz {
                        "Drag onto a grid cell · click to select for PTZ"
                    } else {
                        "Drag onto a grid cell"
                    };
                    let clicked = response.inner.clicked();
                    response.inner.on_hover_text(hover);
                    if has_ptz && clicked {
                        self.select_ptz_camera(&id);
                    }
                    ui.add_space(2.0);
                }
            });

        ui.add_space(6.0);
        ui.separator();
        self.sidebar_ptz_panel(ui);
    }

    fn sidebar_ptz_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if ui
                .selectable_label(
                    self.sidebar_controls == SidebarControls::Ptz,
                    "PTZ",
                )
                .clicked()
            {
                self.sidebar_controls = SidebarControls::Ptz;
            }
            if ui
                .selectable_label(
                    self.sidebar_controls == SidebarControls::Presets,
                    "Presets",
                )
                .clicked()
            {
                self.sidebar_controls = SidebarControls::Presets;
                if let Some(target) = self.active_ptz_target() {
                    self.ptz.fetch_presets(target);
                }
            }
        });

        let cam_label = self
            .sidebar_ptz_cam
            .as_ref()
            .and_then(|id| self.camera_by_id(id))
            .map(|c| c.name.clone());
        let target = self.active_ptz_target();

        match (&cam_label, &target) {
            (Some(name), Some(_)) => {
                ui.label(
                    egui::RichText::new(name)
                        .small()
                        .color(Color32::from_rgb(180, 190, 200)),
                );
            }
            _ => {
                ui.label(
                    egui::RichText::new("Select a PTZ camera above")
                        .small()
                        .color(Color32::from_rgb(140, 140, 140)),
                );
                return;
            }
        }

        ui.add_space(4.0);
        match self.sidebar_controls {
            SidebarControls::Ptz => self.sidebar_ptz_pad(ui),
            SidebarControls::Presets => self.sidebar_presets_list(ui),
        }
    }

    fn sidebar_ptz_pad(&mut self, ui: &mut egui::Ui) {
        let mut pan = 0i32;
        let mut tilt = 0i32;
        let mut zoom = 0i32;

        let btn = |ui: &mut egui::Ui, label: &str| -> bool {
            ui.add_sized(
                [36.0, 28.0],
                egui::Button::new(egui::RichText::new(label).size(14.0)),
            )
            .is_pointer_button_down_on()
        };

        ui.vertical_centered(|ui| {
            ui.horizontal(|ui| {
                ui.add_space(36.0);
                if btn(ui, "▲") {
                    tilt = PTZ_MOVE_SPEED;
                }
            });
            ui.horizontal(|ui| {
                if btn(ui, "◀") {
                    pan = -PTZ_MOVE_SPEED;
                }
                if ui
                    .add_sized([36.0, 28.0], egui::Button::new("⌂"))
                    .on_hover_text("Home")
                    .clicked()
                {
                    if let Some(target) = self.active_ptz_target() {
                        self.ptz_stop();
                        self.ptz.home(target);
                    }
                }
                if btn(ui, "▶") {
                    pan = PTZ_MOVE_SPEED;
                }
            });
            ui.horizontal(|ui| {
                ui.add_space(36.0);
                if btn(ui, "▼") {
                    tilt = -PTZ_MOVE_SPEED;
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if btn(ui, "−") {
                    zoom = -PTZ_ZOOM_SPEED;
                }
                ui.label(
                    egui::RichText::new("Zoom")
                        .small()
                        .color(Color32::from_rgb(150, 150, 150)),
                );
                if btn(ui, "+") {
                    zoom = PTZ_ZOOM_SPEED;
                }
            });
        });

        let held = PtzVector {
            pan,
            tilt,
            zoom,
            focus: 0,
        };
        let pad_id = ui.id().with("ptz_pad_hold");
        let was_holding = ui
            .ctx()
            .data(|d| d.get_temp::<bool>(pad_id).unwrap_or(false));
        if !held.is_stop() {
            self.apply_ptz_vector(held);
            ui.ctx().data_mut(|d| d.insert_temp(pad_id, true));
        } else if was_holding {
            self.apply_ptz_vector(PtzVector::STOP);
            ui.ctx().data_mut(|d| d.insert_temp(pad_id, false));
        }
    }

    fn sidebar_presets_list(&mut self, ui: &mut egui::Ui) {
        let Some(target) = self.active_ptz_target() else {
            return;
        };

        ui.horizontal(|ui| {
            if ui
                .small_button("↻")
                .on_hover_text("Refresh presets from camera")
                .clicked()
            {
                self.ptz.refresh_presets(target.clone());
            }
            match self.ptz.preset_status(&target) {
                PresetListStatus::Idle => {
                    self.ptz.fetch_presets(target.clone());
                    ui.label(
                        egui::RichText::new("Loading…")
                            .small()
                            .color(Color32::from_rgb(160, 160, 160)),
                    );
                }
                PresetListStatus::Loading => {
                    ui.ctx().request_repaint();
                    ui.label(
                        egui::RichText::new("Loading…")
                            .small()
                            .color(Color32::from_rgb(160, 160, 160)),
                    );
                }
                PresetListStatus::Error(err) => {
                    ui.label(
                        egui::RichText::new(err)
                            .small()
                            .color(Color32::from_rgb(255, 140, 120)),
                    );
                }
                PresetListStatus::Ready(list) => {
                    ui.label(
                        egui::RichText::new(format!("{} presets", list.len()))
                            .small()
                            .color(Color32::from_rgb(140, 160, 140)),
                    );
                }
            }
        });

        let status = self.ptz.preset_status(&target);
        let PresetListStatus::Ready(presets) = status else {
            return;
        };

        if presets.is_empty() {
            ui.label(
                egui::RichText::new("No enabled presets on camera")
                    .small()
                    .color(Color32::from_rgb(140, 140, 140)),
            );
            return;
        }

        egui::ScrollArea::vertical()
            .id_salt("ptz_presets")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for preset in &presets {
                    let label = format!("{:>3}  {}", preset.id, preset.name);
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(label)
                                    .color(Color32::from_rgb(220, 220, 220)),
                            )
                            .frame(false)
                            .min_size(Vec2::new(ui.available_width(), 0.0)),
                        )
                        .clicked()
                    {
                        if let Some(t) = self.active_ptz_target() {
                            self.ptz_stop();
                            self.ptz.goto_preset(t, preset.id);
                        }
                    }
                }
            });
    }

    pub(super) fn status_bar(&mut self, ui: &mut egui::Ui) {
        self.stats.refresh_if_due(false);
        let streams = if self.paused {
            0
        } else {
            self.view_camera_ids().len()
        };
        let view_name = self.views.active_view().name.clone();
        let layout = self.active_layout().label();
        let text = self.stats.summary_line(streams, &view_name, layout);

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(text).monospace().size(12.0));
            if self.paused {
                ui.separator();
                ui.colored_label(Color32::from_rgb(255, 180, 80), "background pause");
            }
            if self.debug_overlay {
                ui.separator();
                ui.label(
                    egui::RichText::new(format!(
                        "UI {:.0}fps  tex {:.0}/s {:.1}MiB/s  clone {:.0}/s",
                        self.ui_perf.ui_fps,
                        self.ui_perf.upload_fps,
                        self.ui_perf.upload_mbps,
                        self.ui_perf.clone_fps,
                    ))
                    .monospace()
                    .size(12.0)
                    .color(Color32::from_rgb(120, 220, 160)),
                );
            }
        });
    }
    pub(super) fn refresh_debug(&mut self) {
        if !self.debug_overlay {
            return;
        }
        if self.last_debug_sample.elapsed() < std::time::Duration::from_millis(1000) {
            return;
        }
        self.ui_perf.sample_rates();
        self.debug_rows = self.streams.debug_snapshot();
        self.last_debug_sample = Instant::now();

        if self.last_debug_log.elapsed() >= std::time::Duration::from_secs(5) {
            info!(
                ui_fps = format!("{:.1}", self.ui_perf.ui_fps),
                tex_fps = format!("{:.1}", self.ui_perf.upload_fps),
                tex_mib_s = format!("{:.1}", self.ui_perf.upload_mbps),
                tex_upload_us = format!("{:.0}", self.ui_perf.avg_upload_us),
                tex_clone_fps = format!("{:.1}", self.ui_perf.clone_fps),
                layout = self.active_layout().as_str(),
                hd = self.hd,
                camera_fs = self.fullscreen_slot.is_some(),
                "perf ui"
            );
            log_stream_debug_rows(&self.debug_rows);
            self.last_debug_log = Instant::now();
        }
    }

    pub(super) fn draw_debug_panel(&mut self, ctx: &egui::Context) {
        if !self.debug_overlay {
            return;
        }
        egui::Window::new("🐛 Perf debug")
            .default_width(720.0)
            .default_height(360.0)
            .vscroll(true)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "UI {:.1} fps │ tex uploads {:.1}/s ({:.1} MiB/s, avg {:.0} µs) │ Arc clone {:.1}/s │ D toggles",
                        self.ui_perf.ui_fps,
                        self.ui_perf.upload_fps,
                        self.ui_perf.upload_mbps,
                        self.ui_perf.avg_upload_us,
                        self.ui_perf.clone_fps,
                    ))
                    .monospace()
                    .size(12.0),
                );
                ui.label(
                    egui::RichText::new(
                        "stale = appsink replaced a frame the UI never consumed (decode ahead of display)",
                    )
                    .size(11.0)
                    .color(Color32::from_rgb(160, 160, 160)),
                );
                ui.separator();

                let sum_fps: f32 = self.debug_rows.iter().map(|r| r.fps).sum();
                let sum_stale: f32 = self.debug_rows.iter().map(|r| r.stale_fps).sum();
                let sum_mbps: f32 = self.debug_rows.iter().map(|r| r.rgba_mbps).sum();
                ui.label(
                    egui::RichText::new(format!(
                        "streams {} │ decode {:.1} fps │ stale {:.1}/s │ RGBA copy {:.1} MiB/s",
                        self.debug_rows.len(),
                        sum_fps,
                        sum_stale,
                        sum_mbps,
                    ))
                    .monospace()
                    .strong(),
                );
                ui.add_space(6.0);

                egui::Grid::new("perf_grid")
                    .striped(true)
                    .min_col_width(56.0)
                    .show(ui, |ui| {
                        ui.label("camera");
                        ui.label("run");
                        ui.label("tier");
                        ui.label("size");
                        ui.label("fps");
                        ui.label("stale");
                        ui.label("MiB/s");
                        ui.label("copyµs");
                        ui.label("dec");
                        ui.label("err");
                        ui.end_row();

                        for row in &self.debug_rows {
                            ui.label(&row.id);
                            ui.label(if row.running { "Y" } else { "N" });
                            ui.label(format!("{}@{}", row.max_width, row.max_fps));
                            ui.label(format!("{}x{}", row.width, row.height));
                            ui.label(format!("{:.1}", row.fps));
                            ui.label(format!("{:.1}", row.stale_fps));
                            ui.label(format!("{:.2}", row.rgba_mbps));
                            ui.label(format!("{:.0}", row.avg_copy_us));
                            ui.label(if row.decoder.is_empty() {
                                "?"
                            } else {
                                row.decoder.as_str()
                            });
                            ui.colored_label(
                                if row.error.is_some() {
                                    Color32::from_rgb(255, 120, 100)
                                } else {
                                    Color32::from_rgb(180, 180, 180)
                                },
                                row.error.as_deref().unwrap_or("-"),
                            );
                            ui.end_row();
                        }
                    });
            });
    }

    pub(super) fn draw_log_panel(&mut self, ctx: &egui::Context) {
        if !self.show_log {
            return;
        }

        let (gen, lines) = self.log_buffer.snapshot();
        if gen != self.log_view_generation {
            self.log_view_generation = gen;
            self.log_view_lines = lines;
        }

        let mut open = self.show_log;
        egui::Window::new("📋 Log")
            .open(&mut open)
            .default_width(720.0)
            .default_height(320.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Clear").clicked() {
                        self.log_buffer.clear();
                        self.log_view_lines.clear();
                        self.log_view_generation = self.log_view_generation.wrapping_add(1);
                    }
                    ui.checkbox(&mut self.log_auto_scroll, "Auto-scroll");
                    ui.label(
                        egui::RichText::new(format!("{} lines", self.log_view_lines.len()))
                            .weak()
                            .small(),
                    );
                });
                ui.separator();

                let text = self.log_view_lines.join("\n");
                egui::ScrollArea::vertical()
                    .stick_to_bottom(self.log_auto_scroll)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(if text.is_empty() {
                                    "(no log lines yet)".into()
                                } else {
                                    text
                                })
                                .monospace()
                                .size(12.0),
                            )
                            .selectable(true),
                        );
                    });
            });
        self.show_log = open;

        // Keep refreshing while open so new log lines appear without waiting for other UI events.
        if self.show_log {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }

    pub(super) fn paint_cell_contents(
        &self,
        ui: &mut egui::Ui,
        cam: &CameraConfig,
        cell: Rect,
        hovered: bool,
    ) {
        ui.painter()
            .rect_filled(cell, 0.0, Color32::from_rgb(12, 14, 18));

        if let Some(tex) = self.textures.get(&cam.id) {
            let size = tex.handle.size_vec2();
            let aspect = if size.y > 0.0 {
                size.x / size.y
            } else {
                16.0 / 9.0
            };
            let draw = fitted_rect(cell, aspect, self.fit);
            let painter = ui.painter().with_clip_rect(cell);
            painter.image(
                tex.handle.id(),
                draw,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        } else {
            let msg = if self.paused {
                "Paused".to_string()
            } else if let Some(err) = self.streams.error(&cam.id) {
                format!("Error\n{err}")
            } else if self.streams.is_running(&cam.id) {
                "Connecting…".to_string()
            } else {
                "Offline".to_string()
            };
            ui.painter().text(
                cell.center(),
                egui::Align2::CENTER_CENTER,
                msg,
                egui::FontId::proportional(14.0),
                Color32::from_rgb(180, 180, 180),
            );
        }

        if hovered {
            let bar_h = 24.0;
            let bar = Rect::from_min_max(egui::pos2(cell.min.x, cell.max.y - bar_h), cell.max);
            ui.painter()
                .rect_filled(bar, 0.0, Color32::from_rgba_unmultiplied(0, 0, 0, 170));
            ui.painter().text(
                bar.left_center() + Vec2::new(8.0, 0.0),
                egui::Align2::LEFT_CENTER,
                &cam.name,
                egui::FontId::proportional(13.0),
                Color32::WHITE,
            );
        }
    }

    pub(super) fn draw_grid(&mut self, ui: &mut egui::Ui, full: Rect) {
        let layout = self.active_layout();
        let cols = layout.cols();
        let rows = layout.rows();
        // In monitor fullscreen, use every pixel and let adjacent feeds meet.
        let gap = if self.window_fullscreen { 0.0 } else { 2.0 };
        let cell_w = (full.width() - gap * (cols as f32 + 1.0)) / cols as f32;
        let cell_h = (full.height() - gap * (rows as f32 + 1.0)) / rows as f32;

        let slots = self.views.active_view().slots.clone();
        let mut drop_action: Option<(usize, DragPayload)> = None;
        let mut fullscreen: Option<usize> = None;
        let mut clear: Option<usize> = None;

        for (i, slot) in slots.iter().enumerate() {
            let col = i % cols;
            let row = i / cols;
            let x = full.min.x + gap + col as f32 * (cell_w + gap);
            let y = full.min.y + gap + row as f32 * (cell_h + gap);
            let cell = Rect::from_min_size(egui::pos2(x, y), Vec2::new(cell_w, cell_h));
            let id = Id::new("grid_slot").with(i);

            let sense = if slot.is_some() {
                Sense::click_and_drag()
            } else {
                Sense::click()
            };
            let response = ui.interact(cell, id, sense);

            if let Some(cam_id) = slot {
                if let Some(cam) = self.camera_by_id(cam_id) {
                    self.paint_cell_contents(ui, cam, cell, response.hovered());
                } else {
                    ui.painter()
                        .rect_filled(cell, 0.0, Color32::from_rgb(12, 14, 18));
                    ui.painter().text(
                        cell.center(),
                        egui::Align2::CENTER_CENTER,
                        "Missing camera",
                        egui::FontId::proportional(13.0),
                        Color32::from_rgb(180, 100, 100),
                    );
                }
                response.dnd_set_drag_payload(DragPayload::FromSlot(i));
            } else {
                ui.painter()
                    .rect_filled(cell, 0.0, Color32::from_rgb(12, 14, 18));
                ui.painter().text(
                    cell.center(),
                    egui::Align2::CENTER_CENTER,
                    "Drop camera here",
                    egui::FontId::proportional(13.0),
                    Color32::from_rgb(90, 95, 105),
                );
            }

            if response.dnd_hover_payload::<DragPayload>().is_some() {
                ui.painter().rect_stroke(
                    cell.shrink(1.0),
                    0.0,
                    egui::Stroke::new(2.0_f32, Color32::from_rgb(80, 160, 255)),
                    egui::StrokeKind::Inside,
                );
            }

            if let Some(payload) = response.dnd_release_payload::<DragPayload>() {
                drop_action = Some((i, (*payload).clone()));
            }
            if response.double_clicked() && slot.is_some() {
                fullscreen = Some(i);
            }
            if response.secondary_clicked() {
                clear = Some(i);
            }
        }

        if let Some((idx, payload)) = drop_action {
            self.apply_drop(idx, payload);
        }
        if let Some(idx) = fullscreen {
            self.enter_fullscreen(idx);
        }
        if let Some(idx) = clear {
            self.clear_slot(idx);
        }
    }
}

fn fitted_rect(cell: Rect, video_aspect: f32, fit: FitMode) -> Rect {
    let cell_aspect = cell.width() / cell.height().max(1.0);
    match fit {
        FitMode::Fill => cell,
        FitMode::Contain => {
            if video_aspect > cell_aspect {
                let h = cell.width() / video_aspect;
                Rect::from_center_size(cell.center(), Vec2::new(cell.width(), h))
            } else {
                let w = cell.height() * video_aspect;
                Rect::from_center_size(cell.center(), Vec2::new(w, cell.height()))
            }
        }
        FitMode::Cover => {
            if video_aspect > cell_aspect {
                let w = cell.height() * video_aspect;
                Rect::from_center_size(cell.center(), Vec2::new(w, cell.height()))
            } else {
                let h = cell.width() / video_aspect;
                Rect::from_center_size(cell.center(), Vec2::new(cell.width(), h))
            }
        }
    }
}
