use crate::nvr::{self, DiscoveredChannel};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nvr: Option<NvrConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cameras: Vec<CameraEntry>,
    #[serde(default)]
    pub viewer: ViewerConfig,
}

/// Hikvision NVR used for ISAPI discovery and RTSP proxy streaming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NvrConfig {
    pub host: String,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default = "default_rtsp_port")]
    pub rtsp_port: u16,
    pub username: String,
    pub password: String,
    /// Default stream when a camera override does not set `stream`.
    #[serde(default)]
    pub stream: StreamType,
    /// Default RTSP transport for NVR streams: "udp", "tcp", or "udp+tcp".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocols: Option<String>,
}

impl Default for NvrConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            http_port: default_http_port(),
            rtsp_port: default_rtsp_port(),
            username: "admin".into(),
            password: String::new(),
            stream: StreamType::Sub,
            protocols: None,
        }
    }
}

fn default_http_port() -> u16 {
    80
}

fn default_rtsp_port() -> u16 {
    554
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StreamType {
    Main,
    #[default]
    Sub,
    Third,
}

impl StreamType {
    pub fn as_digit(self) -> u32 {
        match self {
            StreamType::Main => 1,
            StreamType::Sub => 2,
            StreamType::Third => 3,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            StreamType::Main => "main",
            StreamType::Sub => "sub",
            StreamType::Third => "third",
        }
    }
}

/// Raw TOML camera entry under `[[cameras]]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CameraEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Camera-direct RTSP (`rtsp://user:pass@host/...`) — creds + host + lens.
    #[serde(default)]
    pub url: String,
    /// Match discovered InputProxy channel by NVR channel id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u32>,
    /// Per-camera stream override (main / sub / third).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamType>,
    /// Optional RTSP transport: "udp", "tcp", or "udp+tcp"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocols: Option<String>,
    /// Enable ISAPI PTZ. If omitted, ids/names containing `"ptz"` are treated as PTZ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ptz: Option<bool>,
}

/// HTTP Digest target for Hikvision ISAPI PTZ (host includes port).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtzTarget {
    /// `host` or `host:port` for ISAPI HTTP.
    pub host: String,
    pub username: String,
    pub password: String,
    pub channel: u32,
    /// NVR `ContentMgmt/PTZCtrl` (InputProxy channel) instead of camera `PTZCtrl`.
    pub via_nvr: bool,
    /// If camera ISAPI fails, try this NVR route.
    pub fallback: Option<Box<PtzTarget>>,
}

/// Resolved camera ready for the viewer / GStreamer.
#[derive(Debug, Clone)]
pub struct CameraConfig {
    pub id: String,
    pub name: String,
    /// Grid / NVR (or direct) stream URL.
    pub url: String,
    /// Direct camera RTSP for fullscreen (main stream when possible).
    pub direct_url: Option<String>,
    pub protocols: Option<String>,
    /// NVR InputProxy channel id when discovered via `[nvr]`.
    #[allow(dead_code)]
    pub channel_id: Option<u32>,
    /// Camera ISAPI, or NVR `ContentMgmt/PTZCtrl` when `[nvr]` is set.
    pub ptz: Option<PtzTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewerConfig {
    /// Window title and toolbar brand.
    #[serde(default = "default_app_name")]
    pub app_name: String,
    #[serde(default = "default_layout")]
    pub default_layout: String,
    #[serde(default = "default_fit")]
    pub default_fit: String,
    #[serde(default = "default_false")]
    pub pause_when_unfocused: bool,
}

pub fn default_app_name() -> String {
    "Citadel CCTV".into()
}

/// XDG / AppImage config directory name (`~/.config/citadel-cctv`).
pub const LINUX_CONFIG_DIR: &str = "citadel-cctv";

fn appimage_dir() -> Option<PathBuf> {
    let p = std::env::var_os("APPIMAGE").filter(|v| !v.is_empty())?;
    PathBuf::from(p).parent().map(Path::to_path_buf)
}

fn xdg_config_cameras_toml() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|h| h.join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join(LINUX_CONFIG_DIR).join("cameras.toml")
}

