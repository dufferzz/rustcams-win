#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod gst_env;
mod gst_link;
mod layout;
mod log_buffer;
mod nvr;
mod ptz;
mod stats;
mod stream;
mod views;

use app::ViewerApp;
use config::{default_app_name, AppConfig, ResolvedConfig};
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
            "RUSTCAMS_DEBUG set — perf overlay on; summaries every 5s. Tip: RUST_LOG=rustcams=debug GST_DEBUG=2"
        );
    }

    let config_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cameras.toml"));

    let (cfg, config_warning) = match AppConfig::load(&config_path) {
        Ok(cfg) => {
            let warning = if cfg.cameras.is_empty() {
                Some(format!(
                    "No cameras in {} — open Settings to configure.",
                    config_path.display()
                ))
            } else {
                None
            };
            (cfg, warning)
        }
        Err(err) => {
            tracing::warn!(
                "failed to load {}: {err:#} — starting without cameras",
                config_path.display()
            );
            (
                ResolvedConfig::default(),
                Some(format!(
                    "Config needs setup ({}): {err:#}\nOpen Settings to configure cameras.",
                    config_path.display()
                )),
            )
        }
    };

    let app_name = if cfg.viewer.app_name.trim().is_empty() {
        default_app_name()
    } else {
        cfg.viewer.app_name.clone()
    };

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1440.0, 900.0])
        .with_title(app_name);
    if let Some(icon) = load_app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "citadel-cctv",
        options,
        Box::new(move |_cc| match ViewerApp::new(cfg, config_path, config_warning, log_buffer) {
            Ok(app) => Ok(Box::new(app)),
            Err(err) => Err(err.into()),
        }),
    )
}
