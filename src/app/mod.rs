pub(crate) mod icons;
mod input;
mod settings;
mod ui;

use self::settings::{LibraryGroup, CANVAS_BG, PANEL_BG, STATUS_BG};
use crate::config::{
    absolute_path, default_cameras_toml, AppConfig, CameraConfig, ResolvedConfig, StreamType,
};
use crate::layout::{FitMode, Layout};
use crate::log_buffer::LogBuffer;
use crate::nvr::rewrite_stream_digit;
use crate::ptz::{PtzVector, PtzWorker};
use crate::stats::SystemStats;
use crate::stream::{StreamDebugRow, StreamManager};
use crate::views::ViewStore;
use eframe::egui;
use egui::{Color32, ColorImage, TextureHandle, TextureOptions};
use gilrs::Gilrs;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tracing::{info, warn};

#[derive(Clone, Debug)]
enum DragPayload {
    FromLibrary(String),
    FromSlot { view: usize, slot: usize },
}

#[derive(Debug, Clone)]
pub(super) enum UpdateBanner {
    Hidden,
    Offer(crate::update::AvailableUpdate),
    Downloading { version: String },
    Ready { version: String },
    Failed { message: String },
}

/// Status line for Settings → About (manual / auto check feedback).
#[derive(Debug, Clone, Default)]
pub(super) enum UpdateCheckStatus {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available { version: String },
    Failed { message: String },
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

struct NvrDiscoverJob {
    gen: u64,
    resolved: ResolvedConfig,
    warning: Option<String>,
    retry: bool,
}

struct AnprAlertUi {
    person: String,
    plate: String,
    at: Instant,
    texture: Option<TextureHandle>,
}

pub struct ViewerApp {
    /// Shown when config is missing / empty / failed to load.
    /// Shown when config is missing / empty / failed to load.
    config_warning: Option<String>,
    config_path: PathBuf,
    /// True when the user passed `cameras.toml` as argv[1] (do not rediscover).
    config_from_cli: bool,
    last_config_poll: Instant,
    config_mtime: Option<SystemTime>,
    /// Keep probing ISAPI until the NVR is reachable (login vs LAN race).
    nvr_retry: bool,
    nvr_job_gen: Arc<AtomicU64>,
    nvr_inflight: Arc<AtomicBool>,
    pending_nvr: Arc<Mutex<Option<NvrDiscoverJob>>>,
    /// Keep sending Maximized after map — XFCE session restore can undo the create hint.
    maximize_until: Instant,
    app_config: AppConfig,
    nvr_enabled: bool,
    nvr_draft: crate::config::NvrConfig,
    nvr_protocols: String,
    nvr_status: Option<String>,
    gate_draft: crate::config::GateConfig,
    gate_status: Arc<Mutex<Option<String>>>,
    gate_inflight: Arc<AtomicBool>,
    anpr_draft: crate::config::AnprConfig,
    anpr_status: Option<String>,
    anpr: crate::anpr::AnprWorker,
    anpr_test_inflight: Arc<AtomicBool>,
    /// `Ok((jpeg, sound_err))` — sound_err is set when playback failed (or muted).
    pending_anpr_test: Arc<Mutex<Option<Result<(Vec<u8>, Option<String>), String>>>>,
    /// Active watchlist popup (latest hit).
    anpr_alert: Option<AnprAlertUi>,
    /// Toolbar speaker: play RTSP audio for the selected camera.
    audio_enabled: bool,
    listen: crate::listen::ListenAudio,
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
    /// Grid slot waiting on “remove camera” confirmation.
    /// `(view_idx, slot)` waiting on “remove camera” confirmation.
    pending_clear_slot: Option<(usize, usize)>,
    pending_gate_confirm: bool,
    settings_tab: settings::SettingsTab,
    ptz: PtzWorker,
    gilrs: Option<Gilrs>,
    last_ptz: PtzVector,
    /// Edge-detect gamepad face buttons.
    last_cross: bool,
    last_triangle: bool,
    last_square: bool,
    last_l1: bool,
    last_gate_cancel: bool,
    last_dpad: (i8, i8),
    /// D-pad highlight on the main grid.
    pad_focus_slot: Option<usize>,
    /// D-pad highlight on Screen 2.
    aux_pad_focus_slot: Option<usize>,
    /// After PTZ target changes (select / camera fullscreen), ignore button
    /// *press* edges so a held Square does not fire patrol 1.
    ptz_rearm_buttons: bool,
    /// Keyboard/gamepad was commanding PTZ; send STOP once on release.
    ptz_hold_keys: bool,
    /// Perf overlay + verbose stream logging.
    debug_overlay: bool,
    debug_rows: Vec<StreamDebugRow>,
    last_debug_sample: Instant,
    last_debug_log: Instant,
    ui_perf: UiPerf,
    /// Append-only hitch diagnostics next to cameras.toml (every ~2s).
    stutter_log_path: PathBuf,
    stutter_log_file: bool,
    decode_1: i32,
    decode_1_hd: i32,
    decode_2: i32,
    decode_2_hd: i32,
    decode_2x2: i32,
    decode_2x2_hd: i32,
    decode_3x3: i32,
    decode_4x4: i32,
    decode_5x5: i32,
    decode_6x6: i32,
    decode_backend: crate::gst_link::DecodeBackend,
    /// Second monitor window (same process, shared selection / PTZ).
    aux_open: bool,
    aux_view: usize,
    aux_fullscreen_slot: Option<usize>,
    aux_focused: bool,
    /// Main viewport focused this frame (aux focus is sampled in its viewport).
    main_focused: bool,
    /// Last rustcams screen that had focus (`true` = Screen 2). PTZ outline stays here when both are in the background.
    last_ptz_screen_aux: bool,
    aux_window_fullscreen: bool,
    /// In-flight drag so drops work across the auxiliary viewport.
    cross_drag: Option<DragPayload>,
    aux_pointer_down: bool,
    /// In-app log / console window (OS console is hidden on Windows release builds).
    show_log: bool,
    show_settings: bool,
    /// Selection / outline color (default on-view blue).
    accent: Color32,
    outline_width: f32,
    /// Font + icon scale (1.0 = default).
    ui_scale: f32,
    camera_list_open: bool,
    sidebar_open: bool,
    /// Width of the camera library side panel (drag edge to resize).
    sidebar_width: f32,
    /// Ordered camera library groups (persisted in ui.toml).
    library_groups: Vec<LibraryGroup>,
    /// Group index being renamed in the sidebar, if any.
    library_renaming: Option<usize>,
    library_rename_buf: String,
    ui_prefs_path: PathBuf,
    log_buffer: LogBuffer,
    log_auto_scroll: bool,
    log_view_generation: u64,
    log_view_lines: Vec<String>,
    check_updates: bool,
    skipped_update: Option<String>,
    /// Hide the offer for this process only (Later / opt-out).
    update_later: bool,
    update_banner: UpdateBanner,
    last_update_offer: Option<crate::update::AvailableUpdate>,
    /// Result of a background GitHub check (`None` while idle / in flight).
    pending_update: Arc<Mutex<Option<crate::update::CheckOutcome>>>,
    update_apply: Arc<Mutex<Option<Result<String, String>>>>,
    update_progress: Arc<AtomicU64>,
    update_inflight: Arc<AtomicBool>,
    update_check_inflight: Arc<AtomicBool>,
    update_check_status: UpdateCheckStatus,
}

impl ViewerApp {
    pub fn new(
        app_config: AppConfig,
        cfg: ResolvedConfig,
        config_path: PathBuf,
        config_from_cli: bool,
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
        let (nvr_enabled, nvr_draft, nvr_protocols) = settings::nvr_edit_state(&app_config);

        let views_path = config_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("views.toml");
        let mut views = ViewStore::load_or_default(&views_path, &camera_ids, layout);

        let ui_prefs_path = settings::UiPrefs::path_next_to(&config_path);
        let ui_prefs = settings::UiPrefs::load(&ui_prefs_path);
        if let Some(name) = ui_prefs
            .last_view
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Some(i) = views.views.iter().position(|v| v.name == name) {
                views.active = i;
            }
        }
        let mut aux_view = 0usize;
        if let Some(name) = ui_prefs
            .last_aux_view
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Some(i) = views.views.iter().position(|v| v.name == name) {
                aux_view = i;
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

        if ui_prefs.stutter_log_file {
            info!(
                path = %stutter_log_path.display(),
                "stutter-stats.log enabled"
            );
        }

        let gate_draft = app_config.gate.clone().unwrap_or_default();
        let anpr_draft = app_config.anpr.clone().unwrap_or_default();
        let anpr_config_dir = config_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        let anpr = crate::anpr::AnprWorker::start(anpr_draft.clone(), anpr_config_dir);
        let config_mtime = std::fs::metadata(&config_path)
            .and_then(|m| m.modified())
            .ok();
        let mut app = Self {
            config_warning,
            config_path: config_path.clone(),
            config_from_cli,
            last_config_poll: Instant::now(),
            config_mtime,
            nvr_retry: app_config.nvr_discovery_enabled(),
            nvr_job_gen: Arc::new(AtomicU64::new(0)),
            nvr_inflight: Arc::new(AtomicBool::new(false)),
            pending_nvr: Arc::new(Mutex::new(None)),
            maximize_until: Instant::now() + Duration::from_secs(3),
            app_config,
            nvr_enabled,
            nvr_draft,
            nvr_protocols,
            nvr_status: None,
            gate_draft,
            gate_status: Arc::new(Mutex::new(None)),
            gate_inflight: Arc::new(AtomicBool::new(false)),
            anpr_draft,
            anpr_status: None,
            anpr,
            anpr_test_inflight: Arc::new(AtomicBool::new(false)),
            pending_anpr_test: Arc::new(Mutex::new(None)),
            anpr_alert: None,
            audio_enabled: ui_prefs.audio_enabled,
            listen: crate::listen::ListenAudio::new(),
            cameras: cfg.cameras,
            camera_index,
            streams,
            views,
            fit,
            fullscreen_slot: None,
            window_fullscreen: false,
            hd: false,
            app_name: crate::config::app_brand_name(&cfg.viewer.app_name),
            pause_when_unfocused: cfg.viewer.pause_when_unfocused,
            paused: false,
            textures: HashMap::new(),
            stats: SystemStats::new(),
            sidebar_filter: String::new(),
            sidebar_ptz_cam: None,
            sidebar_controls: SidebarControls::default(),
            rename_buffer: String::new(),
            show_rename: false,
            pending_clear_slot: None,
            pending_gate_confirm: false,
            settings_tab: settings::SettingsTab::default(),
            ptz,
            gilrs,
            last_ptz: PtzVector::STOP,
            last_cross: false,
            last_triangle: false,
            last_square: false,
            last_l1: false,
            last_gate_cancel: false,
            last_dpad: (0, 0),
            pad_focus_slot: None,
            aux_pad_focus_slot: None,
            ptz_rearm_buttons: false,
            ptz_hold_keys: false,
            debug_overlay: std::env::var_os("RUSTCAMS_DEBUG").is_some(),
            debug_rows: Vec::new(),
            last_debug_sample: Instant::now() - std::time::Duration::from_secs(2),
            last_debug_log: Instant::now(),
            ui_perf: UiPerf::new(),
            stutter_log_path,
            stutter_log_file: ui_prefs.stutter_log_file,
            decode_1: settings::clamp_decode_width(ui_prefs.decode_1),
            decode_1_hd: settings::clamp_decode_width(ui_prefs.decode_1_hd),
            decode_2: settings::clamp_decode_width(ui_prefs.decode_2),
            decode_2_hd: settings::clamp_decode_width(ui_prefs.decode_2_hd),
            decode_2x2: settings::clamp_decode_width(ui_prefs.decode_2x2),
            decode_2x2_hd: settings::clamp_decode_width(ui_prefs.decode_2x2_hd),
            decode_3x3: settings::clamp_decode_width(ui_prefs.decode_3x3),
            decode_4x4: settings::clamp_decode_width(ui_prefs.decode_4x4),
            decode_5x5: settings::clamp_decode_width(ui_prefs.decode_5x5),
            decode_6x6: settings::clamp_decode_width(ui_prefs.decode_6x6),
            decode_backend: ui_prefs.decode_backend,
            aux_open: false,
            aux_view,
            aux_fullscreen_slot: None,
            aux_focused: false,
            main_focused: false,
            last_ptz_screen_aux: false,
            aux_window_fullscreen: false,
            cross_drag: None,
            aux_pointer_down: false,
            show_log: false,
            show_settings: false,
            accent: ui_prefs.accent_color(),
            outline_width: ui_prefs.outline_width(),
            ui_scale: ui_prefs.ui_scale(),
            camera_list_open: ui_prefs.camera_list_open,
            sidebar_open: ui_prefs.sidebar_open,
            sidebar_width: settings::clamp_sidebar_width(ui_prefs.sidebar_width),
            library_groups: ui_prefs.library_groups,
            library_renaming: None,
            library_rename_buf: String::new(),
            ui_prefs_path,
            log_buffer,
            log_auto_scroll: true,
            log_view_generation: 0,
            log_view_lines: Vec::new(),
            check_updates: ui_prefs.check_updates,
            skipped_update: ui_prefs
                .skipped_update
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
            update_later: false,
            update_banner: UpdateBanner::Hidden,
            last_update_offer: None,
            pending_update: Arc::new(Mutex::new(None)),
            update_apply: Arc::new(Mutex::new(None)),
            update_progress: Arc::new(AtomicU64::new(0)),
            update_inflight: Arc::new(AtomicBool::new(false)),
            update_check_inflight: Arc::new(AtomicBool::new(false)),
            update_check_status: UpdateCheckStatus::Idle,
        };
        app.save_ui_prefs();
        app.ensure_library_groups(true);
        if app.nvr_retry {
            app.kick_nvr_resolve();
        }
        app.kick_update_check(false);
        Ok(app)
    }

