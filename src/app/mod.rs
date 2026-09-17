mod input;
mod settings;
mod ui;

use crate::config::{CameraConfig, ResolvedConfig, StreamType};
use crate::layout::{FitMode, Layout};
use crate::log_buffer::LogBuffer;
use crate::nvr::rewrite_stream_digit;
use crate::ptz::{PtzVector, PtzWorker};
use crate::stats::SystemStats;
use crate::stream::{StreamDebugRow, StreamManager};
use crate::views::ViewStore;
use eframe::egui;
use egui::{Color32, ColorImage, Id, Sense, TextureHandle, TextureOptions};
use gilrs::Gilrs;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

#[derive(Clone, Debug)]
enum DragPayload {
    FromLibrary(String),
    FromSlot(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SidebarControls {
    #[default]
    Ptz,
    Presets,
}

struct TexCache {
    handle: TextureHandle,
    seq: u64,
}

struct UiPerf {
    frames: u64,
    uploads: u64,
    upload_bytes: u64,
    upload_ns: u64,
    cloned_frames: u64,
    upload_skips: u64,
    /// Worst UI tick gap (ms) in the sample window.
    frame_dt_max_ms: u64,
    /// Worst single texture upload (µs) in the sample window.
    upload_us_max: u64,
    /// Worst update_textures call (µs).
    tex_pass_us_max: u64,
    last_frame: Instant,
    last: Instant,
    ui_fps: f32,
    upload_fps: f32,
    upload_mbps: f32,
    avg_upload_us: f32,
    clone_fps: f32,
    skip_fps: f32,
    reported_frame_dt_max_ms: u64,
    reported_upload_us_max: u64,
    reported_tex_pass_us_max: u64,
}

impl UiPerf {
    fn new() -> Self {
        Self {
            frames: 0,
            uploads: 0,
            upload_bytes: 0,
            upload_ns: 0,
            cloned_frames: 0,
            upload_skips: 0,
            frame_dt_max_ms: 0,
            upload_us_max: 0,
            tex_pass_us_max: 0,
            last_frame: Instant::now(),
            last: Instant::now(),
            ui_fps: 0.0,
            upload_fps: 0.0,
            upload_mbps: 0.0,
            avg_upload_us: 0.0,
            clone_fps: 0.0,
            skip_fps: 0.0,
            reported_frame_dt_max_ms: 0,
            reported_upload_us_max: 0,
            reported_tex_pass_us_max: 0,
        }
    }

    fn tick_frame(&mut self) {
        let now = Instant::now();
        let dt_ms = now.saturating_duration_since(self.last_frame).as_millis() as u64;
        self.frame_dt_max_ms = self.frame_dt_max_ms.max(dt_ms);
        self.last_frame = now;
        self.frames += 1;
    }

    fn note_upload(&mut self, bytes: u64, ns: u64, cloned: bool) {
        self.uploads += 1;
        self.upload_bytes = self.upload_bytes.saturating_add(bytes);
        self.upload_ns = self.upload_ns.saturating_add(ns);
        let us = ns / 1000;
        self.upload_us_max = self.upload_us_max.max(us);
        if cloned {
            self.cloned_frames += 1;
        }
    }

    fn note_tex_pass(&mut self, ns: u64) {
        self.tex_pass_us_max = self.tex_pass_us_max.max(ns / 1000);
    }

    fn sample_rates(&mut self) {
        let dt = self.last.elapsed().as_secs_f32().max(0.001);
        self.ui_fps = self.frames as f32 / dt;
        self.upload_fps = self.uploads as f32 / dt;
        self.upload_mbps = (self.upload_bytes as f32 / dt) / (1024.0 * 1024.0);
        self.avg_upload_us = if self.uploads > 0 {
            (self.upload_ns as f32 / self.uploads as f32) / 1000.0
        } else {
            0.0
        };
        self.clone_fps = self.cloned_frames as f32 / dt;
        self.skip_fps = self.upload_skips as f32 / dt;
        self.reported_frame_dt_max_ms = self.frame_dt_max_ms;
        self.reported_upload_us_max = self.upload_us_max;
        self.reported_tex_pass_us_max = self.tex_pass_us_max;
        self.frames = 0;
        self.uploads = 0;
        self.upload_bytes = 0;
        self.upload_ns = 0;
        self.cloned_frames = 0;
        self.upload_skips = 0;
        self.frame_dt_max_ms = 0;
        self.upload_us_max = 0;
        self.tex_pass_us_max = 0;
        self.last = Instant::now();
    }
}

pub struct ViewerApp {
    /// Shown when config is missing / empty / failed to load.
    config_warning: Option<String>,
    cameras: Vec<CameraConfig>,
    camera_index: HashMap<String, usize>,
    streams: StreamManager,
    views: ViewStore,
    fit: FitMode,
    fullscreen_slot: Option<usize>,
    /// OS / monitor fullscreen. Tracked locally because `viewport().fullscreen`
    /// is often `None` on Windows even after `ViewportCommand::Fullscreen`.
    window_fullscreen: bool,
    /// HD = main stream (…01 / 101); off = sub (…02 / 102).
    hd: bool,
    app_name: String,
    pause_when_unfocused: bool,
    paused: bool,
    textures: HashMap<String, TexCache>,
    stats: SystemStats,
    sidebar_filter: String,
    /// Camera selected for PTZ / Presets (click a grid cell or the library).
    sidebar_ptz_cam: Option<String>,
    sidebar_controls: SidebarControls,
    rename_buffer: String,
    show_rename: bool,
    ptz: PtzWorker,
    gilrs: Option<Gilrs>,
    last_ptz: PtzVector,
    /// Edge-detect gamepad face buttons.
    last_cross: bool,
    last_triangle: bool,
    /// Perf overlay + verbose stream logging.
    debug_overlay: bool,
    debug_rows: Vec<StreamDebugRow>,
    last_debug_sample: Instant,
    last_debug_log: Instant,
    ui_perf: UiPerf,
    /// Append-only hitch diagnostics next to cameras.toml (every ~2s).
    stutter_log_path: PathBuf,
    /// In-app log / console window (OS console is hidden on Windows release builds).
    show_log: bool,
    show_settings: bool,
    /// Selection / outline color (default on-view blue).
    accent: Color32,
    outline_width: f32,
    camera_list_open: bool,
    sidebar_open: bool,
    ui_prefs_path: PathBuf,
    log_buffer: LogBuffer,
    log_auto_scroll: bool,
    log_view_generation: u64,
    log_view_lines: Vec<String>,
}

impl ViewerApp {
    pub fn new(
        cfg: ResolvedConfig,
        config_path: PathBuf,
        config_warning: Option<String>,
        log_buffer: LogBuffer,
    ) -> anyhow::Result<Self> {
        let layout = Layout::from_str(&cfg.viewer.default_layout);
        let fit = FitMode::from_str(&cfg.viewer.default_fit);
        let streams = StreamManager::new()?;

        let camera_ids: Vec<String> = cfg.cameras.iter().map(|c| c.id.clone()).collect();
        let mut camera_index = HashMap::new();
        for (i, cam) in cfg.cameras.iter().enumerate() {
            camera_index.insert(cam.id.clone(), i);
        }

        info!(cameras = camera_ids.len(), "resolved camera list");
        let stutter_log_path = config_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("stutter-stats.log");
        info!(
            path = %stutter_log_path.display(),
            "stutter diagnostics on — writing every 2s; perf overlay + log panel open"
        );

        let views_path = config_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("views.toml");
        let mut views = ViewStore::load_or_default(&views_path, &camera_ids, layout);

        let ui_prefs_path = settings::UiPrefs::path_next_to(&config_path);
        let ui_prefs = settings::UiPrefs::load(&ui_prefs_path);
        if let Some(name) = ui_prefs.last_view.as_deref().map(str::trim).filter(|s| !s.is_empty())
        {
            if let Some(i) = views.views.iter().position(|v| v.name == name) {
                views.active = i;
            }
        }
        let fit = ui_prefs
            .fit
            .as_deref()
            .map(FitMode::from_str)
            .unwrap_or(fit);

        let ptz = PtzWorker::spawn();

        let gilrs = match Gilrs::new() {
            Ok(g) => {
                info!("gamepad support ready");
                Some(g)
            }
            Err(err) => {
                warn!("gamepad init failed: {err}");
                None
            }
        };

        let app = Self {
            config_warning,
            cameras: cfg.cameras,
            camera_index,
            streams,
            views,
            fit,
            fullscreen_slot: None,
            window_fullscreen: false,
            hd: false,
            app_name: {
                let name = cfg.viewer.app_name.trim();
                if name.is_empty() {
                    crate::config::default_app_name()
                } else {
                    name.to_string()
                }
            },
            pause_when_unfocused: cfg.viewer.pause_when_unfocused,
            paused: false,
            textures: HashMap::new(),
            stats: SystemStats::new(),
            sidebar_filter: String::new(),
            sidebar_ptz_cam: None,
            sidebar_controls: SidebarControls::default(),
            rename_buffer: String::new(),
            show_rename: false,
            ptz,
            gilrs,
            last_ptz: PtzVector::STOP,
            last_cross: false,
            last_triangle: false,
            // Perf overlay: RUSTCAMS_DEBUG=1 or press D. stutter-stats.log always writes.
            debug_overlay: std::env::var_os("RUSTCAMS_DEBUG").is_some(),
            debug_rows: Vec::new(),
            last_debug_sample: Instant::now() - std::time::Duration::from_secs(2),
            last_debug_log: Instant::now(),
            ui_perf: UiPerf::new(),
            stutter_log_path,
            show_log: false,
            show_settings: false,
            accent: ui_prefs.accent_color(),
            outline_width: ui_prefs.outline_width(),
            camera_list_open: ui_prefs.camera_list_open,
            sidebar_open: ui_prefs.sidebar_open,
            ui_prefs_path,
            log_buffer,
            log_auto_scroll: true,
            log_view_generation: 0,
            log_view_lines: Vec::new(),
        };
        app.save_ui_prefs();
        Ok(app)
    }

    fn camera_by_id(&self, id: &str) -> Option<&CameraConfig> {
        self.camera_index
            .get(id)
            .and_then(|&i| self.cameras.get(i))
    }

    fn active_layout(&self) -> Layout {
        self.views.active_view().layout
    }

    fn persist_view_file(&mut self) {
        if let Err(err) = self.views.save_if_dirty() {
            warn!("failed to save views: {err:#}");
        }
    }

    fn persist_views(&mut self) {
        self.views.mark_dirty();
        self.persist_view_file();
        self.save_ui_prefs();
    }

    /// All cameras slotted in the active view.
    fn view_camera_ids(&self) -> Vec<String> {
        self.views
            .active_view()
            .slots
            .iter()
            .filter_map(|s| s.clone())
            .collect()
    }

    /// Cameras currently painted (fullscreen = one; otherwise the full grid).
    fn displayed_camera_ids(&self) -> Vec<String> {
        let view = self.views.active_view();
        if let Some(slot) = self.fullscreen_slot {
            return view
                .slots
                .get(slot)
                .and_then(|s| s.clone())
                .into_iter()
                .collect();
        }
        self.view_camera_ids()
    }

    /// With D3D11, solo modes drop off-screen decodes to free GPU/CPU.
    fn solo_decode_mode(&self) -> bool {
        self.streams.d3d11_available()
            && (self.fullscreen_slot.is_some()
                || matches!(self.active_layout(), Layout::One))
    }

    /// HD (main stream) on 1 / 2 / 2×2 layouts, or whenever a camera is fullscreen.
    fn hd_allowed(&self) -> bool {
        self.fullscreen_slot.is_some()
            || matches!(
                self.active_layout(),
                Layout::One | Layout::Two | Layout::Grid2
            )
    }

    /// Decode width / fps tier for a stream (fullscreen overrides grid tier).
    fn stream_tier(&self, fullscreen: bool) -> (i32, i32, StreamType) {
        if fullscreen {
            // Fullscreen always allows HD; default on when entering.
            if self.hd {
                return (1280, 30, StreamType::Main);
            }
            return (640, 15, StreamType::Sub);
        }
        let hd = self.hd
            && matches!(
                self.active_layout(),
                Layout::One | Layout::Two | Layout::Grid2
            );
        match self.active_layout() {
            Layout::One if hd => (1280, 30, StreamType::Main),
            Layout::One => (640, 15, StreamType::Sub),
            Layout::Two if hd => (960, 20, StreamType::Main),
            Layout::Two => (640, 15, StreamType::Sub),
            Layout::Grid2 if hd => (640, 15, StreamType::Main),
            Layout::Grid2 => (640, 15, StreamType::Sub),
            // Dense grids: sharp enough for OSD timestamps; SW decode handles this.
            Layout::Grid3 => (480, 15, StreamType::Sub),
            Layout::Grid4 => (400, 15, StreamType::Sub),
            Layout::Grid5 => (352, 15, StreamType::Sub),
            Layout::Grid6 => (288, 15, StreamType::Sub),
        }
    }

    fn desired_streams(&self) -> Vec<crate::stream::StreamRequest> {
        let fullscreen_id = self.fullscreen_slot.and_then(|slot| {
            self.views
                .active_view()
                .slots
                .get(slot)
                .and_then(|s| s.clone())
        });

        // D3D11 1×1 / camera-fullscreen: only the visible camera(s).
        // Otherwise keep the whole view decoding so leaving fullscreen is instant.
        let ids = if self.solo_decode_mode() {
            self.displayed_camera_ids()
        } else {
            self.view_camera_ids()
        };

        ids.into_iter()
            .filter_map(|id| {
                let cam = self.camera_by_id(&id)?;
                let fullscreen = fullscreen_id.as_deref() == Some(cam.id.as_str());
                let (max_width, max_fps, stream) = self.stream_tier(fullscreen);
                let (base_url, protocols) = if fullscreen {
                    if let Some(direct) = cam.direct_url.as_ref() {
                        // Direct camera LAN: prefer UDP (typical default).
                        (direct.clone(), None)
                    } else {
                        (cam.url.clone(), cam.protocols.clone())
                    }
                } else {
                    (cam.url.clone(), cam.protocols.clone())
                };
                Some(crate::stream::StreamRequest {
                    id: cam.id.clone(),
                    url: rewrite_stream_digit(&base_url, stream),
                    protocols,
                    max_width,
                    max_fps,
                })
            })
            .collect()
    }

    fn sync_streams(&mut self, active: bool) {
        if !active {
            if !self.paused {
                info!("window inactive — stopping all streams");
                self.streams.stop_all();
                self.textures.clear();
                self.paused = true;
            }
            return;
        }

        if self.paused {
            info!("window active — restarting streams");
            self.paused = false;
        }

        let desired = self.desired_streams();
        self.streams.sync_active(&desired);
        let keep: Vec<String> = desired.iter().map(|d| d.id.clone()).collect();
        self.textures.retain(|id, _| keep.contains(id));
    }

    fn update_textures(&mut self, ctx: &egui::Context) {
        let pass_t0 = Instant::now();
        let ids = self.displayed_camera_ids();
        if ids.is_empty() {
            return;
        }
        // Uploads are cheap (~3–30µs in stutter-stats); update every tile that has
        // a newer frame. Capping to 1–2/frame made dense grids look like 2fps.
        let dense = matches!(
            self.active_layout(),
            Layout::Grid3 | Layout::Grid4 | Layout::Grid5 | Layout::Grid6
        );
        let tex_opts = if dense {
            TextureOptions::NEAREST
        } else {
            TextureOptions::LINEAR
        };

        for id in ids {
            let seen = self.textures.get(&id).map(|t| t.seq).unwrap_or(0);
            let Some(frame) = self.streams.frame_if_newer(&id, seen) else {
                continue;
            };
            let seq = frame.seq;
            let t0 = Instant::now();
            let (image, bytes, cloned) = match Arc::try_unwrap(frame) {
                Ok(owned) => {
                    let bytes = owned.rgba.len() as u64;
                    (
                        color_image_from_rgba(
                            owned.width as usize,
                            owned.height as usize,
                            owned.rgba,
                        ),
                        bytes,
                        false,
                    )
                }
                Err(shared) => {
                    // Race with appsink: clone once rather than skip a present.
                    let bytes = shared.rgba.len() as u64;
                    (
                        color_image_from_rgba(
                            shared.width as usize,
                            shared.height as usize,
                            shared.rgba.clone(),
                        ),
                        bytes,
                        true,
                    )
                }
            };
            if let Some(entry) = self.textures.get_mut(&id) {
                entry.handle.set(image, tex_opts);
                entry.seq = seq;
            } else {
                let handle = ctx.load_texture(format!("cam-{id}"), image, tex_opts);
                self.textures.insert(
                    id,
                    TexCache {
                        handle,
                        seq,
                    },
                );
            }
            self.ui_perf
                .note_upload(bytes, t0.elapsed().as_nanos() as u64, cloned);
        }
        self.ui_perf
            .note_tex_pass(pass_t0.elapsed().as_nanos() as u64);
    }

    fn apply_drop(&mut self, slot_idx: usize, payload: DragPayload) {
        let n = self.views.active_view().slots.len();
        if slot_idx >= n {
            return;
        }
        match payload {
            DragPayload::FromLibrary(cam_id) => {
                if self.camera_by_id(&cam_id).is_none() {
                    return;
                }
                self.views.active_view_mut().slots[slot_idx] = Some(cam_id);
                self.views.mark_dirty();
            }
            DragPayload::FromSlot(from) => {
                if from >= n || from == slot_idx {
                    return;
                }
                self.views.active_view_mut().slots.swap(from, slot_idx);
                self.views.mark_dirty();
                if let Some(fs) = self.fullscreen_slot.as_mut() {
                    if *fs == from {
                        *fs = slot_idx;
                    } else if *fs == slot_idx {
                        *fs = from;
                    }
                }
            }
        }
        self.persist_view_file();
    }

    fn clear_slot(&mut self, slot_idx: usize) {
        if let Some(slot) = self.views.active_view_mut().slots.get_mut(slot_idx) {
            *slot = None;
            self.views.mark_dirty();
            self.persist_view_file();
        }
        if self.fullscreen_slot == Some(slot_idx) {
            self.exit_fullscreen();
        }
    }

    fn set_layout(&mut self, layout: Layout) {
        self.views.active_view_mut().resize_for_layout(layout);
        self.exit_fullscreen();
        self.views.mark_dirty();
        self.persist_view_file();
    }

    fn ptz_stop(&mut self) {
        self.ptz.stop();
        self.last_ptz = PtzVector::STOP;
    }

    /// Prefer the fullscreen camera when it has PTZ; otherwise the selection.
    fn active_ptz_target(&self) -> Option<crate::config::PtzTarget> {
        if let Some(slot) = self.fullscreen_slot {
            if let Some(cam_id) = self.views.active_view().slots.get(slot).and_then(|s| s.as_ref())
            {
                if let Some(cam) = self.camera_by_id(cam_id) {
                    if let Some(ptz) = cam.ptz.clone() {
                        return Some(ptz);
                    }
                }
            }
        }
        let cam_id = self.sidebar_ptz_cam.as_ref()?;
        self.camera_by_id(cam_id)?.ptz.clone()
    }

    fn select_camera(&mut self, cam_id: &str) {
        let Some(cam) = self.camera_by_id(cam_id) else {
            return;
        };
        let changed = self.sidebar_ptz_cam.as_deref() != Some(cam_id);
        if !changed {
            return;
        }
        let target = cam.ptz.clone();
        self.ptz_stop();
        self.sidebar_ptz_cam = Some(cam_id.to_string());
        if let Some(target) = target {
            self.ptz.prewarm(target.clone());
            self.ptz.fetch_presets(target.clone());
            self.ptz.fetch_park_action(target.clone());
            self.ptz.fetch_tracking(target);
        }
    }

    /// If a grid cell is selected, put `cam_id` in that cell (swap if it is already on the view).
    fn place_library_camera(&mut self, cam_id: &str) {
        if self.camera_by_id(cam_id).is_none() {
            return;
        }
        let selected = self.sidebar_ptz_cam.clone();
        let Some(selected) = selected else {
            self.select_camera(cam_id);
            return;
        };
        if selected.as_str() == cam_id {
            return;
        }
        let found = {
            let slots = &self.views.active_view().slots;
            slots
                .iter()
                .position(|s| s.as_deref() == Some(selected.as_str()))
                .map(|sel_idx| {
                    let already = slots.iter().position(|s| s.as_deref() == Some(cam_id));
                    (sel_idx, already)
                })
        };
        let Some((sel_idx, already)) = found else {
            self.select_camera(cam_id);
            return;
        };
        if let Some(other_idx) = already {
            self.apply_drop(sel_idx, DragPayload::FromSlot(other_idx));
        } else {
            self.apply_drop(sel_idx, DragPayload::FromLibrary(cam_id.to_string()));
        }
        self.select_camera(cam_id);
    }

    fn enter_fullscreen(&mut self, slot: usize) {
        if self.fullscreen_slot == Some(slot) {
            return;
        }
        self.ptz_stop();
        self.fullscreen_slot = Some(slot);
        // Fullscreen arms main/HD by default (toolbar stays toggleable).
        self.hd = true;
        if let Some(cam_id) = self
            .views
            .active_view()
            .slots
            .get(slot)
            .and_then(|s| s.as_ref())
            .cloned()
        {
            self.select_camera(&cam_id);
        } else if let Some(target) = self.active_ptz_target() {
            self.ptz.prewarm(target);
        }
    }

    fn exit_fullscreen(&mut self) {
        if self.fullscreen_slot.is_none() {
            return;
        }
        self.ptz_stop();
        self.fullscreen_slot = None;
    }

    fn set_window_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        if self.window_fullscreen == on {
            return;
        }
        self.window_fullscreen = on;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
    }

    /// Prefer our toggle state; adopt OS reports when egui actually provides them.
    fn sync_window_fullscreen(&mut self, ctx: &egui::Context) {
        // Only trust the OS when it reports leaving fullscreen. Entering is
        // driven by our toolbar toggle — on Windows `viewport().fullscreen` is
        // often still `None`/`true` for a frame after we send Fullscreen(false).
        if ctx.input(|i| i.viewport().fullscreen) == Some(false) {
            self.window_fullscreen = false;
        }
    }

    fn apply_ptz_vector(&mut self, vec: PtzVector) {
        let vec = vec.clamp();
        if vec == self.last_ptz {
            return;
        }
        self.last_ptz = vec;
        if let Some(target) = self.active_ptz_target() {
            self.ptz.set(target, vec);
        } else if vec.is_stop() {
            self.ptz.stop();
        }
    }

}

impl eframe::App for ViewerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui_perf.tick_frame();
        self.sync_window_fullscreen(ctx);
        self.apply_accent_visuals(ctx);
        self.handle_keys(ctx);
        self.handle_ptz_input(ctx);

