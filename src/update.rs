//! GitHub AppImage update check (Linux AppImage builds only).

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::{info, warn};

/// Public GitHub repo that publishes AppImage releases.
pub const GITHUB_REPO: &str = "dufferzz/rustcams-win";

const METADATA_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone)]
pub struct AvailableUpdate {
    pub tag: String,
    pub version: String,
    pub appimage_url: String,
    pub sha256_url: String,
    pub asset_name: String,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u64, u64, u64);

/// Path of the running AppImage (`$APPIMAGE`), when that file exists.
///
/// Official runtimes usually set an absolute path. Extract-and-run / a relative
/// argv[0] can leave a relative `APPIMAGE`; resolve that against `$OWD` (the
/// directory the user launched from) before testing `is_file()`.
pub fn appimage_path() -> Option<PathBuf> {
    let raw = std::env::var_os("APPIMAGE").filter(|v| !v.is_empty())?;
    let path = PathBuf::from(&raw);
    let path = if path.is_absolute() {
        path
    } else if let Some(owd) = std::env::var_os("OWD").filter(|v| !v.is_empty()) {
        PathBuf::from(owd).join(path)
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let path = path.canonicalize().unwrap_or(path);
    path.is_file().then_some(path)
}

pub fn appimage_asset_name(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some("Citadel_CCTV-linux-x86_64.AppImage"),
        "aarch64" => Some("Citadel_CCTV-linux-aarch64.AppImage"),
        _ => None,
    }
}

pub fn parse_sha256sum(text: &str) -> Result<[u8; 32]> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or_else(|| anyhow!("empty sha256 file"))?;
    let hex = line
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("malformed sha256 line"))?;
    parse_hex32(hex).ok_or_else(|| anyhow!("sha256 is not 64 hex digits"))
}

pub fn normalize_tag(tag: &str) -> String {
    tag.trim().trim_start_matches(['v', 'V']).to_string()
}

pub fn version_is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
pub fn available_from_release(
    json: &str,
    current: &str,
    skipped: Option<&str>,
    arch: &str,
) -> Result<Option<AvailableUpdate>> {
    let release: GhRelease = serde_json::from_str(json).context("GitHub release JSON")?;
    offer_from_release(&release, current, skipped, arch)
}

#[derive(Debug, Clone)]
pub enum CheckOutcome {
    /// Running build is current (or no matching asset / skipped tag).
    UpToDate,
    Available(AvailableUpdate),
    Failed(String),
}

/// Query GitHub `/releases/latest`. Distinguishes up-to-date from network/API failure.
pub fn check_status(current: &str, skipped: Option<&str>) -> CheckOutcome {
    match check_latest_inner(current, skipped) {
        Ok(Some(offer)) => CheckOutcome::Available(offer),
        Ok(None) => CheckOutcome::UpToDate,
        Err(err) => {
            warn!("AppImage update check failed: {err:#}");
            CheckOutcome::Failed(format!("{err:#}"))
        }
    }
}

/// Spawn a new process of this AppImage (same CLI args), then exit.
/// Call after a successful in-place replace so the new binary is launched.
pub fn relaunch_self() -> Result<()> {
    let path = appimage_path().ok_or_else(|| anyhow!("not running from an AppImage"))?;
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    info!(path = %path.display(), "relaunching updated AppImage");
    std::process::Command::new(&path)
        .args(args)
        .spawn()
        .with_context(|| format!("spawn {}", path.display()))?;
    std::process::exit(0);
}

fn check_latest_inner(current: &str, skipped: Option<&str>) -> Result<Option<AvailableUpdate>> {
    let Some(asset) = appimage_asset_name(std::env::consts::ARCH) else {
        info!(
            arch = std::env::consts::ARCH,
            "no AppImage asset name for this architecture"
        );
        return Ok(None);
    };
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    let agent = metadata_agent()?;
    let mut resp = agent
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .context("GitHub releases request")?;
    let status = resp.status();
    if status.as_u16() == 404 {
        info!("no GitHub releases published yet");
        return Ok(None);
    }
    if status.as_u16() == 403 || status.as_u16() == 429 {
        bail!("GitHub update check rate limited (HTTP {})", status.as_u16());
    }
    if !status.is_success() {
        bail!("GitHub update check HTTP {}", status.as_u16());
    }
    let release: GhRelease = resp
        .body_mut()
        .read_json()
        .context("decode GitHub release JSON")?;
    let offer = offer_from_release(&release, current, skipped, std::env::consts::ARCH)?;
    if offer.is_none() {
        info!(
            latest = %release.tag_name,
            current,
            asset,
            "AppImage is up to date (or no matching asset)"
        );
    }
    Ok(offer)
}