/// Prefer a writable `cameras.toml`:
/// 1. Next to the AppImage file, else `~/.config/citadel-cctv/` (AppImage)
/// 2. Next to the executable if that file exists (Windows portable)
/// 3. `cameras.toml` in the working directory
pub fn default_cameras_toml() -> PathBuf {
    if let Some(dir) = appimage_dir() {
        let beside = dir.join("cameras.toml");
        if beside.is_file() {
            return beside;
        }
        return xdg_config_cameras_toml();
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let next_to_exe = dir.join("cameras.toml");
            if next_to_exe.is_file() {
                return next_to_exe;
            }
        }
    }
    PathBuf::from("cameras.toml")
}

fn default_layout() -> String {
    "3x3".into()
}

fn default_fit() -> String {
    "contain".into()
}

fn default_false() -> bool {
    false
}

impl Default for ViewerConfig {
    fn default() -> Self {
        Self {
            app_name: default_app_name(),
            default_layout: default_layout(),
            default_fit: default_fit(),
            pause_when_unfocused: false,
        }
    }
}

/// Fully resolved runtime config.
#[derive(Debug, Clone, Default)]
pub struct ResolvedConfig {
    pub cameras: Vec<CameraConfig>,
    pub viewer: ViewerConfig,
}

impl AppConfig {
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("failed to parse config {}", path.display()))
    }

    pub fn load(path: impl AsRef<Path>) -> Result<ResolvedConfig> {
        Self::read(path)?.resolve()
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let text = toml::to_string_pretty(self).context("serialize config")?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
        }
        fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    pub fn resolve(self) -> Result<ResolvedConfig> {
        let cameras = if let Some(nvr) = &self.nvr {
            if nvr.host.trim().is_empty() {
                bail!("[nvr] host is empty");
            }
            resolve_from_nvr(nvr, &self.cameras)?
        } else {
            resolve_direct(&self.cameras)?
        };

        let ptz_ready = cameras.iter().filter(|c| c.ptz.is_some()).count();
        info!(cameras = cameras.len(), ptz_ready, "resolved camera list");

        Ok(ResolvedConfig {
            cameras,
            viewer: self.viewer,
        })
    }
}

fn resolve_direct(entries: &[CameraEntry]) -> Result<Vec<CameraConfig>> {
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let url = e.url.trim();
        if url.is_empty() {
            bail!("camera '{}' needs a url when [nvr] is not configured", e.id);
        }
        let ptz = resolve_ptz_target(e, None, None);
        let direct_url = Some(nvr::rewrite_stream_digit(url, StreamType::Main));
        out.push(CameraConfig {
            id: e.id.clone(),
            name: e.name.clone().unwrap_or_else(|| e.id.clone()),
            url: url.to_string(),
            direct_url,
            protocols: e.protocols.clone(),
            channel_id: None,
            ptz,
        });
    }
    Ok(out)
}

fn resolve_from_nvr(nvr: &NvrConfig, overrides: &[CameraEntry]) -> Result<Vec<CameraConfig>> {
    info!(
        host = %nvr.host,
        http_port = nvr.http_port,
        "discovering cameras via NVR InputProxy"
    );
    let discovered = nvr::list_input_proxy_channels(nvr)
        .with_context(|| format!("failed to list cameras from NVR {}", nvr.host))?;
    info!(count = discovered.len(), "NVR reported InputProxy channels");

    let mut matched_channels: HashSet<u32> = HashSet::new();
    let mut cameras: Vec<CameraConfig> = Vec::new();

    for ovr in overrides {
        let disc = find_override_match(ovr, &discovered, &matched_channels);
        match disc {
            Some(disc) => {
                matched_channels.insert(disc.channel_id);
                cameras.push(camera_from_discovery(nvr, disc, ovr));
            }
            None => {
                warn!(
                    id = %ovr.id,
                    channel = ?ovr.channel,
                    url = %ovr.url,
                    "camera did not match any NVR channel"
                );
            }
        }
    }

    let mut used_ids: HashSet<String> = cameras.iter().map(|c| c.id.clone()).collect();
    for disc in &discovered {
        if matched_channels.contains(&disc.channel_id) {
            continue;
        }
        let base = slugify(&disc.name);
        let id = unique_id(&base, &used_ids);
        used_ids.insert(id.clone());
        let url = nvr::build_rtsp_url(nvr, disc.channel_id, nvr.stream);
        let ptz = name_wants_ptz(&id, Some(&disc.name)).then(|| nvr_ptz_target(nvr, disc.channel_id));
        cameras.push(CameraConfig {
            id,
            name: disc.name.clone(),
            url,
            direct_url: None,
            protocols: nvr.protocols.clone(),
            channel_id: Some(disc.channel_id),
            ptz,
        });
    }

    Ok(cameras)
}