        let focused = ctx.input(|i| i.focused);
        let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        let active = if self.pause_when_unfocused {
            focused && !minimized
        } else {
            true
        };

        if !active && !self.last_ptz.is_stop() {
            self.ptz_stop();
        }

        self.sync_streams(active);
        self.update_textures(ctx);
        self.refresh_debug();

        if !self.window_fullscreen {
            egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
                self.toolbar(ui);
            });

            if self.debug_overlay {
                egui::TopBottomPanel::bottom("statusbar")
                    .exact_height(26.0)
                    .frame(
                        egui::Frame::NONE
                            .fill(Color32::from_rgb(16, 18, 22))
                            .inner_margin(egui::Margin::symmetric(10, 4)),
                    )
                    .show(ctx, |ui| {
                        self.status_bar(ui);
                    });
            }

            if self.sidebar_open {
                egui::SidePanel::left("cameras")
                    .resizable(true)
                    .default_width(220.0)
                    .width_range(160.0..=360.0)
                    .frame(
                        egui::Frame::NONE
                            .fill(Color32::from_rgb(14, 16, 20))
                            .inner_margin(egui::Margin::symmetric(10, 8)),
                    )
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            if ui
                                .small_button("◀")
                                .on_hover_text("Collapse sidebar")
                                .clicked()
                            {
                                self.sidebar_open = false;
                                self.save_ui_prefs();
                            }
                        });
                        self.camera_sidebar(ui);
                    });
            } else {
                egui::SidePanel::left("cameras_collapsed")
                    .exact_width(28.0)
                    .resizable(false)
                    .frame(
                        egui::Frame::NONE
                            .fill(Color32::from_rgb(14, 16, 20))
                            .inner_margin(egui::Margin::symmetric(4, 6)),
                    )
                    .show(ctx, |ui| {
                        if ui
                            .small_button("▶")
                            .on_hover_text("Show sidebar")
                            .clicked()
                        {
                            self.sidebar_open = true;
                            self.save_ui_prefs();
                        }
                    });
            }
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(Color32::from_rgb(8, 9, 12))
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

                if self.cameras.is_empty() {
                    let msg = self.config_warning.clone().unwrap_or_else(|| {
                        "No cameras configured — edit cameras.toml to add streams.".into()
                    });
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new(msg)
                                .size(18.0)
                                .color(Color32::from_rgb(255, 190, 70)),
                        );
                    });
                    return;
                }

                if let Some(slot_idx) = self.fullscreen_slot {
                    let cam_id = self
                        .views
                        .active_view()
                        .slots
                        .get(slot_idx)
                        .cloned()
                        .flatten();
                    if let Some(id) = cam_id {
                        if let Some(cam) = self.camera_by_id(&id).cloned() {
                            let response = ui.interact(full, Id::new("fs"), Sense::click());
                            self.paint_cell_contents(ui, &cam, full, response.hovered(), false);
                            if response.double_clicked() {
                                self.exit_fullscreen();
                            }
                        }
                    } else {
                        self.exit_fullscreen();
                    }
                    return;
                }

                self.draw_grid(ui, full);
            });

        self.draw_debug_panel(ctx);
        self.draw_log_panel(ctx);
        self.settings_window(ctx);

        if let Err(err) = self.views.save_if_dirty() {
            warn!("failed to save views: {err:#}");
        }

        if active && !self.paused {
            // Present at ~60 Hz even when streams are capped at 15 fps. A 30 Hz
            // poll can alias with camera timestamps and make frame spacing uneven.
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
}

/// Video frames from videoconvert are opaque RGBA (`A=255`), which matches
/// egui `Color32`'s packed layout — retarget the buffer without a pixel walk.
fn color_image_from_rgba(width: usize, height: usize, rgba: Vec<u8>) -> ColorImage {
    let expected = width.saturating_mul(height).saturating_mul(4);
    assert_eq!(rgba.len(), expected, "rgba length must be width*height*4");
    let mut rgba = Vec::from(rgba.into_boxed_slice());
    debug_assert_eq!(rgba.len(), rgba.capacity());
    let len = width * height;
    let ptr = rgba.as_mut_ptr().cast::<Color32>();
    std::mem::forget(rgba);
    let pixels = unsafe { Vec::from_raw_parts(ptr, len, len) };
    ColorImage {
        size: [width, height],
        pixels,
    }
}