fn offer_from_release(
    release: &GhRelease,
    current: &str,
    skipped: Option<&str>,
    arch: &str,
) -> Result<Option<AvailableUpdate>> {
    let version = normalize_tag(&release.tag_name);
    if let Some(skipped) = skipped.filter(|s| !s.trim().is_empty()) {
        if !version_is_newer(&version, skipped) {
            return Ok(None);
        }
    }
    if !version_is_newer(&version, current) {
        return Ok(None);
    }
    let Some(asset_name) = appimage_asset_name(arch) else {
        return Ok(None);
    };
    let sha_name = format!("{asset_name}.sha256");
    let appimage_url = asset_url(&release.assets, asset_name);
    let sha256_url = asset_url(&release.assets, &sha_name);
    let (Some(appimage_url), Some(sha256_url)) = (appimage_url, sha256_url) else {
        warn!(
            tag = %release.tag_name,
            asset_name,
            "GitHub release is missing AppImage or .sha256 asset"
        );
        return Ok(None);
    };
    Ok(Some(AvailableUpdate {
        tag: release.tag_name.clone(),
        version,
        appimage_url,
        sha256_url,
        asset_name: asset_name.to_string(),
    }))
}

fn asset_url(assets: &[GhAsset], name: &str) -> Option<String> {
    assets
        .iter()
        .find(|a| a.name == name)
        .map(|a| a.browser_download_url.clone())
}

pub fn download_and_replace(
    dest: &Path,
    update: &AvailableUpdate,
    progress: &AtomicU64,
) -> Result<()> {
    progress.store(0, Ordering::Relaxed);
    let tmp = dest.with_file_name(format!(
        "{}.new",
        dest.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Citadel_CCTV.AppImage")
    ));
    let _ = fs::remove_file(&tmp);

    let expected = {
        let agent = download_agent()?;
        let mut resp = agent
            .get(&update.sha256_url)
            .header("Accept", "application/octet-stream")
            .call()
            .context("download sha256")?;
        if !resp.status().is_success() {
            bail!("sha256 download HTTP {}", resp.status());
        }
        let text = resp.body_mut().read_to_string().context("read sha256")?;
        parse_sha256sum(&text)?
    };

    let digest = download_to_file(&update.appimage_url, &tmp, progress)
        .with_context(|| format!("download {}", update.asset_name));
    let digest = match digest {
        Ok(d) => d,
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
    };
    if digest != expected {
        let _ = fs::remove_file(&tmp);
        bail!("SHA-256 mismatch for {}", update.asset_name);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tmp)
            .with_context(|| format!("stat {}", tmp.display()))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tmp, perms).context("chmod new AppImage")?;
    }

    fs::rename(&tmp, dest).with_context(|| {
        format!(
            "replace {} (is the AppImage directory writable?)",
            dest.display()
        )
    })?;
    info!(
        path = %dest.display(),
        version = %update.version,
        "AppImage updated"
    );
    Ok(())
}

fn download_to_file(url: &str, dest: &Path, progress: &AtomicU64) -> Result<[u8; 32]> {
    let agent = download_agent()?;
    let mut resp = agent
        .get(url)
        .header("Accept", "application/octet-stream")
        .call()
        .context("AppImage download")?;
    if !resp.status().is_success() {
        bail!("AppImage download HTTP {}", resp.status());
    }
    let mut file = File::create(dest).with_context(|| {
        format!(
            "create {} (is the AppImage directory writable?)",
            dest.display()
        )
    })?;
    let mut hasher = Sha256::new();
    let mut reader = resp.body_mut().as_reader();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).context("read AppImage body")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).context("write AppImage")?;
        hasher.update(&buf[..n]);
        progress.fetch_add(n as u64, Ordering::Relaxed);
    }
    file.sync_all().context("fsync AppImage")?;
    drop(file);
    let hash = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&hash);
    Ok(out)
}

fn user_agent() -> String {
    format!(
        "Citadel-CCTV/{} (+https://github.com/{GITHUB_REPO})",
        env!("CARGO_PKG_VERSION")
    )
}

fn metadata_agent() -> Result<ureq::Agent> {
    Ok(ureq::Agent::config_builder()
        .timeout_global(Some(METADATA_TIMEOUT))
        .http_status_as_error(false)
        .max_redirects(10)
        .user_agent(&user_agent())
        .build()
        .into())
}

