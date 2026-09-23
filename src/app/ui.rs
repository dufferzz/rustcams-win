use super::icons;
use super::{DragPayload, SidebarControls, UpdateBanner, ViewerApp};
use crate::config::CameraConfig;
use crate::layout::{FitMode, Layout};
use crate::ptz::{
    ParkAction, ParkActionStatus, PresetListStatus, PtzVector, PTZ_MOVE_SPEED, PTZ_ZOOM_SPEED,
};
use crate::stream::log_stream_debug_rows;
use crate::views::View;
use eframe::egui;
use egui::{Color32, Id, Rect, Sense, Vec2};
use std::time::Instant;
use tracing::info;

use super::settings::{clamp_sidebar_controls_height, clamp_sidebar_width, CANVAS_BG, PANEL_BG};

impl ViewerApp {
    pub(super) fn toolbar(&mut self, ui: &mut egui::Ui, view_idx: usize, is_aux: bool) {
        ui.horizontal(|ui| {
            ui.heading(crate::config::app_screen_title(
                &self.app_name,
                if is_aux {
                    Some(2)
                } else {
                    self.aux_open.then_some(1)
                },
            ));
            ui.separator();

            let view_names: Vec<String> = self.views.views.iter().map(|v| v.name.clone()).collect();
            let mut active = view_idx;
            egui::ComboBox::from_id_salt(("view_select", is_aux))
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
            if active != view_idx {
                if is_aux {
                    self.aux_view = active;
                    self.aux_fullscreen_slot = None;
                } else {
                    self.views.active = active;
                    self.exit_fullscreen();
                }
                self.mark_views_dirty();
                self.save_ui_prefs();
            }

            if ui
                .small_button(icons::PLUS)
                .on_hover_text("New view")
                .clicked()
            {
                let name = format!("View {}", self.views.views.len() + 1);
                let layout = self.views.view(view_idx).layout;
                self.views.views.push(View::new(name, layout));
                let idx = self.views.views.len() - 1;
                if is_aux {
                    self.aux_view = idx;
                    self.aux_fullscreen_slot = None;
                } else {
                    self.views.active = idx;
                    self.exit_fullscreen();
                }
                self.mark_views_dirty();
                self.save_ui_prefs();
            }

            if !is_aux {
                if ui
                    .small_button(icons::PENCIL_SIMPLE)
                    .on_hover_text("Rename view")
                    .clicked()
                {
                    self.rename_buffer = self.views.active_view().name.clone();
                    self.show_rename = true;
                }
            }

            let can_delete = self.views.views.len() > 1;
            if ui
                .add_enabled(can_delete, egui::Button::new(icons::TRASH).small())
                .on_hover_text("Delete view")
                .clicked()
            {
                let idx = view_idx;
                self.views.views.remove(idx);
                if self.views.active >= self.views.views.len() {
                    self.views.active = self.views.views.len() - 1;
                }
                self.clamp_aux_view();
                self.exit_fullscreen();
                self.aux_fullscreen_slot = None;
                self.mark_views_dirty();
                self.save_ui_prefs();
            }

            {
                let dirty = self.views.is_dirty();
                let icon = if dirty {
                    egui::RichText::new(icons::FLOPPY_DISK).color(self.accent)
                } else {
                    egui::RichText::new(icons::FLOPPY_DISK)
                        .color(Color32::from_rgb(110, 115, 125))
                };
                if ui
                    .add_enabled(dirty, egui::Button::new(icon).small())
                    .on_hover_text(if dirty {
                        "Save views (unsaved changes)"
                    } else {
                        "Views saved"
                    })
                    .clicked()
                {
                    self.save_views_now();
                }
            }

            ui.separator();

            let current_layout = self.views.view(view_idx).layout;
            let no_cam_fs = if is_aux {
                self.aux_fullscreen_slot.is_none()
            } else {
                self.fullscreen_slot.is_none()
            };
            for layout in Layout::all() {
                if ui
                    .selectable_label(current_layout == *layout && no_cam_fs, layout.label())
                    .on_hover_text(match layout {
                        Layout::One => "Single camera",
                        Layout::Two => "Two cameras stacked (vertical split)",
                        Layout::Grid2 => "2×2 grid",
                        Layout::Grid3 => "3×3 grid",
                        Layout::Grid4 => "4×4 grid",
                        Layout::Grid5 => "5×5 grid",
                        Layout::Grid6 => "6×6 grid",
                    })
                    .clicked()
                {
                    self.set_layout_at(view_idx, *layout);
                }
            }

            ui.separator();
            let audio_label = if self.audio_enabled {
                icons::SPEAKER_HIGH
            } else {
                icons::SPEAKER_SLASH
            };
            let audio_tip = if !self.audio_enabled {
                "Audio off — click to hear the selected camera (PCMU/PCMA intercoms)".to_string()
            } else if let Some(id) = self.sidebar_ptz_cam.as_deref() {
                format!("Audio on — listening to {id}. Click to mute.")
            } else {
                "Audio on — select a camera to listen".to_string()
            };
            if ui
                .selectable_label(self.audio_enabled, audio_label)
                .on_hover_text(audio_tip)
                .clicked()
            {
                self.audio_enabled = !self.audio_enabled;
                self.save_ui_prefs();
                self.sync_listen_audio();
                info!(audio = self.audio_enabled, "camera audio toggled");
            }

            ui.separator();
            if self.gate_draft.is_configured() {
                let gate_busy = self.gate_busy();
                let gate_tip = if let Some(msg) = self.gate_status_text() {
                    format!("{msg} · Share · confirm with Enter or ✕")
                } else {
                    "Open gates (confirms first). DualShock Share, then Enter or ✕.".to_string()
                };
                if ui
                    .add_enabled(
                        !gate_busy && !self.pending_gate_confirm,
                        egui::Button::new(if gate_busy {
                            "Opening…".to_string()
                        } else {
                            icons::labeled(icons::DOOR_OPEN, "Open Gate")
                        }),
                    )
                    .on_hover_text(gate_tip)
                    .clicked()
                {
                    self.prompt_open_gates();
                }
                ui.separator();
            }
            if !is_aux {
                if ui
                    .selectable_label(self.window_fullscreen, icons::CORNERS_OUT)
                    .on_hover_text("OS / monitor fullscreen (Esc / right-click grid / Triangle to toggle)")
                    .clicked()
                {
                    let on = !self.window_fullscreen;
                    self.set_window_fullscreen(ui.ctx(), on);
                }
                if ui
                    .selectable_label(self.aux_open, icons::MONITOR)
                    .on_hover_text("Open a second window for another monitor (shared PTZ selection)")
                    .clicked()
                {
                    self.aux_open = !self.aux_open;
                    if self.aux_open {
                        self.ensure_distinct_aux_view();
                    }
                }
            } else {
                if ui
                    .selectable_label(self.aux_window_fullscreen, icons::CORNERS_OUT)
                    .on_hover_text("OS / monitor fullscreen for this window (Esc to toggle)")
                    .clicked()
                {
                    let on = !self.aux_window_fullscreen;
                    self.aux_window_fullscreen = on;
                }
                if ui
                    .button(icons::X)
                    .on_hover_text("Close second window")
                    .clicked()
                {
                    self.aux_open = false;
                }
            }

            let fs_slot = if is_aux {
                self.aux_fullscreen_slot
            } else {
                self.fullscreen_slot
            };
            if fs_slot.is_some() {
                ui.separator();
                if ui
                    .button(icons::ARROW_U_UP_LEFT)
                    .on_hover_text("Exit camera fullscreen")
                    .clicked()
                {
                    if is_aux {
                        self.exit_aux_fullscreen();
                    } else {
                        self.exit_fullscreen();
                    }
                }
                if let Some(slot) = fs_slot {
                    let label = self
                        .views
                        .view(view_idx)
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
                    ui.colored_label(self.accent, label);
                }
            }

            if !is_aux {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(icons::GEAR)
                        .on_hover_text("Settings — NVR, gates, decode width, log file, accent")
                        .clicked()
                    {
                        self.show_settings = !self.show_settings;
                    }
                    if self.paused {
                        ui.colored_label(
                            Color32::from_rgb(255, 180, 80),
                            icons::labeled(icons::PAUSE, "Paused (background)"),
                        );
                    }
                });
            }
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
                                self.mark_views_dirty();
                                self.save_ui_prefs();
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

    /// Side panel with an explicit width we own. Built-in egui resize fights widgets
    /// that allocate infinite width in horizontal layouts, so we use `exact_width`
    /// plus a custom drag handle on the right edge.
    pub(super) fn show_resizable_camera_sidebar(
        &mut self,
        ctx: &egui::Context,
        view_idx: usize,
        is_aux: bool,
        id: &'static str,
    ) {
        let width = clamp_sidebar_width(self.sidebar_width);
        let panel = egui::SidePanel::left(id)
            .exact_width(width)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(PANEL_BG)
                    .inner_margin(egui::Margin::symmetric(10, 8)),
            )
            .show(ctx, |ui| {
                ui.set_max_width(ui.max_rect().width());
                if !is_aux {
                    ui.horizontal(|ui| {
                        if ui
                            .small_button(icons::CARET_LEFT)
                            .on_hover_text("Collapse sidebar")
                            .clicked()
                        {
                            self.sidebar_open = false;
                            self.save_ui_prefs();
                        }
                    });
                }
                self.camera_sidebar(ui, view_idx, is_aux);
            });

        let rect = panel.response.rect;
        let resize_id = Id::new((id, "resize_handle"));
        egui::Area::new(resize_id)
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(rect.right() - 4.0, rect.top()))
            .interactable(true)
            .show(ctx, |ui| {
                let size = Vec2::new(8.0, rect.height());
                let (_r, response) = ui.allocate_exact_size(size, Sense::drag());
                if response.hovered() || response.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                    ui.painter().rect_filled(
                        response.rect,
                        0.0,
                        if response.dragged() {
                            self.accent.linear_multiply(0.55)
                        } else {
                            Color32::from_rgba_unmultiplied(
                                self.accent.r(),
                                self.accent.g(),
                                self.accent.b(),
                                40,
                            )
                        },
                    );
                }
                if response.dragged() {
                    let next =
                        clamp_sidebar_width(self.sidebar_width + response.drag_delta().x);
                    if (next - self.sidebar_width).abs() > 0.1 {
                        self.sidebar_width = next;
                    }
                }
                if response.drag_stopped() {
                    self.sidebar_width = clamp_sidebar_width(self.sidebar_width);
                    self.save_ui_prefs();
                }
            });
    }

    pub(super) fn camera_sidebar(&mut self, ui: &mut egui::Ui, view_idx: usize, is_aux: bool) {
        let mut save_groups = false;
        let mut lib_drop: Option<(String, usize, usize)> = None;
        let mut add_group = false;
        let mut delete_group: Option<usize> = None;
        let mut start_rename: Option<usize> = None;
        let mut finish_rename = false;
        let mut cancel_rename = false;
        let mut select_cam: Option<String> = None;
        let mut assign_cam: Option<String> = None;
        let mut play_group: Option<usize> = None;
        let mut sort_group: Option<usize> = None;

        ui.set_max_width(ui.max_rect().width());
        ui.horizontal(|ui| {
            // Horizontal layouts report infinite available_width — size from max_rect.
            let btn_reserve = 64.0;
            let edit_w = (ui.max_rect().width() - btn_reserve - ui.spacing().item_spacing.x).max(40.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.sidebar_filter)
                    .hint_text("Filter…")
                    .desired_width(edit_w),
            );
            if ui
                .small_button(icons::labeled(icons::PLUS, "Group"))
                .on_hover_text("Create a camera group")
                .clicked()
            {
                add_group = true;
            }
        });
        ui.add_space(4.0);

        let filter = self.sidebar_filter.to_ascii_lowercase();
        let active_ids = self.view_camera_ids_at(view_idx);
        let selected_ptz = self.sidebar_ptz_cam.clone();
        let dragging_library = matches!(&self.cross_drag, Some(DragPayload::FromLibrary(_)))
            || ui.ctx().dragged_id().is_some();

        let available = ui.available_height();
        let handle_h = 8.0;
        let max_controls = (available - 80.0 - handle_h).max(140.0);
        let controls_h = clamp_sidebar_controls_height(self.sidebar_controls_height)
            .min(max_controls);
        let list_h = (available - controls_h - handle_h).max(80.0);

        egui::ScrollArea::vertical()
            .id_salt(("camera_library", is_aux))
            .max_height(list_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let group_count = self.library_groups.len();
                let multi = group_count > 1;
                for gi in 0..group_count {
                    let group_name = self.library_groups[gi].name.clone();
                    let group_open = self.library_groups[gi].open || !multi;
                    let cam_ids = self.library_groups[gi].cameras.clone();

                    if self.library_renaming == Some(gi) {
                        ui.horizontal(|ui| {
                            let edit_w = (ui.max_rect().width() - 52.0).max(40.0);
                            let edit = ui.add(
                                egui::TextEdit::singleline(&mut self.library_rename_buf)
                                    .desired_width(edit_w)
                                    .hint_text("Group name"),
                            );
                            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                finish_rename = true;
                            }
                            if ui.small_button("OK").clicked() {
                                finish_rename = true;
                            }
                            if ui.small_button("✕").clicked() {
                                cancel_rename = true;
                            }
                        });
                    }

                    let mut body = |ui: &mut egui::Ui| {
                        self.library_group_body(
                            ui,
                            is_aux,
                            gi,
                            &cam_ids,
                            &filter,
                            &active_ids,
                            selected_ptz.as_deref(),
                            dragging_library,
                            &mut lib_drop,
                            &mut select_cam,
                            &mut assign_cam,
                        );
                    };

                    // Always show a header so groups can be double-clicked to play.
                    let header = egui::CollapsingHeader::new(format!(
                        "{}  ({})",
                        group_name,
                        cam_ids.len()
                    ))
                    .id_salt(("lib_group", is_aux, gi))
                    .open(Some(group_open))
                    .show(ui, |ui| body(ui));

                    if header.header_response.double_clicked() {
                        play_group = Some(gi);
                    } else if multi && header.header_response.clicked() {
                        if let Some(g) = self.library_groups.get_mut(gi) {
                            g.open = !g.open;
                            save_groups = true;
                        }
                    }
                    header.header_response.context_menu(|ui| {
                        if ui.button("Play group").clicked() {
                            play_group = Some(gi);
                            ui.close_menu();
                        }
                        if ui.button("Sort A–Z").clicked() {
                            sort_group = Some(gi);
                            ui.close_menu();
                        }
                        if ui.button("Rename").clicked() {
                            start_rename = Some(gi);
                            ui.close_menu();
                        }
                        if multi && ui.button("Delete group").clicked() {
                            delete_group = Some(gi);
                            ui.close_menu();
                        }
                    });
                    header
                        .header_response
                        .clone()
                        .on_hover_text("Double-click to play this group on the grid");

                    let href = &header.header_response;
                    let hovering = href.dnd_hover_payload::<DragPayload>().is_some()
                        || (href.contains_pointer()
                            && matches!(
                                &self.cross_drag,
                                Some(DragPayload::FromLibrary(_))
                            ));
                    if hovering {
                        ui.painter().rect_stroke(
                            href.rect,
                            2.0,
                            egui::Stroke::new(1.5_f32, self.accent),
                            egui::StrokeKind::Inside,
                        );
                    }
                    if let Some(payload) = href.dnd_release_payload::<DragPayload>() {
                        if let DragPayload::FromLibrary(id) = (*payload).clone() {
                            lib_drop = Some((id, gi, cam_ids.len()));
                            self.cross_drag = None;
                        }
                    } else if href.contains_pointer()
                        && ui.input(|i| i.pointer.primary_released())
                    {
                        if let Some(DragPayload::FromLibrary(id)) = self.cross_drag.take() {
                            lib_drop = Some((id, gi, cam_ids.len()));
                        }
                    }
                }
            });

        if add_group {
            self.add_library_group();
        }
        if let Some(gi) = start_rename {
            self.library_renaming = Some(gi);
            self.library_rename_buf = self
                .library_groups
                .get(gi)
                .map(|g| g.name.clone())
                .unwrap_or_default();
        }
        if finish_rename {
            self.finish_library_rename();
        }
        if cancel_rename {
            self.library_renaming = None;
        }
        if let Some(gi) = delete_group {
            self.delete_library_group(gi);
        }
        if let Some((id, g, i)) = lib_drop {
            self.library_move_camera(&id, g, i);
        }
        if let Some(id) = select_cam {
            self.select_camera(&id);
        }
        if let Some(id) = assign_cam {
            self.assign_camera_to_selected_slot(&id, view_idx, is_aux);
        }
        if let Some(gi) = play_group {
            self.play_library_group(gi, view_idx, is_aux);
        }
        if let Some(gi) = sort_group {
            self.sort_library_group(gi);
        }
        if save_groups {
            self.save_ui_prefs();
        }

        // Drag splitter between camera list and PTZ / Presets.
        let full_w = ui.max_rect().width().min(ui.available_width()).max(1.0);
        let (handle_rect, handle) =
            ui.allocate_exact_size(Vec2::new(full_w, handle_h), Sense::drag());
        let mid_y = handle_rect.center().y;
        ui.painter().hline(
            handle_rect.x_range().shrink(8.0),
            mid_y,
            egui::Stroke::new(
                2.0_f32,
                if handle.hovered() || handle.dragged() {
                    self.accent
                } else {
                    Color32::from_rgb(55, 60, 70)
                },
            ),
        );
        if handle.hovered() || handle.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
        if handle.dragged() {
            // Drag up → taller controls; drag down → taller camera list.
            let next = self.sidebar_controls_height - handle.drag_delta().y;
            self.sidebar_controls_height =
                clamp_sidebar_controls_height(next).min(max_controls);
            self.save_ui_prefs();
        }

        let controls_w = full_w;
        ui.allocate_ui_with_layout(
            Vec2::new(controls_w, controls_h),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_min_height(controls_h);
                ui.set_max_height(controls_h);
                egui::ScrollArea::vertical()
                    .id_salt(("sidebar_controls", is_aux))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.sidebar_ptz_panel(ui);
                    });
            },
        );
    }

    fn library_group_body(
        &mut self,
        ui: &mut egui::Ui,
        is_aux: bool,
        gi: usize,
        cam_ids: &[String],
        filter: &str,
        active_ids: &[String],
        selected_ptz: Option<&str>,
        dragging_library: bool,
        lib_drop: &mut Option<(String, usize, usize)>,
        select_cam: &mut Option<String>,
        assign_cam: &mut Option<String>,
    ) {
        let mut visible: Vec<(usize, String, String, bool, bool)> = Vec::new();
        for (ci, id) in cam_ids.iter().enumerate() {
            let Some(cam) = self.camera_by_id(id) else {
                continue;
            };
            if !filter.is_empty()
                && !cam.name.to_ascii_lowercase().contains(filter)
                && !cam.id.to_ascii_lowercase().contains(filter)
            {
                continue;
            }
            visible.push((
                ci,
                cam.id.clone(),
                cam.name.clone(),
                active_ids.contains(&cam.id),
                cam.ptz.is_some(),
            ));
        }

        if visible.is_empty() {
            let (rect, drop_resp) = ui.allocate_exact_size(
                Vec2::new(ui.available_width(), 28.0),
                Sense::hover(),
            );
            ui.painter().text(
                rect.left_center() + Vec2::new(6.0, 0.0),
                egui::Align2::LEFT_CENTER,
                if filter.is_empty() {
                    "Drop cameras here"
                } else {
                    "No matches"
                },
                self.scaled_font(12.0),
                Color32::from_rgb(120, 125, 135),
            );
            let hovering = drop_resp.dnd_hover_payload::<DragPayload>().is_some()
                || (drop_resp.contains_pointer()
                    && matches!(&self.cross_drag, Some(DragPayload::FromLibrary(_))));
            if hovering {
                ui.painter().rect_stroke(
                    rect,
                    2.0,
                    egui::Stroke::new(1.5_f32, self.accent),
                    egui::StrokeKind::Inside,
                );
            }
            if let Some(payload) = drop_resp.dnd_release_payload::<DragPayload>() {
                if let DragPayload::FromLibrary(id) = (*payload).clone() {
                    *lib_drop = Some((id, gi, cam_ids.len()));
                    self.cross_drag = None;
                }
            } else if drop_resp.contains_pointer() && ui.input(|i| i.pointer.primary_released()) {
                if let Some(DragPayload::FromLibrary(id)) = self.cross_drag.take() {
                    *lib_drop = Some((id, gi, cam_ids.len()));
                }
            }
        }

        for (ci, id, name, on_view, has_ptz) in visible {
            let item_id = Id::new(("lib_cam", is_aux, gi)).with(&id);
            let payload = DragPayload::FromLibrary(id.clone());
            let is_ptz_sel = selected_ptz == Some(id.as_str());
            let accent = self.accent;
            let response = ui.dnd_drag_source(item_id, payload.clone(), |ui| {
                let mark = if on_view {
                    icons::PLAY
                } else {
                    icons::SQUARE
                };
                let label = if has_ptz {
                    format!("{mark}  {name}  {}", icons::CROSSHAIR)
                } else {
                    format!("{mark}  {name}")
                };
                let color = if is_ptz_sel {
                    selection_border(has_ptz)
                } else if on_view {
                    super::settings::mix_rgb(accent, Color32::from_rgb(200, 210, 220), 0.35)
                } else {
                    Color32::from_rgb(210, 210, 210)
                };
                ui.add(
                    egui::Label::new(egui::RichText::new(label).color(color))
                        .truncate()
                        .sense(Sense::click_and_drag()),
                )
            });

            let row = &response.response;
            let hovering = row.dnd_hover_payload::<DragPayload>().is_some()
                || (row.contains_pointer()
                    && matches!(
                        &self.cross_drag,
                        Some(DragPayload::FromLibrary(other)) if other != &id
                    ));
            if hovering {
                ui.painter().hline(
                    row.rect.x_range(),
                    row.rect.top(),
                    egui::Stroke::new(2.0_f32, self.accent),
                );
            }
            if let Some(payload) = row.dnd_release_payload::<DragPayload>() {
                if let DragPayload::FromLibrary(drag_id) = (*payload).clone() {
                    if drag_id != id {
                        *lib_drop = Some((drag_id, gi, ci));
                    }
                    self.cross_drag = None;
                }
            } else if row.contains_pointer() && ui.input(|i| i.pointer.primary_released()) {
                if let Some(DragPayload::FromLibrary(drag_id)) = self.cross_drag.clone() {
                    if drag_id != id {
                        *lib_drop = Some((drag_id, gi, ci));
                        self.cross_drag = None;
                    }
                }
            }

            let hover = if has_ptz {
                "Click to select · double-click onto selected slot · drag to reorder"
            } else {
                "Click to select · double-click onto selected slot · drag to reorder"
            };
            let clicked = response.inner.clicked();
            let double = response.inner.double_clicked();
            if response.inner.drag_started() {
                self.cross_drag = Some(payload);
            }
            response.inner.on_hover_text(hover);
            if double {
                *assign_cam = Some(id);
            } else if clicked {
                *select_cam = Some(id);
            }
            ui.add_space(2.0);
        }

        if dragging_library && !cam_ids.is_empty() && filter.is_empty() {
            let (rect, drop_resp) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 10.0), Sense::hover());
            let hovering = drop_resp.dnd_hover_payload::<DragPayload>().is_some()
                || (drop_resp.contains_pointer()
                    && matches!(&self.cross_drag, Some(DragPayload::FromLibrary(_))));
            if hovering {
                ui.painter().hline(
                    rect.x_range(),
                    rect.center().y,
                    egui::Stroke::new(2.0_f32, self.accent),
                );
            }
            if let Some(payload) = drop_resp.dnd_release_payload::<DragPayload>() {
                if let DragPayload::FromLibrary(id) = (*payload).clone() {
                    *lib_drop = Some((id, gi, cam_ids.len()));
                    self.cross_drag = None;
                }
            } else if drop_resp.contains_pointer() && ui.input(|i| i.pointer.primary_released()) {
                if let Some(DragPayload::FromLibrary(id)) = self.cross_drag.take() {
                    *lib_drop = Some((id, gi, cam_ids.len()));
                }
            }
        }
    }

    fn sidebar_ptz_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if ui
                .selectable_label(self.sidebar_controls == SidebarControls::Ptz, "PTZ")
                .clicked()
            {
                self.sidebar_controls = SidebarControls::Ptz;
            }
            if ui
                .selectable_label(self.sidebar_controls == SidebarControls::Presets, "Presets")
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
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(name)
                            .small()
                            .color(Color32::from_rgb(180, 190, 200)),
                    )
                    .truncate(),
                )
                .on_hover_text(name.clone());
            }
            (Some(name), None) => {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(name)
                            .small()
                            .color(Color32::from_rgb(180, 190, 200)),
                    )
                    .truncate(),
                )
                .on_hover_text(name.clone());
                ui.label(
                    egui::RichText::new("No PTZ on this camera")
                        .small()
                        .color(Color32::from_rgb(140, 140, 140)),
                );
                return;
            }
            _ => {
                ui.label(
                    egui::RichText::new("Click a camera to control PTZ")
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
        // Display → UI scale must not affect the PTZ pad; keep default button chrome here.
        {
            let style = ui.style_mut();
            style.text_styles.insert(
                egui::TextStyle::Button,
                egui::FontId::new(16.0, egui::FontFamily::Proportional),
            );
            style.spacing.interact_size = egui::vec2(18.0, 18.0);
            style.spacing.button_padding = egui::vec2(4.0, 2.0);
        }

        let mut pan = 0i32;
        let mut tilt = 0i32;
        let mut zoom = 0i32;
        let mut focus = 0i32;
        let mut home = false;

        let gap = 4.0;
        // Prefer the panel's max rect — available_width can be infinite in some layouts.
        let width = ui.max_rect().width().min(ui.available_width()).clamp(120.0, 360.0);
        let cell = ((width - gap * 2.0) / 3.0).floor().clamp(34.0, 48.0);
        let font = (cell * 0.42).clamp(14.0, 18.0);
        let pad_w = cell * 3.0 + gap * 2.0;

        let hold = |ui: &mut egui::Ui, label: &str| -> bool {
            let response = ui.add_sized(
                [cell, cell],
                egui::Button::new(egui::RichText::new(label).size(font))
                    .sense(Sense::click_and_drag()),
            );
            let down = response.is_pointer_button_down_on()
                || (response.contains_pointer() && ui.input(|i| i.pointer.primary_down()));
            if down {
                ui.ctx().request_repaint();
            }
            down
        };

        ui.horizontal(|ui| {
            // Never use available_width() here — it's infinite in LTR and add_space(∞)
            // expands the whole side panel to its max.
            let inset = ((width - pad_w) * 0.5).max(0.0);
            if inset > 0.0 && inset.is_finite() {
                ui.add_space(inset);
            }
            egui::Grid::new("ptz_pad")
                .min_col_width(cell)
                .max_col_width(cell)
                .spacing([gap, gap])
                .show(ui, |ui| {
                    if hold(ui, icons::ARROW_UP_LEFT) {
                        pan = -PTZ_MOVE_SPEED;
                        tilt = PTZ_MOVE_SPEED;
                    }
                    if hold(ui, icons::ARROW_UP) {
                        tilt = PTZ_MOVE_SPEED;
                    }
                    if hold(ui, icons::ARROW_UP_RIGHT) {
                        pan = PTZ_MOVE_SPEED;
                        tilt = PTZ_MOVE_SPEED;
                    }
                    ui.end_row();

                    if hold(ui, icons::ARROW_LEFT) {
                        pan = -PTZ_MOVE_SPEED;
                    }
                    if ui
                        .add_sized(
                            [cell, cell],
                            egui::Button::new(egui::RichText::new(icons::HOUSE).size(font)),
                        )
                        .on_hover_text("Home")
                        .clicked()
                    {
                        home = true;
                    }
                    if hold(ui, icons::ARROW_RIGHT) {
                        pan = PTZ_MOVE_SPEED;
                    }
                    ui.end_row();

                    if hold(ui, icons::ARROW_DOWN_LEFT) {
                        pan = -PTZ_MOVE_SPEED;
                        tilt = -PTZ_MOVE_SPEED;
                    }
                    if hold(ui, icons::ARROW_DOWN) {
                        tilt = -PTZ_MOVE_SPEED;
                    }
                    if hold(ui, icons::ARROW_DOWN_RIGHT) {
                        pan = PTZ_MOVE_SPEED;
                        tilt = -PTZ_MOVE_SPEED;
                    }
                    ui.end_row();

                    if hold(ui, icons::MAGNIFYING_GLASS_MINUS) {
                        zoom = -PTZ_ZOOM_SPEED;
                    }
                    let (zoom_rect, _) = ui.allocate_exact_size(Vec2::splat(cell), Sense::hover());
                    ui.painter().text(
                        zoom_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "Zoom",
                        egui::FontId::proportional(11.0),
                        Color32::from_rgb(150, 150, 150),
                    );
                    if hold(ui, icons::MAGNIFYING_GLASS_PLUS) {
                        zoom = PTZ_ZOOM_SPEED;
                    }
                    ui.end_row();

                    if hold(ui, "F−") {
                        focus = -PTZ_ZOOM_SPEED;
                    }
                    let (focus_rect, _) = ui.allocate_exact_size(Vec2::splat(cell), Sense::hover());
                    ui.painter().text(
                        focus_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "Focus",
                        egui::FontId::proportional(11.0),
                        Color32::from_rgb(150, 150, 150),
                    );
                    if hold(ui, "F+") {
                        focus = PTZ_ZOOM_SPEED;
                    }
                    ui.end_row();
                });
        });

        if home {
            if let Some(target) = self.active_ptz_target() {
                self.ptz_stop();
                self.ptz.home(target);
            }
        }

        let held = PtzVector {
            pan,
            tilt,
            zoom,
            focus,
        };
        let pad_id = ui.id().with("ptz_pad_hold");
        let was_holding = ui
            .ctx()
            .data(|d| d.get_temp::<bool>(pad_id).unwrap_or(false));
        if !held.is_stop() {
            self.apply_ptz_vector(held);
            ui.ctx().data_mut(|d| d.insert_temp(pad_id, true));
            ui.ctx().request_repaint();
        } else if was_holding {
            self.apply_ptz_vector(PtzVector::STOP);
            ui.ctx().data_mut(|d| d.insert_temp(pad_id, false));
        }
    }

    #[allow(dead_code)] // Hidden: parkaction toggle is unreliable on our NVRs.
    fn sidebar_park_action(&mut self, ui: &mut egui::Ui) {
        let Some(target) = self.active_ptz_target() else {
            return;
        };

        ui.horizontal(|ui| {
            if ui
                .small_button(icons::ARROW_CLOCKWISE)
                .on_hover_text("Refresh park action")
                .clicked()
            {
                self.ptz.refresh_park_action(target.clone());
            }
            match self.ptz.park_status(&target) {
                ParkActionStatus::Idle => {
                    self.ptz.fetch_park_action(target.clone());
                    ui.label(
                        egui::RichText::new("Park…")
                            .small()
                            .color(Color32::from_rgb(160, 160, 160)),
                    );
                }
                ParkActionStatus::Loading => {
                    ui.ctx().request_repaint();
                    ui.label(
                        egui::RichText::new("Park…")
                            .small()
                            .color(Color32::from_rgb(160, 160, 160)),
                    );
                }
                ParkActionStatus::Error(_) | ParkActionStatus::Ready(_) => {}
            }
        });

        // Full-width controls must stay in the vertical layout — inside a
        // horizontal row `available_width()` is infinite and blows the side panel
        // out to its max.
        match self.ptz.park_status(&target) {
            ParkActionStatus::Idle | ParkActionStatus::Loading => {}
            ParkActionStatus::Error(err) => {
                if ui
                    .add_sized(
                        [ui.available_width(), 0.0],
                        egui::Button::new(
                            egui::RichText::new("Park ?")
                                .color(Color32::from_rgb(255, 160, 120)),
                        ),
                    )
                    .on_hover_text(err)
                    .clicked()
                {
                    self.ptz.refresh_park_action(target.clone());
                }
            }
            ParkActionStatus::Ready(park) => {
                let (label, color) = if park.enabled {
                    ("Park On", Color32::from_rgb(140, 210, 150))
                } else {
                    ("Park Off", Color32::from_rgb(180, 180, 180))
                };
                let hint = park_action_hint(&park);
                if ui
                    .add_sized(
                        [ui.available_width(), 0.0],
                        egui::Button::new(egui::RichText::new(label).color(color)),
                    )
                    .on_hover_text(format!(
                        "{hint}\nClick to {}",
                        if park.enabled { "disable" } else { "enable" }
                    ))
                    .clicked()
                {
                    self.ptz.set_park_enabled(target.clone(), !park.enabled);
                }
                ui.label(
                    egui::RichText::new(hint)
                        .small()
                        .color(Color32::from_rgb(140, 150, 160)),
                );
            }
        }
    }

    fn sidebar_presets_list(&mut self, ui: &mut egui::Ui) {
        let Some(target) = self.active_ptz_target() else {
            return;
        };

        ui.horizontal(|ui| {
            if ui
                .small_button(icons::ARROW_CLOCKWISE)
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
                                egui::RichText::new(label).color(Color32::from_rgb(220, 220, 220)),
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

    pub(super) fn update_dialog(&mut self, ctx: &egui::Context) {
        if matches!(self.update_banner, UpdateBanner::Hidden) {
            return;
        }

        let title = match &self.update_banner {
            UpdateBanner::Offer(offer) => format!("Update to {}", offer.tag),
            UpdateBanner::Downloading { version } => format!("Downloading {version}"),
            UpdateBanner::Ready { version } => format!("Update {version} installed"),
            UpdateBanner::Failed { .. } => "Update failed".into(),
            UpdateBanner::Hidden => return,
        };

        let mut open = true;
        egui::Window::new(title)
            .id(Id::new("update_dialog"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(420.0)
            .show(ctx, |ui| match self.update_banner.clone() {
                UpdateBanner::Hidden => {}
                UpdateBanner::Offer(offer) => {
                    ui.label(
                        egui::RichText::new(format!(
                            "A newer AppImage ({}) is available.",
                            offer.tag
                        ))
                        .color(Color32::from_rgb(220, 200, 120)),
                    );
                    if !offer.changelog.is_empty() {
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new("What's new").strong());
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .id_salt("update_changelog")
                            .max_height(220.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(&offer.changelog)
                                        .color(Color32::from_rgb(190, 195, 205)),
                                );
                            });
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Update").clicked() {
                            self.start_update_download();
                        }
                        if ui.button("Later").clicked() {
                            self.update_later = true;
                            self.update_banner = UpdateBanner::Hidden;
                        }
                        if ui.button("Skip this version").clicked() {
                            self.skip_this_update();
                        }
                    });
                }
                UpdateBanner::Downloading { version } => {
                    let bytes = self
                        .update_progress
                        .load(std::sync::atomic::Ordering::Relaxed);
                    ui.label(format!(
                        "Downloading {version}… {}",
                        format_download_bytes(bytes)
                    ));
                }
                UpdateBanner::Ready { version } => {
                    ui.colored_label(
                        Color32::from_rgb(140, 220, 160),
                        format!("Update {version} is ready."),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Restart now").clicked() {
                            self.relaunch_after_update();
                        }
                        if ui.button("Quit").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
                UpdateBanner::Failed { message } => {
                    ui.colored_label(
                        Color32::from_rgb(255, 160, 120),
                        format!("Update failed: {message}"),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Retry").clicked() {
                            self.start_update_download();
                        }
                        if ui.button("Dismiss").clicked() {
                            self.update_later = true;
                            self.update_banner = UpdateBanner::Hidden;
                        }
                    });
                }
            });

        if !open {
            match &self.update_banner {
                UpdateBanner::Downloading { .. } => {
                    // Keep dialog open while download runs.
                }
                UpdateBanner::Offer(_) | UpdateBanner::Failed { .. } => {
                    self.update_later = true;
                    self.update_banner = UpdateBanner::Hidden;
                }
                UpdateBanner::Ready { .. } => {
                    self.update_banner = UpdateBanner::Hidden;
                }
                UpdateBanner::Hidden => {}
            }
        }
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
        // Always sample/write hitch stats (overlay only controls the on-screen panel).
        if self.last_debug_sample.elapsed() < std::time::Duration::from_millis(2000) {
            return;
        }
        self.ui_perf.sample_rates();
        self.debug_rows = self.streams.debug_snapshot();
        self.last_debug_sample = Instant::now();

        let gap_max = self
            .debug_rows
            .iter()
            .map(|r| r.emit_gap_max_ms)
            .max()
            .unwrap_or(0);
        let sum_fps: f32 = self.debug_rows.iter().map(|r| r.fps).sum();
        let sum_stale: f32 = self.debug_rows.iter().map(|r| r.stale_fps).sum();
        let sum_drop_rate: f32 = self.debug_rows.iter().map(|r| r.drop_rate_fps).sum();
        let sum_drop_delta: f32 = self.debug_rows.iter().map(|r| r.drop_delta_fps).sum();
        let sum_in: f32 = self.debug_rows.iter().map(|r| r.samples_in_fps).sum();

        if self.debug_overlay {
            info!(
                ui_fps = format!("{:.1}", self.ui_perf.ui_fps),
                ui_dt_max_ms = self.ui_perf.reported_frame_dt_max_ms,
                tex_fps = format!("{:.1}", self.ui_perf.upload_fps),
                tex_skip_fps = format!("{:.1}", self.ui_perf.skip_fps),
                tex_upload_us = format!("{:.0}", self.ui_perf.avg_upload_us),
                tex_upload_us_max = self.ui_perf.reported_upload_us_max,
                tex_pass_us_max = self.ui_perf.reported_tex_pass_us_max,
                decode_fps = format!("{sum_fps:.1}"),
                in_fps = format!("{sum_in:.1}"),
                stale_fps = format!("{sum_stale:.1}"),
                drop_rate_fps = format!("{sum_drop_rate:.1}"),
                drop_delta_fps = format!("{sum_drop_delta:.1}"),
                emit_gap_max_ms = gap_max,
                layout = self.active_layout().as_str(),
                hd = self.hd,
                view = %self.views.active_view().name,
                "stutter sample"
            );
            log_stream_debug_rows(&self.debug_rows);
        }
        if self.stutter_log_file {
            self.append_stutter_stats_file();
        }
        self.last_debug_log = Instant::now();
    }

    fn append_stutter_stats_file(&self) {
        use std::io::Write;
        use std::time::{SystemTime, UNIX_EPOCH};
        let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.stutter_log_path)
        else {
            return;
        };
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(
            f,
            "=== unix={secs} view={} layout={} hd={} ui_fps={:.1} ui_dt_max_ms={} tex_fps={:.1} tex_skip={:.1} upload_us_avg={:.0} upload_us_max={} tex_pass_us_max={} ===",
            self.views.active_view().name,
            self.active_layout().as_str(),
            self.hd,
            self.ui_perf.ui_fps,
            self.ui_perf.reported_frame_dt_max_ms,
            self.ui_perf.upload_fps,
            self.ui_perf.skip_fps,
            self.ui_perf.avg_upload_us,
            self.ui_perf.reported_upload_us_max,
            self.ui_perf.reported_tex_pass_us_max,
        );
        for row in &self.debug_rows {
            let _ = writeln!(
                f,
                "  {id} run={run} {w}x{h} tier={tw}@{tf} out={out:.1} in={inn:.1} stale={st:.1} drop_rate={dr:.1} drop_delta={dd:.1} drop_key={dk:.1} gap_ms={gap} copy_us={cu:.0} dec={dec}",
                id = row.id,
                run = row.running,
                w = row.width,
                h = row.height,
                tw = row.max_width,
                tf = row.max_fps,
                out = row.fps,
                inn = row.samples_in_fps,
                st = row.stale_fps,
                dr = row.drop_rate_fps,
                dd = row.drop_delta_fps,
                dk = row.drop_key_fps,
                gap = row.emit_gap_max_ms,
                cu = row.avg_copy_us,
                dec = if row.decoder.is_empty() {
                    "?"
                } else {
                    row.decoder.as_str()
                },
            );
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
                        "UI {:.1} fps (dt_max {}ms) │ tex {:.1}/s skip {:.1}/s (avg {:.0}µs max {}µs pass_max {}µs) │ D toggles",
                        self.ui_perf.ui_fps,
                        self.ui_perf.reported_frame_dt_max_ms,
                        self.ui_perf.upload_fps,
                        self.ui_perf.skip_fps,
                        self.ui_perf.avg_upload_us,
                        self.ui_perf.reported_upload_us_max,
                        self.ui_perf.reported_tex_pass_us_max,
                    ))
                    .monospace()
                    .size(12.0),
                );
                ui.label(
                    egui::RichText::new(format!(
                        "logging → {} (every 2s). drop_rate=wall-clock cap, drop_delta=IDR-prefer, gap_ms=worst emit spacing",
                        self.stutter_log_path.display()
                    ))
                    .size(11.0)
                    .color(Color32::from_rgb(160, 160, 160)),
                );
                ui.separator();

                let sum_fps: f32 = self.debug_rows.iter().map(|r| r.fps).sum();
                let sum_stale: f32 = self.debug_rows.iter().map(|r| r.stale_fps).sum();
                let sum_mbps: f32 = self.debug_rows.iter().map(|r| r.rgba_mbps).sum();
                let sum_drop_rate: f32 = self.debug_rows.iter().map(|r| r.drop_rate_fps).sum();
                let sum_drop_delta: f32 = self.debug_rows.iter().map(|r| r.drop_delta_fps).sum();
                let gap_max = self
                    .debug_rows
                    .iter()
                    .map(|r| r.emit_gap_max_ms)
                    .max()
                    .unwrap_or(0);
                ui.label(
                    egui::RichText::new(format!(
                        "streams {} │ out {:.1} fps │ stale {:.1}/s │ drop_rate {:.1} drop_delta {:.1} │ gap_max {}ms │ RGBA {:.1} MiB/s",
                        self.debug_rows.len(),
                        sum_fps,
                        sum_stale,
                        sum_drop_rate,
                        sum_drop_delta,
                        gap_max,
                        sum_mbps,
                    ))
                    .monospace()
                    .strong(),
                );
                ui.add_space(6.0);

                egui::Grid::new("perf_grid")
                    .striped(true)
                    .min_col_width(48.0)
                    .show(ui, |ui| {
                        ui.label("camera");
                        ui.label("out");
                        ui.label("in");
                        ui.label("stale");
                        ui.label("d_rate");
                        ui.label("d_delta");
                        ui.label("gap");
                        ui.label("copyµs");
                        ui.label("dec");
                        ui.end_row();

                        for row in &self.debug_rows {
                            ui.label(&row.id);
                            ui.label(format!("{:.1}", row.fps));
                            ui.label(format!("{:.1}", row.samples_in_fps));
                            ui.label(format!("{:.1}", row.stale_fps));
                            ui.label(format!("{:.1}", row.drop_rate_fps));
                            ui.label(format!("{:.1}", row.drop_delta_fps));
                            ui.label(format!("{}", row.emit_gap_max_ms));
                            ui.label(format!("{:.0}", row.avg_copy_us));
                            ui.label(if row.decoder.is_empty() {
                                "?"
                            } else {
                                row.decoder.as_str()
                            });
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
        selected: bool,
    ) {
        ui.painter()
            .rect_filled(cell, 0.0, Color32::from_rgb(12, 14, 18));

        let live = self.streams.has_live_frame(&cam.id);
        let reconnecting = self.streams.is_reconnecting(&cam.id);
        let err = self.streams.error(&cam.id);
        let waiting = self.streams.is_running(&cam.id) && !live;
        let has_tex = self.textures.contains_key(&cam.id);
        let offline = !has_tex || !live || err.is_some() || reconnecting;

        if let Some(tex) = self.textures.get(&cam.id) {
            let size = tex.handle.size_vec2();
            let aspect = if size.y > 0.0 {
                size.x / size.y
            } else {
                16.0 / 9.0
            };
            let draw = fitted_rect(cell, aspect, self.fit);
            let painter = ui.painter().with_clip_rect(cell);
            let tint = if live && err.is_none() && !reconnecting {
                Color32::WHITE
            } else {
                // Hold last good frame dimmed while reconnecting / waiting for keyframe.
                Color32::from_rgb(160, 160, 160)
            };
            painter.image(
                tex.handle.id(),
                draw,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                tint,
            );
        }

        let status = if self.paused {
            Some("Paused")
        } else if reconnecting || err.is_some() {
            // Keep the banner short — never paint the raw GStreamer error string.
            Some("Reconnecting…")
        } else if waiting || (has_tex && !live) {
            Some("Connecting…")
        } else if !has_tex {
            Some("Offline")
        } else {
            None
        };
        if let Some(msg) = status {
            ui.painter().text(
                cell.center(),
                egui::Align2::CENTER_CENTER,
                msg,
                self.scaled_font(11.0),
                Color32::from_rgb(190, 195, 205),
            );
        }

        // Always show the name on offline / reconnecting cells; otherwise on hover/select.
        if hovered || selected || offline || self.paused {
            let bar_h = 22.0;
            let bar = Rect::from_min_max(egui::pos2(cell.min.x, cell.max.y - bar_h), cell.max);
            ui.painter()
                .rect_filled(bar, 0.0, Color32::from_rgba_unmultiplied(0, 0, 0, 170));
            let label = if selected && cam.ptz.is_some() {
                format!("{}  {}", cam.name, icons::CROSSHAIR)
            } else {
                cam.name.clone()
            };
            ui.painter().text(
                bar.left_center() + Vec2::new(8.0, 0.0),
                egui::Align2::LEFT_CENTER,
                label,
                self.scaled_font(12.0),
                if selected {
                    selection_border(cam.ptz.is_some())
                } else {
                    Color32::WHITE
                },
            );
        }
    }

    pub(super) fn paint_monitor(
        &mut self,
        ui: &mut egui::Ui,
        full: Rect,
        view_idx: usize,
        fullscreen_slot: Option<usize>,
        is_aux: bool,
    ) {
        if let Some(slot_idx) = fullscreen_slot {
            let cam_id = self
                .views
                .view(view_idx)
                .slots
                .get(slot_idx)
                .cloned()
                .flatten();
            if let Some(id) = cam_id {
                if let Some(cam) = self.camera_by_id(&id).cloned() {
                    let screen_focused = ui.ctx().input(|i| i.focused);
                    let selected = self.show_ptz_selection_here(is_aux, screen_focused)
                        && self.sidebar_ptz_cam.as_deref() == Some(cam.id.as_str());
                    let response =
                        ui.interact(full, Id::new(("fs", view_idx, is_aux)), Sense::click());
                    self.paint_cell_contents(ui, &cam, full, response.hovered(), selected);
                    if response.clicked() {
                        self.select_camera(&cam.id);
                    }
                    if response.double_clicked() || response.secondary_clicked() {
                        if is_aux {
                            self.exit_aux_fullscreen();
                        } else {
                            self.exit_fullscreen();
                        }
                    }
                }
            } else if is_aux {
                self.exit_aux_fullscreen();
            } else {
                self.exit_fullscreen();
            }
            return;
        }
        self.draw_grid(ui, full, view_idx, is_aux);
    }

    pub(super) fn show_aux_window(&mut self, ctx: &egui::Context) {
        if !self.aux_open {
            self.aux_focused = false;
            self.last_ptz_screen_aux = false;
            self.aux_pointer_down = false;
            return;
        }
        self.clamp_aux_view();
        let mut close = false;
        let title = crate::config::app_screen_title(&self.app_name, Some(2));
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("citadel_aux_screen"),
            egui::ViewportBuilder::default()
                .with_app_id(crate::config::LINUX_CONFIG_DIR)
                .with_title(title.clone())
                .with_inner_size([1280.0, 720.0])
                .with_fullscreen(self.aux_window_fullscreen),
            |ctx, _class| {
                self.apply_accent_visuals(ctx);
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
                self.aux_focused = ctx.input(|i| i.focused);
                if self.aux_focused {
                    self.last_ptz_screen_aux = true;
                }
                self.aux_pointer_down = ctx.input(|i| i.pointer.primary_down());
                if ctx.input(|i| i.viewport().close_requested()) {
                    close = true;
                }
                if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                    if self.pending_gate_confirm {
                        self.pending_gate_confirm = false;
                    } else if self.aux_fullscreen_slot.is_some() {
                        self.exit_aux_fullscreen();
                    } else if self.aux_window_fullscreen {
                        self.aux_window_fullscreen = false;
                    }
                }

                let aux_view = self.aux_view;
                if !self.aux_window_fullscreen {
                    self.aux_fs_toolbar_reveal = false;
                    egui::TopBottomPanel::top("aux_toolbar")
                        .frame(
                            egui::Frame::NONE
                                .fill(PANEL_BG)
                                .inner_margin(egui::Margin::symmetric(8, 4)),
                        )
                        .show(ctx, |ui| {
                            self.toolbar(ui, aux_view, true);
                        });

                    if self.sidebar_open {
                        self.show_resizable_camera_sidebar(ctx, aux_view, true, "aux_cameras_side");
                    }
                } else if Self::update_fs_toolbar_reveal(&mut self.aux_fs_toolbar_reveal, ctx) {
                    egui::TopBottomPanel::top("aux_toolbar_fs")
                        .frame(
                            egui::Frame::NONE
                                .fill(PANEL_BG)
                                .inner_margin(egui::Margin::symmetric(8, 4)),
                        )
                        .show(ctx, |ui| {
                            self.toolbar(ui, aux_view, true);
                        });
                    ctx.request_repaint();
                } else {
                    ctx.request_repaint();
                }

                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::NONE
                            .fill(CANVAS_BG)
                            .inner_margin(egui::Margin::ZERO),
                    )
                    .show(ctx, |ui| {
                        let full = ui.max_rect();
                        if self.paused {
                            ui.centered_and_justified(|ui| {
                                ui.label(
                                    egui::RichText::new("Streams paused while in background")
                                        .size(18.0)
                                        .color(Color32::from_rgb(200, 200, 200)),
                                );
                            });
                            return;
                        }
                        let aux_fs = self.aux_fullscreen_slot;
                        self.paint_monitor(ui, full, aux_view, aux_fs, true);
                    });

                self.draw_clear_slot_dialog(ctx);
                self.draw_gate_confirm_dialog(ctx);
                ctx.request_repaint_after(std::time::Duration::from_millis(16));
            },
        );
        if close {
            self.aux_open = false;
            self.aux_focused = false;
            self.last_ptz_screen_aux = false;
        }
    }

    pub(super) fn draw_grid(
        &mut self,
        ui: &mut egui::Ui,
        full: Rect,
        view_idx: usize,
        is_aux: bool,
    ) {
        let layout = self.views.view(view_idx).layout;
        let cols = layout.cols();
        let rows = layout.rows();
        let gap = if (!is_aux && self.window_fullscreen) || (is_aux && self.aux_window_fullscreen) {
            0.0
        } else {
            2.0
        };
        let cell_w = (full.width() - gap * (cols as f32 + 1.0)) / cols as f32;
        let cell_h = (full.height() - gap * (rows as f32 + 1.0)) / rows as f32;

        let slots = self.views.view(view_idx).slots.clone();
        let screen_focused = ui.ctx().input(|i| i.focused);
        let selected_id = self.sidebar_ptz_cam.clone();
        if !is_aux {
            self.ensure_pad_focus();
        } else {
            self.ensure_aux_pad_focus();
        }
        let pad_focus = if is_aux {
            self.aux_pad_focus_slot
        } else {
            self.pad_focus_slot
        };
        let mut drop_action: Option<(usize, DragPayload)> = None;
        let mut fullscreen: Option<usize> = None;
        let mut clear: Option<usize> = None;
        let mut exit_os_fullscreen = false;
        let mut select: Option<String> = None;
        let mut clear_selection = false;
        let mut focus_slot: Option<usize> = None;

        for (i, slot) in slots.iter().enumerate() {
            let col = i % cols;
            let row = i / cols;
            let x = full.min.x + gap + col as f32 * (cell_w + gap);
            let y = full.min.y + gap + row as f32 * (cell_h + gap);
            let cell = Rect::from_min_size(egui::pos2(x, y), Vec2::new(cell_w, cell_h));
            let id = Id::new(("grid_slot", view_idx, i));

            let sense = Sense::click_and_drag();
            let response = ui.interact(cell, id, sense);

            if response.drag_started() {
                if slot.is_some() {
                    self.cross_drag = Some(DragPayload::FromSlot {
                        view: view_idx,
                        slot: i,
                    });
                }
            }

            if let Some(cam_id) = slot {
                let selected = self.show_ptz_selection_here(is_aux, screen_focused)
                    && selected_id.as_deref() == Some(cam_id.as_str());
                let has_ptz = self.camera_by_id(cam_id).is_some_and(|c| c.ptz.is_some());
                if let Some(cam) = self.camera_by_id(cam_id) {
                    self.paint_cell_contents(ui, cam, cell, response.hovered(), selected);
                } else {
                    ui.painter()
                        .rect_filled(cell, 0.0, Color32::from_rgb(12, 14, 18));
                    ui.painter().text(
                        cell.center(),
                        egui::Align2::CENTER_CENTER,
                        "Missing camera",
                        self.scaled_font(13.0),
                        Color32::from_rgb(180, 100, 100),
                    );
                }
                if selected {
                    ui.painter().rect_stroke(
                        cell.shrink(1.0),
                        0.0,
                        egui::Stroke::new(self.outline_width, selection_border(has_ptz)),
                        egui::StrokeKind::Inside,
                    );
                }
                if screen_focused && pad_focus == Some(i) && !selected {
                    ui.painter().rect_stroke(
                        cell.shrink(1.0),
                        0.0,
                        egui::Stroke::new(self.outline_width.max(2.0_f32), self.accent),
                        egui::StrokeKind::Inside,
                    );
                }
                response.dnd_set_drag_payload(DragPayload::FromSlot {
                    view: view_idx,
                    slot: i,
                });
            } else {
                ui.painter()
                    .rect_filled(cell, 0.0, Color32::from_rgb(12, 14, 18));
                ui.painter().text(
                    cell.center(),
                    egui::Align2::CENTER_CENTER,
                    "Drop camera here",
                    self.scaled_font(13.0),
                    Color32::from_rgb(90, 95, 105),
                );
                if screen_focused && pad_focus == Some(i) {
                    ui.painter().rect_stroke(
                        cell.shrink(1.0),
                        0.0,
                        egui::Stroke::new(self.outline_width.max(2.0_f32), self.accent),
                        egui::StrokeKind::Inside,
                    );
                }
            }

            let hovering_drop = response.dnd_hover_payload::<DragPayload>().is_some()
                || (response.contains_pointer() && self.cross_drag.is_some());
            if hovering_drop {
                ui.painter().rect_stroke(
                    cell.shrink(1.0),
                    0.0,
                    egui::Stroke::new(self.outline_width.max(2.0_f32), self.accent),
                    egui::StrokeKind::Inside,
                );
            }

            if let Some(payload) = response.dnd_release_payload::<DragPayload>() {
                drop_action = Some((i, (*payload).clone()));
                self.cross_drag = None;
            } else if response.contains_pointer() && ui.input(|inp| inp.pointer.primary_released())
            {
                if let Some(payload) = self.cross_drag.take() {
                    drop_action = Some((i, payload));
                }
            }
            if response.clicked() {
                // Single-click selects the slot (destination for library double-click).
                focus_slot = Some(i);
                if let Some(cam_id) = slot {
                    select = Some(cam_id.clone());
                } else {
                    clear_selection = true;
                }
            }
            if response.double_clicked() && slot.is_some() {
                fullscreen = Some(i);
            }
            if response.secondary_clicked() {
                let os_fs = if is_aux {
                    self.aux_window_fullscreen
                } else {
                    self.window_fullscreen
                };
                if os_fs {
                    exit_os_fullscreen = true;
                } else if slot.is_some() {
                    clear = Some(i);
                }
            }
        }

        if let Some((idx, payload)) = drop_action {
            self.apply_drop(view_idx, idx, payload);
        }
        if clear_selection {
            self.clear_camera_selection();
        }
        if let Some(id) = select {
            self.select_camera(&id);
        }
        if let Some(idx) = focus_slot {
            if is_aux {
                self.aux_pad_focus_slot = Some(idx);
            } else {
                self.pad_focus_slot = Some(idx);
            }
        }
        if let Some(idx) = fullscreen {
            self.enter_fullscreen_at(view_idx, idx, is_aux);
        }
        if exit_os_fullscreen {
            if is_aux {
                self.aux_window_fullscreen = false;
            } else {
                self.set_window_fullscreen(ui.ctx(), false);
            }
        }
        if let Some(idx) = clear {
            self.pending_clear_slot = Some((view_idx, idx));
        }
    }

    pub(super) fn draw_clear_slot_dialog(&mut self, ctx: &egui::Context) {
        let Some((view_idx, idx)) = self.pending_clear_slot else {
            return;
        };
        let name = self
            .views
            .view(view_idx)
            .slots
            .get(idx)
            .and_then(|s| s.as_deref())
            .and_then(|id| self.camera_by_id(id).map(|c| c.name.clone()))
            .unwrap_or_else(|| "this camera".into());

        let mut confirmed = false;
        let mut cancelled = false;
        let mut open = true;
        egui::Window::new("Remove camera")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .order(egui::Order::Foreground)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!("Remove {name} from this view?"));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Remove").clicked() {
                        confirmed = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancelled = true;
                    }
                });
            });

        if confirmed {
            self.pending_clear_slot = None;
            self.clear_slot(view_idx, idx);
        } else if cancelled || !open {
            self.pending_clear_slot = None;
        }
    }

    pub(super) fn draw_gate_confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.pending_gate_confirm {
            return;
        }

        let mut confirmed = false;
        let mut cancelled = false;
        let mut open = true;
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            confirmed = true;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancelled = true;
        }

        egui::Window::new("Open Gate?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .order(egui::Order::Foreground)
            .default_width(420.0)
            .min_width(380.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new(icons::QUESTION)
                            .size(72.0)
                            .color(self.accent),
                    );
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new("Open the gate?")
                            .size(28.0)
                            .strong(),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(
                            "Enter or ✕ to open  ·  Esc / Cancel to abort",
                        )
                        .size(14.0)
                        .weak(),
                    );
                    ui.add_space(18.0);
                    ui.horizontal(|ui| {
                        let btn_h = 40.0;
                        let btn_w = 140.0;
                        let gap = 12.0;
                        let total = btn_w * 2.0 + gap;
                        let inset = ((ui.available_width() - total) * 0.5).max(0.0);
                        if inset > 0.0 && inset.is_finite() {
                            ui.add_space(inset);
                        }
                        if ui
                            .add_sized(
                                [btn_w, btn_h],
                                egui::Button::new(
                                    egui::RichText::new("Open Gate").size(18.0).strong(),
                                ),
                            )
                            .clicked()
                        {
                            confirmed = true;
                        }
                        ui.add_space(gap);
                        if ui
                            .add_sized(
                                [btn_w, btn_h],
                                egui::Button::new(egui::RichText::new("Cancel").size(18.0)),
                            )
                            .clicked()
                        {
                            cancelled = true;
                        }
                    });
                    ui.add_space(12.0);
                });
            });

        if confirmed {
            self.pending_gate_confirm = false;
            self.open_gates();
        } else if cancelled || !open {
            self.pending_gate_confirm = false;
        }
    }

    pub(super) fn draw_anpr_alert(&mut self, ctx: &egui::Context) {
        let Some(alert) = &self.anpr_alert else {
            return;
        };

        let title = format!("ANPR: {}", alert.person);
        let plate = alert.plate.clone();
        let person = alert.person.clone();
        let elapsed = alert.at.elapsed().as_secs();
        let texture = alert.texture.clone();
        let mut silent = self.anpr.silent();
        let mut dismiss = false;
        let mut open = true;

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            dismiss = true;
        }

        egui::Window::new(title)
            .collapsible(false)
            .resizable(true)
            .default_width(520.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .order(egui::Order::Foreground)
            .open(&mut open)
            .show(ctx, |ui| {
                if let Some(tex) = &texture {
                    let max_w = ui.available_width().min(640.0);
                    let size = tex.size_vec2();
                    let scale = (max_w / size.x).min(360.0 / size.y).min(1.0);
                    ui.image((tex.id(), size * scale));
                }
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(format!("{person}  ·  {plate}"))
                        .size(18.0)
                        .strong(),
                );
                ui.label(
                    egui::RichText::new(format!("{elapsed}s ago"))
                        .small()
                        .weak(),
                );
                ui.add_space(6.0);
                if ui
                    .checkbox(&mut silent, "Mute alert audio")
                    .changed()
                {
                    self.anpr.set_silent(silent);
                    self.anpr_draft.silent = silent;
                }
                ui.add_space(4.0);
                if ui.button("Dismiss").clicked() {
                    dismiss = true;
                }
            });

        if dismiss || !open {
            self.anpr_alert = None;
        }
    }
}

fn selection_border(has_ptz: bool) -> Color32 {
    if has_ptz {
        Color32::from_rgb(70, 200, 110)
    } else {
        Color32::from_rgb(220, 70, 70)
    }
}

#[allow(dead_code)] // Used only by hidden park action UI.
fn park_action_hint(park: &ParkAction) -> String {
    let action = match (park.action_type.as_deref(), park.action_num) {
        (Some(kind), Some(n)) if n > 0 => format!("{kind} {n}"),
        (Some(kind), _) => kind.to_string(),
        _ => "idle return".into(),
    };
    match park.park_time_sec {
        Some(sec) => format!("{action} · {sec}s"),
        None => action,
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

fn format_download_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    }
}
