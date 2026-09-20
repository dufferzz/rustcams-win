#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod gate;
mod http_client;
mod gst_env;
mod gst_link;
mod layout;
mod log_buffer;
mod nvr;
mod ptz;
mod redact;
mod stats;
mod stream;
mod views;

use app::ViewerApp;
use config::{app_screen_title, default_cameras_toml, AppConfig, LINUX_CONFIG_DIR};
use eframe::egui;
use log_buffer::LogBuffer;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

fn log_ansi_enabled() -> bool {
    // Plain text by default on Windows — cmd.exe / redirected consoles often
    // print raw ESC sequences (shown as "←[32m" etc). Opt in with RUSTCAMS_LOG_ANSI=1.
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if matches!(
        std::env::var("RUSTCAMS_LOG_ANSI").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE")
    ) {
        return true;
    }
    if cfg!(windows) {
        return false;
    }
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

fn load_app_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/icon.png");
    let image = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    Some(egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    })
}

fn main() -> eframe::Result<()> {
    let log_buffer = LogBuffer::new(2000);
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(log_ansi_enabled())
        .with_writer(log_buffer.make_writer())
        .init();

    if std::env::var_os("RUSTCAMS_DEBUG").is_some() {
        tracing::info!(
            "RUSTCAMS_DEBUG set — perf overlay on; summaries every 5s. Tip: RUST_LOG=rustcams=debug (avoid GST_DEBUG; it can log RTSP credentials)"
        );
    }

    let config_from_cli = std::env::args().nth(1).is_some();
    let config_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_cameras_toml);
    let config_path = config::absolute_path(config_path);
    match std::env::current_dir() {
        Ok(cwd) => tracing::info!(
            cwd = %cwd.display(),
            path = %config_path.display(),
            from_cli = config_from_cli,
            exists = config_path.is_file(),
            "config path"
        ),
        Err(err) => tracing::info!(
            path = %config_path.display(),
            from_cli = config_from_cli,
            exists = config_path.is_file(),
            "config path (cwd unavailable: {err})"
        ),
    }

    let (raw_cfg, cfg, config_warning) = AppConfig::read_for_app(&config_path);

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1440.0, 900.0])
        .with_maximized(true)
        .with_app_id(LINUX_CONFIG_DIR)
        .with_title(app_screen_title(&cfg.viewer.app_name, 1));
    if let Some(icon) = load_app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        persist_window: false,
        ..Default::default()
    };

    eframe::run_native(
        "citadel-cctv",
        options,
        Box::new(move |cc| {
            app::icons::install(&cc.egui_ctx);
            match ViewerApp::new(
                raw_cfg,
                cfg,
                config_path,
                config_from_cli,
                config_warning,
                log_buffer,
            ) {
                Ok(app) => Ok(Box::new(app)),
                Err(err) => Err(err.into()),
            }
        }),
    )
}