fn download_agent() -> Result<ureq::Agent> {
    Ok(ureq::Agent::config_builder()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .http_status_as_error(false)
        .max_redirects(10)
        .user_agent(&user_agent())
        .build()
        .into())
}

fn parse_version(s: &str) -> Option<Version> {
    let s = normalize_tag(s);
    let core = s.split(['-', '+']).next().unwrap_or(&s);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some(Version(major, minor, patch))
}

fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
        let hex = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(hex, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "tag_name": "v0.3.0",
        "assets": [
            {
                "name": "Citadel_CCTV-linux-x86_64.AppImage",
                "browser_download_url": "https://example.com/app"
            },
            {
                "name": "Citadel_CCTV-linux-x86_64.AppImage.sha256",
                "browser_download_url": "https://example.com/sha"
            },
            {
                "name": "Citadel_CCTV-linux-aarch64.AppImage",
                "browser_download_url": "https://example.com/app-arm"
            },
            {
                "name": "Citadel_CCTV-linux-aarch64.AppImage.sha256",
                "browser_download_url": "https://example.com/sha-arm"
            }
        ]
    }"#;

    #[test]
    fn normalize_strips_v_prefix() {
        assert_eq!(normalize_tag("v0.2.1"), "0.2.1");
        assert_eq!(normalize_tag("0.2.1"), "0.2.1");
    }

    #[test]
    fn newer_version_compares_semver_core() {
        assert!(version_is_newer("0.3.0", "0.2.1"));
        assert!(version_is_newer("v0.2.2", "0.2.1"));
        assert!(!version_is_newer("0.2.1", "0.2.1"));
        assert!(!version_is_newer("0.2.0", "0.2.1"));
        assert!(version_is_newer("1.0.0", "0.9.9"));
    }

    #[test]
    fn asset_names_match_packaging_script() {
        assert_eq!(
            appimage_asset_name("x86_64"),
            Some("Citadel_CCTV-linux-x86_64.AppImage")
        );
        assert_eq!(
            appimage_asset_name("aarch64"),
            Some("Citadel_CCTV-linux-aarch64.AppImage")
        );
        assert_eq!(appimage_asset_name("arm"), None);
    }

    #[test]
    fn sha256sum_parses_gnu_line() {
        let text = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  Citadel_CCTV-linux-x86_64.AppImage\n";
        let got = parse_sha256sum(text).unwrap();
        assert_eq!(got[0], 0x01, "first byte");
        assert_eq!(got[31], 0xef);
    }

    #[test]
    fn offer_when_newer_and_assets_present() {
        let offer = available_from_release(SAMPLE, "0.2.1", None, "x86_64")
            .unwrap()
            .expect("offer");
        assert_eq!(offer.version, "0.3.0");
        assert_eq!(offer.appimage_url, "https://example.com/app");
        assert_eq!(offer.sha256_url, "https://example.com/sha");
        assert_eq!(offer.asset_name, "Citadel_CCTV-linux-x86_64.AppImage");
    }

    #[test]
    fn no_offer_when_current_is_latest() {
        assert!(available_from_release(SAMPLE, "0.3.0", None, "x86_64")
            .unwrap()
            .is_none());
    }

    #[test]
    fn no_offer_when_skipped_until_newer_tag() {
        assert!(
            available_from_release(SAMPLE, "0.2.1", Some("0.3.0"), "x86_64")
                .unwrap()
                .is_none()
        );
        assert!(
            available_from_release(SAMPLE, "0.2.1", Some("v0.3.0"), "x86_64")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn skipped_older_tag_still_offers() {
        assert!(
            available_from_release(SAMPLE, "0.2.1", Some("0.2.5"), "x86_64")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn missing_sha_asset_is_not_an_offer() {
        let json = r#"{
            "tag_name": "v0.4.0",
            "assets": [
                {
                    "name": "Citadel_CCTV-linux-x86_64.AppImage",
                    "browser_download_url": "https://example.com/app"
                }
            ]
        }"#;
        assert!(available_from_release(json, "0.2.1", None, "x86_64")
            .unwrap()
            .is_none());
    }

    #[test]
    fn aarch64_picks_arm_assets() {
        let offer = available_from_release(SAMPLE, "0.2.1", None, "aarch64")
            .unwrap()
            .unwrap();
        assert_eq!(offer.appimage_url, "https://example.com/app-arm");
    }
}