fn find_override_match<'a>(
    ovr: &CameraEntry,
    discovered: &'a [DiscoveredChannel],
    matched: &HashSet<u32>,
) -> Option<&'a DiscoveredChannel> {
    if let Some(ch) = ovr.channel {
        return discovered.iter().find(|d| d.channel_id == ch);
    }
    if let Some((ip, lens)) = camera_url_identity(&ovr.url) {
        let candidates: Vec<_> = discovered
            .iter()
            .filter(|d| {
                !matched.contains(&d.channel_id)
                    && d.source_ip
                        .as_deref()
                        .is_some_and(|sip| sip.eq_ignore_ascii_case(&ip))
            })
            .collect();
        if let Some(d) = candidates
            .iter()
            .find(|d| d.src_input_port == Some(lens))
        {
            return Some(*d);
        }
        return candidates.first().copied();
    }
    let name_key = ovr.name.as_deref()?.to_ascii_lowercase();
    discovered.iter().find(|d| {
        !matched.contains(&d.channel_id) && d.name.to_ascii_lowercase() == name_key
    })
}

fn camera_from_discovery(
    nvr: &NvrConfig,
    disc: &DiscoveredChannel,
    ovr: &CameraEntry,
) -> CameraConfig {
    let stream = ovr.stream.unwrap_or(nvr.stream);
    let grid_url = nvr::build_rtsp_url(nvr, disc.channel_id, stream);
    let direct_url = {
        let u = ovr.url.trim();
        if u.is_empty() {
            None
        } else {
            Some(nvr::rewrite_stream_digit(u, StreamType::Main))
        }
    };
    let ptz = resolve_ptz_target(ovr, Some(nvr), Some(disc.channel_id));
    CameraConfig {
        id: ovr.id.clone(),
        name: ovr
            .name
            .clone()
            .unwrap_or_else(|| disc.name.clone()),
        url: grid_url,
        direct_url,
        protocols: ovr.protocols.clone().or_else(|| nvr.protocols.clone()),
        channel_id: Some(disc.channel_id),
        ptz,
    }
}

/// Host + Hikvision lens/channel from a camera RTSP URL.
fn camera_url_identity(raw: &str) -> Option<(String, u32)> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let u = Url::parse(raw).ok()?;
    let host = u.host_str()?.to_string();
    let lens = hikvision_stream_channel(u.path()).unwrap_or(1);
    Some((host, lens))
}

fn name_wants_ptz(id: &str, name: Option<&str>) -> bool {
    id.to_ascii_lowercase().contains("ptz")
        || name.is_some_and(|n| n.to_ascii_lowercase().contains("ptz"))
}

fn nvr_ptz_target(nvr: &NvrConfig, channel: u32) -> PtzTarget {
    PtzTarget {
        host: format!("{}:{}", nvr.host, nvr.http_port),
        username: nvr.username.clone(),
        password: nvr.password.clone(),
        channel,
        via_nvr: true,
        fallback: None,
    }
}