    fn note_config_mtime(&mut self) {
        self.config_mtime = std::fs::metadata(&self.config_path)
            .and_then(|m| m.modified())
            .ok();
    }

    fn retarget_config_path(&mut self, path: PathBuf) {
        if path == self.config_path {
            return;
        }
        info!(
            from = %self.config_path.display(),
            to = %path.display(),
            "using cameras.toml"
        );
        self.config_path = path;
        let parent = self
            .config_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        self.stutter_log_path = parent.join("stutter-stats.log");
        self.ui_prefs_path = settings::UiPrefs::path_next_to(&self.config_path);
        self.views.path = parent.join("views.toml");
    }

    fn apply_loaded_config(
        &mut self,
        raw: AppConfig,
        resolved: ResolvedConfig,
        warning: Option<String>,
    ) {
        let cameras_changed = self.cameras != resolved.cameras;
        self.app_config = raw;
        if !self.show_settings {
            let (nvr_enabled, nvr_draft, nvr_protocols) =
                settings::nvr_edit_state(&self.app_config);
            self.nvr_enabled = nvr_enabled;
            self.nvr_draft = nvr_draft;
            self.nvr_protocols = nvr_protocols;
            self.gate_draft = self.app_config.gate.clone().unwrap_or_default();
            self.anpr_draft = self.app_config.anpr.clone().unwrap_or_default();
            self.restart_anpr_from_config();
        }
        self.app_name = crate::config::app_brand_name(&resolved.viewer.app_name);
        self.pause_when_unfocused = resolved.viewer.pause_when_unfocused;
        if cameras_changed {
            self.apply_resolved_cameras(resolved.cameras);
            let ids: Vec<String> = self.cameras.iter().map(|c| c.id.clone()).collect();
            let all_empty = self
                .views
                .views
                .iter()
                .all(|v| v.slots.iter().all(Option::is_none));
            if all_empty && !ids.is_empty() {
                if let Some(view) = self.views.views.first_mut() {
                    view.fill_from_cameras(&ids);
                }
                self.mark_views_dirty();
            }
        }
        self.config_warning = warning;
        self.note_config_mtime();
    }

