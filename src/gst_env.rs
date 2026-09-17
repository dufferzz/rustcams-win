//! Bundled GStreamer discovery, PATH setup, and decoder ranking.

use gstreamer as gst;
use std::path::{Path, PathBuf};
use tracing::{debug, info};

/// If a portable GStreamer tree sits next to the executable, point the
/// process at it before `gst::init()`. No-op when the tree is missing so
/// system installs (Linux packages / Windows PATH) keep working.
pub(crate) fn configure_bundled_gstreamer() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(exe_dir) = exe.parent() else {
        return;
    };
    let root = exe_dir.join("gstreamer");
    let plugin_dir = root.join("lib").join("gstreamer-1.0");
    if !plugin_dir.is_dir() {
        return;
    }
    let bin_dir = root.join("bin");

    prepend_path_env("PATH", &bin_dir);
    // Prefer our plugins; leave registry discovery otherwise unchanged.
    set_path_env("GST_PLUGIN_PATH", &plugin_dir);
    set_path_env("GST_PLUGIN_SYSTEM_PATH", &plugin_dir);

    if let Some(scanner) = find_plugin_scanner(&bin_dir, &root) {
        set_path_env("GST_PLUGIN_SCANNER", &scanner);
    }

    info!(
        root = %root.display(),
        plugins = %plugin_dir.display(),
        "using bundled GStreamer next to executable"
    );
}

fn find_plugin_scanner(bin_dir: &Path, root: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let names = ["gst-plugin-scanner.exe"];
    #[cfg(not(windows))]
    let names = ["gst-plugin-scanner", "gst-plugin-scanner-1.0"];

    for name in names {
        let p = bin_dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    // Some layouts keep the scanner under libexec.
    for name in names {
        let p = root.join("libexec").join("gstreamer-1.0").join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn set_path_env(key: &str, path: &Path) {
    // SAFETY: called once before gst::init / other threads touch GStreamer.
    unsafe { std::env::set_var(key, path) };
}

fn prepend_path_env(key: &str, dir: &Path) {
    let dir_s = dir.display().to_string();
    let sep = if cfg!(windows) { ';' } else { ':' };
    let new_val = match std::env::var_os(key) {
        Some(existing) if !existing.is_empty() => {
            let mut s = dir_s;
            s.push(sep);
            s.push_str(&existing.to_string_lossy());
            s
        }
        _ => dir_s,
    };
    // SAFETY: called once before gst::init / other threads touch GStreamer.
    unsafe { std::env::set_var(key, new_val) };
}

pub(crate) fn prefer_hardware_decoders() {
    use gst::prelude::PluginFeatureExtManual;
    // Above PRIMARY (256) so decodebin prefers these over avdec_*.
    let hw_rank = gst::Rank::PRIMARY + 10;
    const HW_DECODERS: &[&str] = &[
        "d3d11h264dec",
        "d3d11h265dec",
        "mfh264dec",
        "mfh265dec",
        "nvh264dec",
        "nvh265dec",
        "vah264dec",
        "vah265dec",
    ];
    for name in HW_DECODERS {
        if let Some(factory) = gst::ElementFactory::find(name) {
            factory.set_rank(hw_rank);
            debug!(element = name, rank = ?hw_rank, "preferred hardware decoder");
        }
    }
    // Keep software as a fallback, but below DXVA / NVDEC / VAAPI.
    let sw_rank = gst::Rank::PRIMARY - 1;
    for name in ["avdec_h264", "avdec_h265"] {
        if let Some(factory) = gst::ElementFactory::find(name) {
            factory.set_rank(sw_rank);
            debug!(element = name, rank = ?sw_rank, "demoted software decoder");
        }
    }
}

pub(crate) fn log_decoder_availability() {
    use gst::prelude::PluginFeatureExtManual;
    const NAMES: &[&str] = &[
        "d3d11h264dec",
        "d3d11h265dec",
        "mfh264dec",
        "mfh265dec",
        "nvh264dec",
        "nvh265dec",
        "vah264dec",
        "vah265dec",
        "avdec_h264",
        "avdec_h265",
        "jpegdec",
        "d3d11convert",
        "d3d11scale",
        "d3d11download",
    ];
    let mut d3d11_dec = false;
    for name in NAMES {
        match gst::ElementFactory::find(name) {
            Some(f) => {
                debug!(element = name, rank = ?f.rank(), "decoder available");
                if *name == "d3d11h264dec" || *name == "d3d11h265dec" {
                    d3d11_dec = true;
                }
            }
            None => debug!(element = name, "decoder not found"),
        }
    }
    if d3d11_dec {
        info!("D3D11/DXVA hardware decoders present (Intel/AMD/NVIDIA via Direct3D11)");
    }
}

pub(crate) fn d3d11_postproc_available() -> bool {
    gst::ElementFactory::find("d3d11convert").is_some()
        && gst::ElementFactory::find("d3d11scale").is_some()
        && gst::ElementFactory::find("d3d11download").is_some()
}