/// Camera-direct ISAPI when `url` is a camera; NVR ISAPI as fallback in `[nvr]` mode.
fn resolve_ptz_target(
    entry: &CameraEntry,
    nvr: Option<&NvrConfig>,
    nvr_channel: Option<u32>,
) -> Option<PtzTarget> {
    let wants_ptz = match entry.ptz {
        Some(flag) => flag,
        None => name_wants_ptz(&entry.id, entry.name.as_deref()),
    };
    if !wants_ptz {
        return None;
    }

    let nvr_target = match (nvr, nvr_channel) {
        (Some(nvr), Some(ch)) => Some(nvr_ptz_target(nvr, ch)),
        _ => None,
    };

    let cam_url = entry.url.trim();
    let camera_target = if cam_url.is_empty() {
        None
    } else {
        match target_from_rtsp_url(cam_url) {
            Ok(t) => {
                let points_at_nvr = nvr.is_some_and(|n| {
                    Url::parse(cam_url)
                        .ok()
                        .and_then(|u| u.host_str().map(str::to_string))
                        .is_some_and(|h| h.eq_ignore_ascii_case(&n.host))
                });
                if points_at_nvr {
                    None
                } else {
                    Some(t)
                }
            }
            Err(err) => {
                warn!(id = %entry.id, "invalid camera url for PTZ: {err:#}");
                None
            }
        }
    };

    match (camera_target, nvr_target) {
        (Some(mut cam), Some(nvr_t)) => {
            cam.fallback = Some(Box::new(nvr_t));
            Some(cam)
        }
        (Some(cam), None) => Some(cam),
        (None, Some(nvr_t)) => Some(nvr_t),
        (None, None) => None,
    }
}

/// Parse an RTSP (or HTTP) URL into an ISAPI host:port + channel.
pub fn target_from_rtsp_url(raw: &str) -> Result<PtzTarget> {
    let u = Url::parse(raw).context("parse PTZ URL")?;
    let host = u
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("PTZ URL missing host"))?;
    let http_host = isapi_http_host(host, u.port());
    let channel = hikvision_stream_channel(u.path())?;
    let username = percent_decode_userinfo(u.username());
    let password = u
        .password()
        .map(percent_decode_userinfo)
        .unwrap_or_default();
    Ok(PtzTarget {
        host: http_host,
        username,
        password,
        channel,
        via_nvr: false,
        fallback: None,
    })
}

fn percent_decode_userinfo(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

fn isapi_http_host(hostname: &str, rtsp_port: Option<u16>) -> String {
    let http_port = match rtsp_port {
        None | Some(554) => 80,
        Some(p) if p > 554 => p - 2,
        Some(p) => p,
    };
    format!("{hostname}:{http_port}")
}

fn hikvision_stream_channel(path: &str) -> Result<u32> {
    const MARKER: &str = "/Streaming/Channels/";
    let Some(rest) = path.split(MARKER).nth(1) else {
        bail!("no stream channel in path {path:?}");
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let stream_id: u32 = digits
        .parse()
        .with_context(|| format!("parse stream channel from {path:?}"))?;
    if stream_id < 100 {
        Ok(stream_id)
    } else {
        Ok(stream_id / 100)
    }
}

fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut prev_sep = true;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_sep = false;
        } else if !prev_sep {
            out.push('_');
            prev_sep = true;
        }
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() {
        "cam".into()
    } else {
        out
    }
}