    pub(super) fn kick_nvr_resolve(&mut self) {
        if !self.app_config.nvr_discovery_enabled() {
            self.nvr_retry = false;
            return;
        }
        if self.nvr_inflight.swap(true, Ordering::SeqCst) {
            return;
        }
        let gen = self.nvr_job_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let raw = self.app_config.clone();
        let slot = self.pending_nvr.clone();
        let inflight = self.nvr_inflight.clone();
        std::thread::spawn(move || {
            let (resolved, warning, retry) = match raw.clone().resolve() {
                Ok(resolved) => {
                    info!(
                        cameras = resolved.cameras.len(),
                        host = raw.nvr_host_label(),
                        "NVR camera list ready"
                    );
                    (resolved, None, false)
                }
                Err(err) => {
                    warn!(host = raw.nvr_host_label(), "NVR discovery failed: {err:#}");
                    let fallback = raw.resolve_direct_fallback();
                    let warning = Some(raw.waiting_for_nvr_message(Some(&format!("{err:#}"))));
                    (fallback, warning, true)
                }
            };
            *slot.lock() = Some(NvrDiscoverJob {
                gen,
                resolved,
                warning,
                retry,
            });
            inflight.store(false, Ordering::SeqCst);
        });
    }

    fn apply_pending_nvr(&mut self) {
        let job = self.pending_nvr.lock().take();
        let Some(job) = job else {
            return;
        };
        if job.gen != self.nvr_job_gen.load(Ordering::SeqCst) {
            return;
        }
        self.nvr_retry = job.retry;
        self.apply_loaded_config(self.app_config.clone(), job.resolved, job.warning);
    }

    fn poll_config_reload(&mut self) {
        self.apply_pending_nvr();

        if self.last_config_poll.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.last_config_poll = Instant::now();
        if self.show_settings {
            return;
        }

        if !self.config_from_cli && !self.config_path.is_file() {
            let discovered = absolute_path(default_cameras_toml());
            if discovered != self.config_path {
                self.retarget_config_path(discovered);
            }
        }

        let mtime = std::fs::metadata(&self.config_path)
            .and_then(|m| m.modified())
            .ok();
        if mtime != self.config_mtime {
            if mtime.is_none() {
                self.config_mtime = None;
                return;
            }
            match AppConfig::read(&self.config_path) {
                Ok(raw) => {
                    self.app_config = raw;
                    self.note_config_mtime();
                    if !self.show_settings {
                        let (nvr_enabled, nvr_draft, nvr_protocols) =
                            settings::nvr_edit_state(&self.app_config);
                        self.nvr_enabled = nvr_enabled;
                        self.nvr_draft = nvr_draft;
                        self.nvr_protocols = nvr_protocols;
                        self.gate_draft = self.app_config.gate.clone().unwrap_or_default();
                        self.anpr_draft = self.app_config.anpr.clone().unwrap_or_default();
                        self.restart_anpr_from_config();
                    }
                    self.app_name = crate::config::app_brand_name(&self.app_config.viewer.app_name);
                    self.pause_when_unfocused = self.app_config.viewer.pause_when_unfocused;
                    if self.app_config.nvr_discovery_enabled() {
                        let fallback = self.app_config.resolve_direct_fallback();
                        if self.cameras != fallback.cameras {
                            self.apply_resolved_cameras(fallback.cameras);
                        }
                        if self.cameras.is_empty() {
                            self.config_warning =
                                Some(self.app_config.waiting_for_nvr_message(None));
                        }
                        self.nvr_job_gen.fetch_add(1, Ordering::SeqCst);
                        self.nvr_retry = true;
                        self.kick_nvr_resolve();
                    } else {
                        self.nvr_retry = false;
                        match self.app_config.clone().resolve() {
                            Ok(resolved) => {
                                let warning = if resolved.cameras.is_empty() {
                                    Some(format!(
                                        "No cameras in {} — edit cameras.toml or Settings → NVR.",
                                        self.config_path.display()
                                    ))
                                } else {
                                    None
                                };
                                self.apply_loaded_config(
                                    self.app_config.clone(),
                                    resolved,
                                    warning,
                                );
                            }
                            Err(err) => {
                                self.config_warning = Some(format!(
                                    "Config needs setup ({}): {err:#}\nFix cameras.toml or Settings → NVR.",
                                    self.config_path.display()
                                ));
                            }
                        }
                    }
                }
                Err(err) => {
                    self.config_mtime = mtime;
                    self.config_warning = Some(format!(
                        "Config needs setup ({}): {err:#}\nEdit cameras.toml to configure cameras.",
                        self.config_path.display()
                    ));
                }
            }
            return;
        }

        if self.nvr_retry {
            self.kick_nvr_resolve();
        }
    }

