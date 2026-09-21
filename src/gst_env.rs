//! Bundled GStreamer discovery, PATH setup, and decoder ranking.

use gstreamer as gst;
use std::path::{Path, PathBuf};
use tracing::{debug, info};

fn running_from_appimage() -> bool {
    std::env::var_os("APPIMAGE")
        .or_else(|| std::env::var_os("APPDIR"))
        .is_some_and(|v| !v.is_empty())
}

/// Portable GStreamer layouts:
/// - Windows ZIP: `exe_dir/gstreamer/lib/gstreamer-1.0`
/// - linuxdeploy AppImage: `$APPDIR/usr/lib/gstreamer-1.0` (or `exe_dir/../lib/gstreamer-1.0`)
///
/// No-op when neither tree exists so a system GStreamer install still works.
pub(crate) fn configure_bundled_gstreamer() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(exe_dir) = exe.parent() else {
        return;
    };
    let Some((root, plugin_dir)) = discover_bundled_gstreamer(exe_dir) else {
        return;
    };
    let bin_dir = bundled_bin_dir(&root, exe_dir);

    prepend_path_env("PATH", &bin_dir);
    set_path_env("GST_PLUGIN_PATH", &plugin_dir);
    set_path_env("GST_PLUGIN_PATH_1_0", &plugin_dir);
    set_path_env("GST_PLUGIN_SYSTEM_PATH", &plugin_dir);
    set_path_env("GST_PLUGIN_SYSTEM_PATH_1_0", &plugin_dir);
    // SAFETY: before gst::init / other threads.
    unsafe { std::env::set_var("GST_REGISTRY_REUSE_PLUGIN_SCANNER", "no") };

    if let Some(scanner) = find_plugin_scanner(&bin_dir, &root, &plugin_dir) {
        set_path_env("GST_PLUGIN_SCANNER", &scanner);
        set_path_env("GST_PLUGIN_SCANNER_1_0", &scanner);
    }

    info!(
        root = %root.display(),
        plugins = %plugin_dir.display(),
        "using bundled GStreamer next to executable"
    );
}

fn discover_bundled_gstreamer(exe_dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut roots = vec![exe_dir.join("gstreamer")];
    if running_from_appimage() {
        if let Some(appdir) = std::env::var_os("APPDIR").filter(|v| !v.is_empty()) {
            roots.push(PathBuf::from(appdir));
        }
        roots.push(exe_dir.join(".."));
    }
    for root in roots {
        for rel in ["lib/gstreamer-1.0", "usr/lib/gstreamer-1.0"] {
            let plugin_dir = root.join(rel);
            if plugin_dir.is_dir() {
                return Some((root, plugin_dir));
            }
        }
    }
    None
}

fn bundled_bin_dir(root: &Path, exe_dir: &Path) -> PathBuf {
    for rel in ["usr/bin", "bin"] {
        let p = root.join(rel);
        if p.is_dir() {
            return p;
        }
    }
    exe_dir.to_path_buf()
}

fn find_plugin_scanner(bin_dir: &Path, root: &Path, plugin_dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let names = ["gst-plugin-scanner.exe"];
    #[cfg(not(windows))]
    let names = ["gst-plugin-scanner", "gst-plugin-scanner-1.0"];

    let mut dirs = vec![
        bin_dir.to_path_buf(),
        root.join("libexec").join("gstreamer-1.0"),
        root.join("usr").join("libexec").join("gstreamer-1.0"),
        root.join("lib").join("gstreamer1.0").join("gstreamer-1.0"),
        root.join("usr")
            .join("lib")
            .join("gstreamer1.0")
            .join("gstreamer-1.0"),
        plugin_dir.to_path_buf(),
    ];
    if let Some(appdir) = std::env::var_os("APPDIR").filter(|v| !v.is_empty()) {
        let appdir = PathBuf::from(appdir);
        dirs.push(
            appdir
                .join("usr")
                .join("lib")
                .join("gstreamer1.0")
                .join("gstreamer-1.0"),
        );
    }

    for dir in dirs {
        for name in names {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
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
        "v4l2slh264dec",
        "v4l2slh265dec",
        "v4l2h264dec",
        "v4l2h265dec",
    ];
    for name in HW_DECODERS {
        if let Some(factory) = gst::ElementFactory::find(name) {
            factory.set_rank(hw_rank);
            debug!(element = name, rank = ?hw_rank, "preferred hardware decoder");
        }
    }
    // Keep software as a fallback, but below DXVA / NVDEC / VAAPI / V4L2.
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
        "v4l2slh264dec",
        "v4l2slh265dec",
        "v4l2h264dec",
        "v4l2h265dec",
        "avdec_h264",
        "avdec_h265",
        "jpegdec",
        "d3d11convert",
        "d3d11scale",
        "d3d11download",
        "nvcudah264dec",
        "nvcudah265dec",
        "cudaconvert",
        "cudascale",
        "cudadownload",
    ];
    let mut d3d11_dec = false;
    let mut nv_dec = false;
    let mut v4l2_dec = false;
    for name in NAMES {
        match gst::ElementFactory::find(name) {
            Some(f) => {
                debug!(element = name, rank = ?f.rank(), "decoder available");
                if *name == "d3d11h264dec" || *name == "d3d11h265dec" {
                    d3d11_dec = true;
                }
                if *name == "nvh264dec" || *name == "nvh265dec" {
                    nv_dec = true;
                }
                if name.starts_with("v4l2") {
                    v4l2_dec = true;
                }
            }
            None => debug!(element = name, "decoder not found"),
        }
    }
    if d3d11_dec {
        info!("D3D11/DXVA hardware decoders present (Intel/AMD/NVIDIA via Direct3D11)");
    }
    if nv_dec {
        info!("NVIDIA NVDEC decoders present (nvh264dec / nvh265dec)");
    }
    if v4l2_dec {
        info!("V4L2 hardware decoders present (Raspberry Pi / Linux stateless or stateful)");
    }
}

pub(crate) fn nvdec_available() -> bool {
    gst::ElementFactory::find("nvh264dec").is_some()
        || gst::ElementFactory::find("nvcudah264dec").is_some()
}

pub(crate) fn cuda_postproc_available() -> bool {
    gst::ElementFactory::find("cudadownload").is_some()
}

pub(crate) fn d3d11_postproc_available() -> bool {
    gst::ElementFactory::find("d3d11convert").is_some()
        && gst::ElementFactory::find("d3d11scale").is_some()
        && gst::ElementFactory::find("d3d11download").is_some()
}