fn unique_id(base: &str, used: &HashSet<String>) -> String {
    if !used.contains(base) {
        return base.to_string();
    }
    for n in 2..10_000 {
        let candidate = format!("{base}_{n}");
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    format!("{base}_{}", used.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Front PTZ"), "front_ptz");
        assert_eq!(slugify("  "), "cam");
    }

    #[test]
    fn parses_ptz_from_camera_rtsp() {
        let t = target_from_rtsp_url(
            "rtsp://admin:secret@192.0.2.10:554/Streaming/Channels/101",
        )
        .unwrap();
        assert_eq!(t.host, "192.0.2.10:80");
        assert_eq!(t.channel, 1);
        assert_eq!(t.username, "admin");
        assert_eq!(t.password, "secret");
    }

    #[test]
    fn parses_ptz_from_nvr_rtsp() {
        let t = target_from_rtsp_url(
            "rtsp://admin:pass@198.51.100.20:49002/Streaming/Channels/502",
        )
        .unwrap();
        assert_eq!(t.host, "198.51.100.20:49000");
        assert_eq!(t.channel, 5);
    }

    #[test]
    fn ptz_from_camera_url() {
        let entry = CameraEntry {
            id: "front_ptz".into(),
            name: Some("Front PTZ".into()),
            url: "rtsp://admin:secret@192.0.2.10:554/Streaming/Channels/101".into(),
            ..Default::default()
        };
        let t = resolve_ptz_target(&entry, None, None).unwrap();
        assert_eq!(
            t,
            PtzTarget {
                host: "192.0.2.10:80".into(),
                username: "admin".into(),
                password: "secret".into(),
                channel: 1,
                via_nvr: false,
                fallback: None,
            }
        );
    }

    #[test]
    fn ptz_nvr_mode_uses_camera_then_nvr_fallback() {
        let entry = CameraEntry {
            id: "front_ptz".into(),
            name: Some("Front PTZ".into()),
            url: "rtsp://admin:secret@192.0.2.10:554/Streaming/Channels/101".into(),
            ..Default::default()
        };
        let nvr = NvrConfig {
            host: "198.51.100.20".into(),
            http_port: 49000,
            username: "nvr".into(),
            password: "nvpass".into(),
            ..Default::default()
        };
        let t = resolve_ptz_target(&entry, Some(&nvr), Some(17)).unwrap();
        assert_eq!(t.host, "192.0.2.10:80");
        assert!(!t.via_nvr);
        let fb = t.fallback.expect("NVR fallback");
        assert_eq!(
            *fb,
            PtzTarget {
                host: "198.51.100.20:49000".into(),
                username: "nvr".into(),
                password: "nvpass".into(),
                channel: 17,
                via_nvr: true,
                fallback: None,
            }
        );
    }

    #[test]
    fn ptz_flag_enables_without_ptz_in_name() {
        let entry = CameraEntry {
            id: "water_tower".into(),
            name: Some("Water Tower".into()),
            url: "rtsp://admin:secret@192.0.2.10:554/Streaming/Channels/102".into(),
            ptz: Some(true),
            ..Default::default()
        };
        assert!(resolve_ptz_target(&entry, None, None).is_some());
    }

    #[test]
    fn ptz_flag_false_overrides_name_heuristic() {
        let entry = CameraEntry {
            id: "front_ptz".into(),
            name: Some("Front PTZ".into()),
            url: "rtsp://admin:secret@192.0.2.10:554/Streaming/Channels/102".into(),
            ptz: Some(false),
            ..Default::default()
        };
        assert!(resolve_ptz_target(&entry, None, None).is_none());
    }

    #[test]
    fn identity_from_url() {
        assert_eq!(
            camera_url_identity(
                "rtsp://admin:x@192.0.2.10:554/Streaming/Channels/201"
            ),
            Some(("192.0.2.10".into(), 2))
        );
    }

    #[test]
    fn empty_config_resolves() {
        let cfg = AppConfig::default();
        let resolved = cfg.resolve().unwrap();
        assert!(resolved.cameras.is_empty());
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn with_env(key: &str, value: Option<&str>, f: impl FnOnce()) {
        let _guard = env_lock();
        let prev = std::env::var_os(key);
        match value {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
        f();
        match prev {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    #[test]
    fn appimage_uses_toml_beside_image() {
        let root = std::env::temp_dir().join(format!(
            "rustcams-appimage-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let image = root.join("Citadel_CCTV-linux-x86_64.AppImage");
        fs::write(&image, []).unwrap();
        let toml_path = root.join("cameras.toml");
        fs::write(&toml_path, "[]\n").unwrap();
        with_env("APPIMAGE", Some(image.to_str().unwrap()), || {
            assert_eq!(default_cameras_toml(), toml_path);
        });
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn appimage_falls_back_to_xdg_config() {
        let root = std::env::temp_dir().join(format!(
            "rustcams-appimage-xdg-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let image = root.join("Citadel.AppImage");
        fs::write(&image, []).unwrap();
        let xdg = root.join("xdg-config");
        fs::create_dir_all(&xdg).unwrap();
        let _guard = env_lock();
        let prev_app = std::env::var_os("APPIMAGE");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("APPIMAGE", &image);
            std::env::set_var("XDG_CONFIG_HOME", &xdg);
        }
        let got = default_cameras_toml();
        match prev_app {
            Some(v) => unsafe { std::env::set_var("APPIMAGE", v) },
            None => unsafe { std::env::remove_var("APPIMAGE") },
        }
        match prev_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
        assert_eq!(got, xdg.join(LINUX_CONFIG_DIR).join("cameras.toml"));
        let _ = fs::remove_dir_all(&root);
    }
}