    fn ensure_maximized_on_launch(&mut self, ctx: &egui::Context) {
        if self.window_fullscreen || Instant::now() >= self.maximize_until {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
    }

    pub(super) fn apply_resolved_cameras(&mut self, cameras: Vec<CameraConfig>) {
        let keep = self.sidebar_ptz_cam.clone();
        self.streams.stop_all();
        self.textures.clear();
        let mut camera_index = HashMap::new();
        for (i, cam) in cameras.iter().enumerate() {
            camera_index.insert(cam.id.clone(), i);
        }
        self.cameras = cameras;
        self.camera_index = camera_index;
        if let Some(id) = keep {
            if self.camera_index.contains_key(&id) {
                self.sidebar_ptz_cam = Some(id);
            } else {
                self.sidebar_ptz_cam = None;
                self.ptz_stop();
            }
        }
        self.ensure_library_groups(true);
    }

    /// Keep library groups in sync with the resolved camera list.
    ///
    /// Empty prefs → one "Cameras" group sorted alphabetically by display name.
    /// Unknown ids are pruned; newly discovered cameras are appended to the first
    /// group in alphabetical order.
    ///
    /// When `self.cameras` is empty (e.g. NVR discovery still in flight), do nothing —
    /// pruning against an empty inventory would wipe every group's membership and
    /// persist that to `ui.toml`.
    pub(super) fn ensure_library_groups(&mut self, persist: bool) {
        if self.cameras.is_empty() {
            return;
        }

        let mut name_by_id: HashMap<String, String> = HashMap::new();
        for cam in &self.cameras {
            name_by_id.insert(cam.id.clone(), cam.name.to_ascii_lowercase());
        }
        let sort_ids = |ids: &mut Vec<String>, names: &HashMap<String, String>| {
            ids.sort_by(|a, b| {
                let na = names.get(a).map(String::as_str).unwrap_or(a.as_str());
                let nb = names.get(b).map(String::as_str).unwrap_or(b.as_str());
                na.cmp(nb).then_with(|| a.cmp(b))
            });
        };

        let known: std::collections::HashSet<String> =
            self.cameras.iter().map(|c| c.id.clone()).collect();

        if self.library_groups.is_empty() {
            let mut ids: Vec<String> = self.cameras.iter().map(|c| c.id.clone()).collect();
            sort_ids(&mut ids, &name_by_id);
            self.library_groups.push(LibraryGroup {
                name: "Cameras".into(),
                open: true,
                cameras: ids,
            });
            if persist {
                self.save_ui_prefs();
            }
            return;
        }

        let mut changed = false;
        for g in &mut self.library_groups {
            let before = g.cameras.len();
            g.cameras.retain(|id| known.contains(id));
            if g.cameras.len() != before {
                changed = true;
            }
        }
        if self.library_groups.is_empty() {
            self.library_groups.push(LibraryGroup {
                name: "Cameras".into(),
                open: true,
                cameras: Vec::new(),
            });
            changed = true;
        }

        // Drop duplicate ids (keep the first group that lists each camera).
        let mut seen = std::collections::HashSet::new();
        for g in &mut self.library_groups {
            let before = g.cameras.len();
            g.cameras.retain(|id| seen.insert(id.clone()));
            if g.cameras.len() != before {
                changed = true;
            }
        }

        let mut assigned = std::collections::HashSet::new();
        for g in &self.library_groups {
            for id in &g.cameras {
                assigned.insert(id.clone());
            }
        }
        let mut orphans: Vec<String> = self
            .cameras
            .iter()
            .filter(|c| !assigned.contains(&c.id))
            .map(|c| c.id.clone())
            .collect();
        if !orphans.is_empty() {
            sort_ids(&mut orphans, &name_by_id);
            self.library_groups[0].cameras.extend(orphans);
            changed = true;
        }

        if changed && persist {
            self.save_ui_prefs();
        }
    }

    pub(super) fn add_library_group(&mut self) {
        self.ensure_library_groups(false);
        let n = self.library_groups.len() + 1;
        let name = format!("Group {n}");
        self.library_groups.push(LibraryGroup {
            name: name.clone(),
            open: true,
            cameras: Vec::new(),
        });
        self.library_renaming = Some(self.library_groups.len() - 1);
        self.library_rename_buf = name;
        self.save_ui_prefs();
    }

    pub(super) fn delete_library_group(&mut self, group_idx: usize) {
        if self.library_groups.len() <= 1 || group_idx >= self.library_groups.len() {
            return;
        }
        let removed = self.library_groups.remove(group_idx);
        self.library_groups[0].cameras.extend(removed.cameras);
        if self.library_renaming == Some(group_idx) {
            self.library_renaming = None;
        } else if let Some(r) = self.library_renaming {
            if r > group_idx {
                self.library_renaming = Some(r - 1);
            }
        }
        self.save_ui_prefs();
    }

    pub(super) fn finish_library_rename(&mut self) {
        let Some(idx) = self.library_renaming.take() else {
            return;
        };
        let name = self.library_rename_buf.trim().to_string();
        if name.is_empty() {
            return;
        }
        if let Some(g) = self.library_groups.get_mut(idx) {
            if g.name != name {
                g.name = name;
                self.save_ui_prefs();
            }
        }
    }

    /// Move `cam_id` into `dest_group` at `dest_index` (0 = top).
    pub(super) fn library_move_camera(
        &mut self,
        cam_id: &str,
        dest_group: usize,
        dest_index: usize,
    ) {
        if self.camera_by_id(cam_id).is_none() || self.library_groups.is_empty() {
            return;
        }
        let dest_group = dest_group.min(self.library_groups.len() - 1);

        let mut from_group = None;
        let mut from_index = None;
        for (gi, g) in self.library_groups.iter_mut().enumerate() {
            if let Some(pos) = g.cameras.iter().position(|c| c == cam_id) {
                g.cameras.remove(pos);
                from_group = Some(gi);
                from_index = Some(pos);
                break;
            }
        }

        let mut idx = dest_index;
        if from_group == Some(dest_group) {
            if let Some(fi) = from_index {
                if fi < idx {
                    idx = idx.saturating_sub(1);
                }
            }
        }
        let g = &mut self.library_groups[dest_group];
        idx = idx.min(g.cameras.len());
        if from_group == Some(dest_group) && from_index == Some(idx) {
            // No-op reinsert at same place — still put it back.
        }
        g.cameras.insert(idx, cam_id.to_string());
        self.save_ui_prefs();
    }

    fn clamp_aux_view(&mut self) {
        self.aux_view = self.views.clamp_index(self.aux_view);
    }

    fn ensure_distinct_aux_view(&mut self) {
        self.clamp_aux_view();
        if self.aux_view != self.views.active {
            return;
        }
        if let Some(i) = (0..self.views.views.len()).find(|&i| i != self.views.active) {
            self.aux_view = i;
            return;
        }
        let layout = self.active_layout();
        self.views
            .views
            .push(crate::views::View::new("Screen 2", layout));
        self.aux_view = self.views.views.len() - 1;
        self.mark_views_dirty();
    }

    fn camera_by_id(&self, id: &str) -> Option<&CameraConfig> {
        self.camera_index.get(id).and_then(|&i| self.cameras.get(i))
    }

    fn active_layout(&self) -> Layout {
        self.views.active_view().layout
    }

    fn persist_view_file(&mut self) {
        if let Err(err) = self.views.save() {
            warn!("failed to save views: {err:#}");
        }
    }

    /// Mark views dirty without writing `views.toml` (manual Save button).
    fn mark_views_dirty(&mut self) {
        self.views.mark_dirty();
    }

    /// Write views to disk and refresh ui.toml (last view name, etc.).
    fn save_views_now(&mut self) {
        self.persist_view_file();
        self.save_ui_prefs();
    }

    /// All cameras slotted in a view.
    fn view_camera_ids_at(&self, view_idx: usize) -> Vec<String> {
        self.views
            .view(view_idx)
            .slots
            .iter()
            .filter_map(|s| s.clone())
            .collect()
    }

    /// All cameras slotted in the active view.
    fn view_camera_ids(&self) -> Vec<String> {
        self.view_camera_ids_at(self.views.active)
    }

    fn displayed_camera_ids_at(
        &self,
        view_idx: usize,
        fullscreen_slot: Option<usize>,
    ) -> Vec<String> {
        let view = self.views.view(view_idx);
        if let Some(slot) = fullscreen_slot {
            return view
                .slots
                .get(slot)
                .and_then(|s| s.clone())
                .into_iter()
                .collect();
        }
        self.view_camera_ids_at(view_idx)
    }

    /// Cameras currently painted (fullscreen = one; otherwise the full grid).
    fn displayed_camera_ids(&self) -> Vec<String> {
        self.displayed_camera_ids_at(self.views.active, self.fullscreen_slot)
    }

    fn all_painted_camera_ids(&self) -> Vec<String> {
        let mut ids = self.displayed_camera_ids();
        if self.aux_open {
            ids.extend(self.displayed_camera_ids_at(self.aux_view, self.aux_fullscreen_slot));
        }
        ids.sort();
        ids.dedup();
        ids
    }

    fn hd_allowed(&self) -> bool {
        self.fullscreen_slot.is_some()
            || self.aux_fullscreen_slot.is_some()
            || matches!(
                self.active_layout(),
                Layout::One | Layout::Two | Layout::Grid2
            )
            || (self.aux_open
                && matches!(
                    self.views.view(self.aux_view).layout,
                    Layout::One | Layout::Two | Layout::Grid2
                ))
    }

    /// Decode width / fps tier for a stream (fullscreen overrides grid tier).
    fn stream_tier_for(&self, layout: Layout, fullscreen: bool) -> (i32, i32, StreamType) {
        if fullscreen {
            if self.hd {
                return (self.decode_1_hd, 30, StreamType::Main);
            }
            return (self.decode_1, 15, StreamType::Sub);
        }
        let hd = self.hd && matches!(layout, Layout::One | Layout::Two | Layout::Grid2);
        match layout {
            Layout::One if hd => (self.decode_1_hd, 30, StreamType::Main),
            Layout::One => (self.decode_1, 15, StreamType::Sub),
            Layout::Two if hd => (self.decode_2_hd, 20, StreamType::Main),
            Layout::Two => (self.decode_2, 15, StreamType::Sub),
            Layout::Grid2 if hd => (self.decode_2x2_hd, 15, StreamType::Main),
            Layout::Grid2 => (self.decode_2x2, 15, StreamType::Sub),
            Layout::Grid3 => (self.decode_3x3, 15, StreamType::Sub),
            Layout::Grid4 => (self.decode_4x4, 15, StreamType::Sub),
            Layout::Grid5 => (self.decode_5x5, 15, StreamType::Sub),
            Layout::Grid6 => (self.decode_6x6, 15, StreamType::Sub),
        }
    }

    fn merge_stream_request(
        into: &mut HashMap<String, crate::stream::StreamRequest>,
        req: crate::stream::StreamRequest,
    ) {
        match into.get_mut(&req.id) {
            Some(existing) => {
                existing.max_width = existing.max_width.max(req.max_width);
                existing.max_fps = existing.max_fps.max(req.max_fps);
                if req.url.contains("01") || req.url.contains("101") {
                    existing.url = req.url;
                    existing.protocols = req.protocols;
                }
            }
            None => {
                into.insert(req.id.clone(), req);
            }
        }
    }

    fn collect_view_streams(
        &self,
        view_idx: usize,
        fullscreen_slot: Option<usize>,
        into: &mut HashMap<String, crate::stream::StreamRequest>,
    ) {
        let view = self.views.view(view_idx);
        let layout = view.layout;
        let solo = self.streams.d3d11_available()
            && (fullscreen_slot.is_some() || matches!(layout, Layout::One));
        let fullscreen_id =
            fullscreen_slot.and_then(|slot| view.slots.get(slot).and_then(|s| s.clone()));
        let ids = if solo {
            self.displayed_camera_ids_at(view_idx, fullscreen_slot)
        } else {
            self.view_camera_ids_at(view_idx)
        };
        for id in ids {
            let Some(cam) = self.camera_by_id(&id) else {
                continue;
            };
            let fullscreen = fullscreen_id.as_deref() == Some(cam.id.as_str());
            let (max_width, max_fps, stream) = self.stream_tier_for(layout, fullscreen);
            let (base_url, protocols) = if fullscreen {
                if let Some(direct) = cam.direct_url.as_ref() {
                    (direct.clone(), None)
                } else {
                    (cam.url.clone(), cam.protocols.clone())
                }
            } else {
                (cam.url.clone(), cam.protocols.clone())
            };
            Self::merge_stream_request(
                into,
                crate::stream::StreamRequest {
                    id: cam.id.clone(),
                    url: rewrite_stream_digit(&base_url, stream),
                    protocols,
                    max_width,
                    max_fps,
                    decode: crate::gst_link::DecodeBackend::resolve(self.decode_backend),
                },
            );
        }
    }

    fn desired_streams(&self) -> Vec<crate::stream::StreamRequest> {
        let mut map = HashMap::new();
        self.collect_view_streams(self.views.active, self.fullscreen_slot, &mut map);
        if self.aux_open {
            self.collect_view_streams(self.aux_view, self.aux_fullscreen_slot, &mut map);
        }
        map.into_values().collect()
    }

    fn sync_streams(&mut self, active: bool) {
        if !active {
            if !self.paused {
                info!("window inactive — stopping all streams");
                self.streams.stop_all();
                self.listen.stop();
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
        self.sync_listen_audio();
    }

    fn update_textures(&mut self, ctx: &egui::Context) {
        let pass_t0 = Instant::now();
        let ids = self.all_painted_camera_ids();
        if ids.is_empty() {
            return;
        }
        // Uploads are cheap (~3–30µs in stutter-stats); update every tile that has
        // a newer frame. Capping to 1–2/frame made dense grids look like 2fps.
        let dense = {
            let main_dense = matches!(
                self.active_layout(),
                Layout::Grid3 | Layout::Grid4 | Layout::Grid5 | Layout::Grid6
            );
            let aux_dense = self.aux_open
                && matches!(
                    self.views.view(self.aux_view).layout,
                    Layout::Grid3 | Layout::Grid4 | Layout::Grid5 | Layout::Grid6
                );
            main_dense || aux_dense
        };
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
                self.textures.insert(id, TexCache { handle, seq });
            }
            self.ui_perf
                .note_upload(bytes, t0.elapsed().as_nanos() as u64, cloned);
        }
        self.ui_perf
            .note_tex_pass(pass_t0.elapsed().as_nanos() as u64);
    }

    fn apply_drop(&mut self, view_idx: usize, slot_idx: usize, payload: DragPayload) {
        let n = self.views.view(view_idx).slots.len();
        if slot_idx >= n {
            return;
        }
        match payload {
            DragPayload::FromLibrary(cam_id) => {
                if self.camera_by_id(&cam_id).is_none() {
                    return;
                }
                self.views.view_mut(view_idx).slots[slot_idx] = Some(cam_id);
                self.views.mark_dirty();
            }
            DragPayload::FromSlot {
                view: src_view,
                slot: from,
            } => {
                if src_view == view_idx {
                    if from >= n || from == slot_idx {
                        return;
                    }
                    self.views.view_mut(view_idx).slots.swap(from, slot_idx);
                    self.views.mark_dirty();
                    let remap = |cur: &mut Option<usize>| {
                        if let Some(fs) = cur.as_mut() {
                            if *fs == from {
                                *fs = slot_idx;
                            } else if *fs == slot_idx {
                                *fs = from;
                            }
                        }
                    };
                    if view_idx == self.views.active {
                        remap(&mut self.fullscreen_slot);
                    }
                    if view_idx == self.aux_view {
                        remap(&mut self.aux_fullscreen_slot);
                    }
                } else {
                    let cam = self.views.view(src_view).slots.get(from).cloned().flatten();
                    let Some(cam) = cam else {
                        return;
                    };
                    self.views.view_mut(view_idx).slots[slot_idx] = Some(cam);
                    self.views.mark_dirty();
                }
            }
        }
    }

    fn clear_slot(&mut self, view_idx: usize, slot_idx: usize) {
        if let Some(slot) = self.views.view_mut(view_idx).slots.get_mut(slot_idx) {
            *slot = None;
            self.views.mark_dirty();
        }
        if self.fullscreen_slot == Some(slot_idx) && view_idx == self.views.active {
            self.exit_fullscreen();
        }
        if self.aux_fullscreen_slot == Some(slot_idx) && view_idx == self.aux_view {
            self.aux_fullscreen_slot = None;
        }
    }

    fn set_layout_at(&mut self, view_idx: usize, layout: Layout) {
        self.views.view_mut(view_idx).resize_for_layout(layout);
        if view_idx == self.views.active {
            self.exit_fullscreen();
        }
        if view_idx == self.aux_view {
            self.aux_fullscreen_slot = None;
        }
        self.views.mark_dirty();
    }

    fn ptz_stop(&mut self) {
        self.ptz.stop();
        self.last_ptz = PtzVector::STOP;
    }

    /// PTZ follows the single selected camera (never two at once).
    fn active_ptz_target(&self) -> Option<crate::config::PtzTarget> {
        let cam_id = self.sidebar_ptz_cam.as_ref()?;
        self.camera_by_id(cam_id)?.ptz.clone()
    }

    /// PTZ outline on this screen when it is focused, or when no rustcams window is focused and this was last used.
    pub(super) fn show_ptz_selection_here(&self, is_aux: bool, screen_focused: bool) -> bool {
        if screen_focused {
            return true;
        }
        let other_focused = if is_aux {
            self.main_focused
        } else {
            self.aux_focused
        };
        if other_focused {
            return false;
        }
        self.last_ptz_screen_aux == is_aux
    }

    /// Gamepad grid nav / fullscreen follows the focused screen, else the last used one.
    pub(super) fn gamepad_targets_aux(&self) -> bool {
        self.aux_open && (self.aux_focused || (!self.main_focused && self.last_ptz_screen_aux))
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
        self.ptz_rearm_buttons = true;
        self.sidebar_ptz_cam = Some(cam_id.to_string());
        if let Some(target) = target {
            self.ptz.prewarm(target.clone());
            self.ptz.fetch_presets(target);
        }
        self.sync_listen_audio();
    }

    fn clear_camera_selection(&mut self) {
        if self.sidebar_ptz_cam.is_none() {
            return;
        }
        self.ptz_stop();
        self.ptz_rearm_buttons = true;
        self.sidebar_ptz_cam = None;
        self.sync_listen_audio();
    }

    /// Put `cam_id` on the single-clicked / pad-focused slot of `view_idx`.
    fn assign_camera_to_selected_slot(&mut self, cam_id: &str, view_idx: usize, is_aux: bool) {
        if self.camera_by_id(cam_id).is_none() {
            return;
        }
        let n = self.views.view(view_idx).slots.len();
        if n == 0 {
            return;
        }
        let focused = if is_aux {
            self.aux_pad_focus_slot
        } else {
            self.pad_focus_slot
        };
        let slot_idx = focused
            .filter(|&i| i < n)
            .or_else(|| {
                let selected = self.sidebar_ptz_cam.as_deref()?;
                self.views
                    .view(view_idx)
                    .slots
                    .iter()
                    .position(|s| s.as_deref() == Some(selected))
            })
            .unwrap_or(0);

        let already = self
            .views
            .view(view_idx)
            .slots
            .iter()
            .position(|s| s.as_deref() == Some(cam_id));
        if let Some(other_idx) = already {
            if other_idx != slot_idx {
                self.apply_drop(
                    view_idx,
                    slot_idx,
                    DragPayload::FromSlot {
                        view: view_idx,
                        slot: other_idx,
                    },
                );
            }
        } else {
            self.apply_drop(
                view_idx,
                slot_idx,
                DragPayload::FromLibrary(cam_id.to_string()),
            );
        }
        if is_aux {
            self.aux_pad_focus_slot = Some(slot_idx);
        } else {
            self.pad_focus_slot = Some(slot_idx);
        }
        self.select_camera(cam_id);
    }

    /// Fill `view_idx` with every camera in a library group (auto-picks a layout that fits).
    fn play_library_group(&mut self, group_idx: usize, view_idx: usize, is_aux: bool) {
        let ids: Vec<String> = match self.library_groups.get(group_idx) {
            Some(g) => g
                .cameras
                .iter()
                .filter(|id| self.camera_by_id(id).is_some())
                .cloned()
                .collect(),
            None => return,
        };
        if ids.is_empty() {
            return;
        }
        let layout = Layout::all()
            .iter()
            .copied()
            .find(|l| l.cells() >= ids.len())
            .unwrap_or(Layout::Grid6);
        self.set_layout_at(view_idx, layout);
        self.views.view_mut(view_idx).fill_from_cameras(&ids);
        self.views.mark_dirty();
        if is_aux {
            self.aux_pad_focus_slot = Some(0);
        } else {
            self.pad_focus_slot = Some(0);
        }
        if let Some(id) = ids.first() {
            self.select_camera(id);
        }
    }

    pub(super) fn sync_listen_audio(&mut self) {
        let target = if self.audio_enabled && !self.paused {
            self.sidebar_ptz_cam.as_ref().and_then(|id| {
                let cam = self.camera_by_id(id)?;
                Some(crate::listen::ListenTarget {
                    id: id.clone(),
                    url: cam.url.clone(),
                    protocols: cam.protocols.clone(),
                })
            })
        } else {
            None
        };
        // Keep enabled flag on the listener; sync only switches the target.
        if self.listen.enabled() != self.audio_enabled {
            self.listen.set_enabled(self.audio_enabled);
        }
        self.listen.sync(target);
    }

    fn enter_fullscreen_at(&mut self, view_idx: usize, slot: usize, is_aux: bool) {
        let fs = if is_aux {
            &mut self.aux_fullscreen_slot
        } else {
            &mut self.fullscreen_slot
        };
        if *fs == Some(slot) {
            return;
        }
        *fs = Some(slot);
        if !is_aux {
            self.pad_focus_slot = Some(slot);
        }
        self.ptz_rearm_buttons = true;
        self.hd = true;
        if let Some(cam_id) = self
            .views
            .view(view_idx)
            .slots
            .get(slot)
            .and_then(|s| s.as_ref())
            .cloned()
        {
            self.select_camera(&cam_id);
        }
    }

    fn exit_fullscreen(&mut self) {
        if self.fullscreen_slot.is_none() {
            return;
        }
        self.fullscreen_slot = None;
        self.ptz_rearm_buttons = true;
    }

    fn exit_aux_fullscreen(&mut self) {
        if self.aux_fullscreen_slot.is_none() {
            return;
        }
        self.aux_fullscreen_slot = None;
        self.ptz_rearm_buttons = true;
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

    pub(super) fn prompt_open_gates(&mut self) {
        if !self.gate_draft.is_configured() {
            *self.gate_status.lock() = Some("Set gate host and user in Settings.".into());
            return;
        }
        if self.gate_busy() {
            return;
        }
        self.pending_gate_confirm = true;
    }

    pub(super) fn open_gates(&mut self) {
        use std::sync::atomic::Ordering;
        if !self.gate_draft.is_configured() {
            *self.gate_status.lock() = Some("Set gate host and user in Settings.".into());
            return;
        }
        if self
            .gate_inflight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        *self.gate_status.lock() = Some("Opening gates…".into());
        let cfg = self.gate_draft.clone();
        let status = self.gate_status.clone();
        let inflight = self.gate_inflight.clone();
        std::thread::spawn(move || {
            let msg = match crate::gate::remote_control_door(&cfg) {
                Ok(()) => "Gates opened.".to_string(),
                Err(err) => format!("Gate failed: {err:#}"),
            };
            info!("{msg}");
            *status.lock() = Some(msg);
            inflight.store(false, Ordering::SeqCst);
        });
    }

    pub(super) fn gate_status_text(&self) -> Option<String> {
        self.gate_status.lock().clone()
    }

    pub(super) fn gate_busy(&self) -> bool {
        self.gate_inflight.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(super) fn restart_anpr_from_config(&mut self) {
        let dir = self
            .config_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let cfg = self.app_config.anpr.clone().unwrap_or_default();
        self.anpr_draft = cfg.clone();
        self.anpr.apply_config(cfg, dir);
    }

    pub(super) fn poll_anpr_alerts(&mut self, ctx: &egui::Context) {
        if let Some(result) = self.pending_anpr_test.lock().take() {
            ctx.request_repaint();
            match result {
                Ok((jpeg, sound_note)) => {
                    let texture = image::load_from_memory(&jpeg).ok().map(|img| {
                        let rgba = img.to_rgba8();
                        let size = [rgba.width() as usize, rgba.height() as usize];
                        let color = ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                        ctx.load_texture("anpr_alert", color, TextureOptions::LINEAR)
                    });
                    self.anpr_status = Some(match sound_note.as_deref() {
                        None => format!("Test OK — snapshot + alert sound ({} bytes).", jpeg.len()),
                        Some("muted") => format!(
                            "Test snapshot OK ({} bytes); audio muted.",
                            jpeg.len()
                        ),
                        Some(err) => format!(
                            "Test snapshot OK ({} bytes); sound failed: {err}",
                            jpeg.len()
                        ),
                    });
                    self.anpr_alert = Some(AnprAlertUi {
                        person: "Test snapshot".into(),
                        plate: self.anpr_draft.host.trim().to_string(),
                        at: Instant::now(),
                        texture,
                    });
                }
                Err(err) => {
                    self.anpr_status = Some(format!("Test snapshot failed: {err}"));
                    warn!("ANPR test snapshot failed: {err}");
                }
            }
        }

        // Don't overwrite a fresh test-status message with the listener status.
        if self.anpr_status.as_deref().is_none_or(|s| {
            !s.starts_with("Test snapshot") && !s.starts_with("Fetching test")
        }) {
            self.anpr_status = self.anpr.status();
        }
        if self.anpr.needs_repaint() {
            ctx.request_repaint();
        }
        let incoming = self.anpr.take_alerts();
        if incoming.is_empty() {
            return;
        }
        ctx.request_repaint();
        if let Some(alert) = incoming.into_iter().last() {
            let texture = alert.image_jpeg.as_ref().and_then(|jpeg| {
                image::load_from_memory(jpeg).ok().map(|img| {
                    let rgba = img.to_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    let color = ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                    ctx.load_texture("anpr_alert", color, TextureOptions::LINEAR)
                })
            });
            self.anpr_alert = Some(AnprAlertUi {
                person: alert.person,
                plate: alert.plate,
                at: alert.at,
                texture,
            });
        }
    }

    pub(super) fn test_anpr_snapshot(&mut self) {
        if self.anpr_draft.host.trim().is_empty() {
            self.anpr_status = Some("Set ANPR host first.".into());
            return;
        }
        if self.anpr_draft.username.trim().is_empty() {
            self.anpr_status = Some("Set ANPR username first.".into());
            return;
        }
        if let Err(err) = crate::config::validate_host(&self.anpr_draft.host) {
            self.anpr_status = Some(format!("Invalid ANPR host: {err:#}"));
            return;
        }
        if self
            .anpr_test_inflight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            self.anpr_status = Some("Test snapshot already in progress…".into());
            return;
        }
        self.anpr_status = Some("Fetching test snapshot…".into());
        let mut cfg = self.anpr_draft.clone();
        if cfg.channel == 0 {
            cfg.channel = 1;
        }
        let play_audio = !cfg.silent;
        let config_dir = self
            .config_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let slot = self.pending_anpr_test.clone();
        let inflight = self.anpr_test_inflight.clone();
        std::thread::spawn(move || {
            let result = match crate::anpr::fetch_snapshot(&cfg) {
                Ok(jpeg) => {
                    // Show the popup immediately; play sound in parallel (do not block UI).
                    let sound_note = if play_audio {
                        None
                    } else {
                        Some("muted".into())
                    };
                    *slot.lock() = Some(Ok((jpeg, sound_note)));
                    inflight.store(false, Ordering::SeqCst);
                    if play_audio {
                        std::thread::spawn(move || {
                            if let Err(err) = crate::anpr::play_sound(
                                crate::config::ANPR_SOUND_ALERT,
                                &config_dir,
                            ) {
                                warn!(error = %err, "ANPR test alert sound failed");
                            }
                        });
                    }
                    return;
                }
                Err(err) => Err(format!("{err:#}")),
            };
            *slot.lock() = Some(result);
            inflight.store(false, Ordering::SeqCst);
        });
    }

    /// Start a GitHub release check.
    ///
    /// - `manual`: from Settings → About; ignores the check-on-launch toggle and
    ///   `update_later`, and does not treat a skipped version as quiet.
    /// - automatic: requires `check_updates`, skips if a check/download is already
    ///   running; shows a banner and waits for confirmation before download.
    fn kick_update_check(&mut self, manual: bool) {
        if !manual && !self.check_updates {
            return;
        }
        let Some(path) = crate::update::appimage_path() else {
            if manual {
                self.update_check_status = UpdateCheckStatus::Failed {
                    message: "Updates are only available for the Linux AppImage.".into(),
                };
            } else if std::env::var_os("APPDIR").is_some() {
                warn!("AppImage update check skipped: $APPIMAGE missing or not a file");
            }
            return;
        };
        if self.update_inflight.load(Ordering::SeqCst) {
            if manual {
                self.update_check_status = UpdateCheckStatus::Checking;
            }
            return;
        }
        if self.update_check_inflight.swap(true, Ordering::SeqCst) {
            if manual {
                self.update_check_status = UpdateCheckStatus::Checking;
            }
            return;
        }

        self.update_check_status = UpdateCheckStatus::Checking;
        if manual {
            self.update_later = false;
        }

        info!(
            path = %path.display(),
            manual,
            "checking GitHub for AppImage updates"
        );
        let skipped = if manual {
            None
        } else {
            self.skipped_update.clone()
        };
        let slot = self.pending_update.clone();
        let inflight = self.update_check_inflight.clone();
        std::thread::spawn(move || {
            let outcome =
                crate::update::check_status(env!("CARGO_PKG_VERSION"), skipped.as_deref());
            if let crate::update::CheckOutcome::Available(ref offer) = outcome {
                info!(tag = %offer.tag, version = %offer.version, "AppImage update available");
            }
            *slot.lock() = Some(outcome);
            inflight.store(false, Ordering::SeqCst);
        });
    }

    fn poll_update_check(&mut self) {
        let apply_res = self.update_apply.lock().take();
        if let Some(res) = apply_res {
            self.update_inflight.store(false, Ordering::SeqCst);
            match res {
                Ok(version) => {
                    self.update_banner = UpdateBanner::Ready {
                        version: version.clone(),
                    };
                    self.update_check_status = UpdateCheckStatus::Available { version };
                }
                Err(message) => {
                    self.update_banner = UpdateBanner::Failed {
                        message: message.clone(),
                    };
                    self.update_check_status = UpdateCheckStatus::Failed { message };
                }
            }
        }

        let outcome = self.pending_update.lock().take();
        if let Some(outcome) = outcome {
            match outcome {
                crate::update::CheckOutcome::UpToDate => {
                    self.update_check_status = UpdateCheckStatus::UpToDate;
                }
                crate::update::CheckOutcome::Failed(message) => {
                    self.update_check_status = UpdateCheckStatus::Failed { message };
                }
                crate::update::CheckOutcome::Available(offer) => {
                    self.update_check_status = UpdateCheckStatus::Available {
                        version: offer.tag.clone(),
                    };
                    self.last_update_offer = Some(offer.clone());
                    if !self.update_later && matches!(self.update_banner, UpdateBanner::Hidden) {
                        self.update_banner = UpdateBanner::Offer(offer);
                    }
                }
            }
        }
    }

    fn start_update_download(&mut self) {
        let Some(dest) = crate::update::appimage_path() else {
            self.update_banner = UpdateBanner::Failed {
                message: "Not running from an AppImage.".into(),
            };
            return;
        };
        let Some(offer) = self.last_update_offer.clone() else {
            return;
        };
        if self.update_inflight.swap(true, Ordering::SeqCst) {
            return;
        }
        self.update_progress.store(0, Ordering::Relaxed);
        self.update_banner = UpdateBanner::Downloading {
            version: offer.tag.clone(),
        };
        self.update_check_status = UpdateCheckStatus::Checking;
        let progress = self.update_progress.clone();
        let apply = self.update_apply.clone();
        std::thread::spawn(move || {
            let result = crate::update::download_and_replace(&dest, &offer, &progress)
                .map(|()| offer.tag.clone())
                .map_err(|err| format!("{err:#}"));
            if let Err(msg) = &result {
                warn!("AppImage update failed: {msg}");
            }
            *apply.lock() = Some(result);
        });
    }

    fn relaunch_after_update(&mut self) {
        if let Err(err) = crate::update::relaunch_self() {
            warn!("failed to relaunch updated AppImage: {err:#}");
            self.update_banner = UpdateBanner::Failed {
                message: format!("Updated, but relaunch failed: {err:#}"),
            };
        }
    }

    fn skip_this_update(&mut self) {
        if let Some(offer) = &self.last_update_offer {
            self.skipped_update = Some(offer.version.clone());
            self.save_ui_prefs();
        }
        self.update_later = true;
        self.update_banner = UpdateBanner::Hidden;
    }
}

impl eframe::App for ViewerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui_perf.tick_frame();
        self.ensure_maximized_on_launch(ctx);
        self.poll_config_reload();
        self.poll_anpr_alerts(ctx);
        if self.anpr_draft.should_run() {
            // Wake periodically so watchlist hits surface even when video is paused.
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        self.poll_update_check();
        if matches!(
            self.update_banner,
            UpdateBanner::Downloading { .. } | UpdateBanner::Ready { .. }
        ) || matches!(self.update_check_status, UpdateCheckStatus::Checking)
        {
            ctx.request_repaint();
        }
        self.sync_window_fullscreen(ctx);
        self.apply_accent_visuals(ctx);
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(
            crate::config::app_screen_title(&self.app_name, self.aux_open.then_some(1)),
        ));
        self.handle_keys(ctx);

        self.main_focused = ctx.input(|i| i.focused);
        if self.main_focused {
            self.last_ptz_screen_aux = false;
        }
        let focused = self.main_focused || self.aux_focused;
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
            egui::TopBottomPanel::top("toolbar")
                .frame(
                    egui::Frame::NONE
                        .fill(PANEL_BG)
                        .inner_margin(egui::Margin::symmetric(8, 4)),
                )
                .show(ctx, |ui| {
                    self.toolbar(ui, self.views.active, false);
                });

            if self.debug_overlay {
                egui::TopBottomPanel::bottom("statusbar")
                    .exact_height(26.0)
                    .frame(
                        egui::Frame::NONE
                            .fill(STATUS_BG)
                            .inner_margin(egui::Margin::symmetric(10, 4)),
                    )
                    .show(ctx, |ui| {
                        self.status_bar(ui);
                    });
            }

            if self.sidebar_open {
                self.show_resizable_camera_sidebar(ctx, self.views.active, false, "cameras_side");
            } else {
                egui::SidePanel::left("cameras_collapsed")
                    .exact_width(28.0)
                    .resizable(false)
                    .frame(
                        egui::Frame::NONE
                            .fill(PANEL_BG)
                            .inner_margin(egui::Margin::symmetric(4, 6)),
                    )
                    .show(ctx, |ui| {
                        if ui
                            .small_button(icons::CARET_RIGHT)
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

                self.paint_monitor(ui, full, self.views.active, self.fullscreen_slot, false);
            });

        self.draw_clear_slot_dialog(ctx);
        self.draw_gate_confirm_dialog(ctx);
        self.update_dialog(ctx);
        self.draw_anpr_alert(ctx);
        self.draw_debug_panel(ctx);
        self.draw_log_panel(ctx);
        self.settings_window(ctx);
        self.show_aux_window(ctx);
        self.handle_ptz_input(ctx);

        let main_down = ctx.input(|i| i.pointer.primary_down());
        if !main_down && !self.aux_pointer_down {
            self.cross_drag = None;
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
