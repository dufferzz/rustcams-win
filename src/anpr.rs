//! Hikvision ANPR alert stream — digest `GET …/Event/notification/alertStream`.

use crate::config::{AnprConfig, ANPR_SOUND_ALERT, ANPR_SOUND_KIM};
use anyhow::{bail, Context, Result};
use digest_auth::{AuthContext, HttpMethod};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const ALERT_BYTES: &[u8] = include_bytes!("../assets/alert.mp3");
const KIM_BYTES: &[u8] = include_bytes!("../assets/kim.mp3");

#[derive(Debug, Clone)]
pub struct AnprAlert {
    pub person: String,
    pub plate: String,
    /// JPEG bytes when a snapshot was fetched.
    pub image_jpeg: Option<Vec<u8>>,
    pub at: Instant,
}

struct Shared {
    cfg: Mutex<AnprConfig>,
    /// Bumped whenever the worker should drop the current stream and re-read config.
    gen: AtomicU64,
    stop: AtomicBool,
    silent: AtomicBool,
    /// Set when a new alert is queued so the UI can request a repaint.
    pending_ui: AtomicBool,
    status: Mutex<Option<String>>,
    alerts: Mutex<VecDeque<AnprAlert>>,
    /// Directory containing `cameras.toml` (for relative sound paths).
    config_dir: Mutex<PathBuf>,
}

pub struct AnprWorker {
    shared: Arc<Shared>,
    _handle: JoinHandle<()>,
}

impl AnprWorker {
    pub fn start(cfg: AnprConfig, config_dir: PathBuf) -> Self {
        let shared = Arc::new(Shared {
            silent: AtomicBool::new(cfg.silent),
            cfg: Mutex::new(cfg),
            gen: AtomicU64::new(1),
            stop: AtomicBool::new(false),
            pending_ui: AtomicBool::new(false),
            status: Mutex::new(None),
            alerts: Mutex::new(VecDeque::new()),
            config_dir: Mutex::new(config_dir),
        });
        let thread_shared = shared.clone();
        let handle = thread::Builder::new()
            .name("anpr-stream".into())
            .spawn(move || worker_loop(thread_shared))
            .expect("spawn anpr thread");
        Self {
            shared,
            _handle: handle,
        }
    }

    pub fn apply_config(&self, cfg: AnprConfig, config_dir: PathBuf) {
        self.shared.silent.store(cfg.silent, Ordering::SeqCst);
        *self.shared.config_dir.lock() = config_dir;
        *self.shared.cfg.lock() = cfg;
        self.shared.gen.fetch_add(1, Ordering::SeqCst);
    }

    /// Replace the live watchlist without dropping the alertStream connection.
    pub fn update_watchlist(&self, cfg: AnprConfig) {
        let n = cfg.plates.len();
        let host = cfg.host.trim().to_string();
        self.shared.silent.store(cfg.silent, Ordering::SeqCst);
        *self.shared.cfg.lock() = cfg;
        let mut status = self.shared.status.lock();
        if status
            .as_deref()
            .is_some_and(|s| s.starts_with("Listening on"))
        {
            *status = Some(format!("Listening on {host} ({n} plate(s))"));
        }
    }

    pub fn set_silent(&self, silent: bool) {
        self.shared.silent.store(silent, Ordering::SeqCst);
        self.shared.cfg.lock().silent = silent;
    }

    pub fn silent(&self) -> bool {
        self.shared.silent.load(Ordering::SeqCst)
    }

    pub fn status(&self) -> Option<String> {
        self.shared.status.lock().clone()
    }

    pub fn take_alerts(&self) -> Vec<AnprAlert> {
        self.shared.pending_ui.store(false, Ordering::SeqCst);
        let mut q = self.shared.alerts.lock();
        q.drain(..).collect()
    }

    pub fn needs_repaint(&self) -> bool {
        self.shared.pending_ui.load(Ordering::SeqCst)
    }
}

impl Drop for AnprWorker {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.gen.fetch_add(1, Ordering::SeqCst);
    }
}

fn set_status(shared: &Shared, msg: impl Into<String>) {
    *shared.status.lock() = Some(msg.into());
}

fn worker_loop(shared: Arc<Shared>) {
    let mut last_gen = 0u64;
    while !shared.stop.load(Ordering::SeqCst) {
        let gen = shared.gen.load(Ordering::SeqCst);
        let cfg = shared.cfg.lock().clone();
        if !cfg.should_run() {
            if gen != last_gen {
                set_status(&shared, "ANPR idle — enable and set host in Settings.");
                last_gen = gen;
            }
            thread::sleep(Duration::from_secs(2));
            continue;
        }

        if gen != last_gen {
            set_status(
                &shared,
                format!("Connecting to ANPR at {}…", cfg.host.trim()),
            );
            last_gen = gen;
        }

        match run_stream_session(&shared, &cfg, gen) {
            Ok(()) => {
                if shared.stop.load(Ordering::SeqCst) {
                    break;
                }
                if shared.gen.load(Ordering::SeqCst) != gen {
                    continue;
                }
                set_status(&shared, "ANPR stream ended — reconnecting…");
                thread::sleep(Duration::from_secs(5));
            }
            Err(err) => {
                if shared.stop.load(Ordering::SeqCst) {
                    break;
                }
                warn!(error = %err, "ANPR stream error");
                set_status(&shared, format!("ANPR error: {err:#}"));
                thread::sleep(Duration::from_secs(10));
            }
        }
    }
    set_status(&shared, "ANPR stopped.");
}

fn run_stream_session(shared: &Shared, cfg: &AnprConfig, gen: u64) -> Result<()> {
    crate::config::validate_host(&cfg.host)?;
    let path = AnprConfig::alert_stream_path();
    let url = cfg.alert_stream_url();
    debug!(%url, host = %cfg.host.trim(), "ANPR alertStream connect");

    let agent = crate::http_client::agent(Duration::from_secs(20));

    let challenge = agent
        .get(&url)
        .header(
            "Accept",
            "multipart/x-mixed-replace, multipart/mixed, application/xml",
        )
        .call()
        .with_context(|| format!("ANPR probe GET {url}"))?;

    let status = challenge.status();
    debug!(%status, "ANPR alertStream probe");
    let www = if status == 401 {
        challenge
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow::anyhow!("ANPR 401 missing WWW-Authenticate"))?
            .to_string()
    } else if (200..300).contains(&status.as_u16()) {
        // Some firmwares accept without digest on a second try only; rare.
        drop(challenge);
        return stream_body_unauth(shared, cfg, gen, &url);
    } else {
        bail!("ANPR digest challenge expected HTTP 401 from {url}, got {status}");
    };
    drop(challenge);

    let mut prompt = digest_auth::parse(&www).context("parse WWW-Authenticate")?;
    let context = AuthContext::new_with_method(
        &cfg.username,
        &cfg.password,
        path,
        Option::<&[u8]>::None,
        HttpMethod::GET,
    );
    let answer = prompt
        .respond(&context)
        .context("compute digest Authorization")?
        .to_string();

    // Long-lived read: no global timeout on the streaming agent.
    let stream_agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(None)
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();

    let response = stream_agent
        .get(&url)
        .header("Authorization", &answer)
        .header(
            "Accept",
            "multipart/x-mixed-replace, multipart/mixed, application/xml",
        )
        .header("Connection", "keep-alive")
        .call()
        .with_context(|| format!("ANPR authenticated GET {url}"))?;

    let status = response.status();
    if !(200..300).contains(&status.as_u16()) {
        bail!("ANPR alertStream failed: HTTP {status}");
    }

    set_status(
        shared,
        format!(
            "Listening on {} ({} plate(s))",
            cfg.host.trim(),
            cfg.plates.len()
        ),
    );
    info!(host = %cfg.host.trim(), plates = cfg.plates.len(), "ANPR alertStream connected");

    let (_, body) = response.into_parts();
    let reader = BufReader::with_capacity(64 * 1024, body.into_reader());
    process_raw_stream(shared, cfg, gen, reader)
}

fn stream_body_unauth(shared: &Shared, cfg: &AnprConfig, gen: u64, url: &str) -> Result<()> {
    let stream_agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(None)
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();
    let response = stream_agent
        .get(url)
        .header(
            "Accept",
            "multipart/x-mixed-replace, multipart/mixed, application/xml",
        )
        .header("Connection", "keep-alive")
        .call()
        .with_context(|| format!("ANPR open GET {url}"))?;
    if !(200..300).contains(&response.status().as_u16()) {
        bail!("ANPR alertStream failed: HTTP {}", response.status());
    }
    set_status(
        shared,
        format!(
            "Listening on {} ({} plate(s))",
            cfg.host.trim(),
            cfg.plates.len()
        ),
    );
    let (_, body) = response.into_parts();
    let reader = BufReader::with_capacity(64 * 1024, body.into_reader());
    process_raw_stream(shared, cfg, gen, reader)
}

fn process_raw_stream<R: Read>(
    shared: &Shared,
    cfg: &AnprConfig,
    gen: u64,
    reader: BufReader<R>,
) -> Result<()> {
    let mut reader = reader;
    let mut buffer = String::new();
    let mut collecting = false;
    let mut line = String::new();

    loop {
        if shared.stop.load(Ordering::SeqCst) || shared.gen.load(Ordering::SeqCst) != gen {
            return Ok(());
        }

        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(err) => return Err(err).context("read ANPR stream"),
        }

        let trimmed = line.trim();
        if trimmed.starts_with("<EventNotificationAlert") {
            buffer.clear();
            buffer.push_str(&line);
            collecting = true;
        } else if collecting {
            buffer.push_str(&line);
        }

        if collecting && trimmed.contains("</EventNotificationAlert>") {
            handle_event_xml(shared, cfg, &buffer);
            buffer.clear();
            collecting = false;
        }
    }
}

fn handle_event_xml(shared: &Shared, _cfg: &AnprConfig, xml: &str) {
    let Some((event_type, plate, _confidence)) = parse_anpr_event(xml) else {
        return;
    };
    if !event_type.eq_ignore_ascii_case("anpr") {
        return;
    }
    let plate_key = AnprConfig::normalize_plate(&plate);
    if plate_key.is_empty() {
        return;
    }

    // Re-read live config so tray/settings toggles apply without reconnect.
    let live = shared.cfg.lock().clone();
    let Some(entry) = live.find_plate(&plate_key).cloned() else {
        debug!(plate = %plate_key, "ANPR plate not on watchlist");
        return;
    };
    if !entry.enabled {
        info!(
            plate = %plate_key,
            name = %entry.display_name(),
            "ANPR watchlist hit ignored (disabled)"
        );
        return;
    }

    info!(
        plate = %plate_key,
        name = %entry.display_name(),
        "ANPR watchlist hit"
    );

    let image_jpeg = match download_snapshot(&live) {
        Ok(bytes) => {
            debug!(plate = %plate_key, bytes = bytes.len(), "ANPR snapshot fetched");
            Some(bytes)
        }
        Err(err) => {
            warn!(plate = %plate_key, error = %err, "ANPR snapshot failed");
            None
        }
    };
    {
        let mut q = shared.alerts.lock();
        q.push_back(AnprAlert {
            person: entry.display_name().to_string(),
            plate: plate_key.clone(),
            image_jpeg,
            at: Instant::now(),
        });
        while q.len() > 8 {
            q.pop_front();
        }
    }
    shared.pending_ui.store(true, Ordering::SeqCst);

    // Sound after the UI alert is queued so the popup appears with the audio.
    if !shared.silent.load(Ordering::SeqCst) {
        let sound = entry.sound_key().to_string();
        let config_dir = shared.config_dir.lock().clone();
        thread::spawn(move || {
            if let Err(err) = play_alert_sound(&sound, &config_dir) {
                warn!(error = %err, sound = %sound, "ANPR alert sound failed");
            }
        });
    }
}

fn parse_anpr_event(xml: &str) -> Option<(String, String, Option<i32>)> {
    // Lightweight tag scrape — Hikvision XML namespaces vary by firmware.
    let event_type = xml_tag_text(xml, "eventType")?;
    let plate = xml_tag_text(xml, "licensePlate").unwrap_or_default();
    let confidence = xml_tag_text(xml, "confidenceLevel").and_then(|s| s.parse::<i32>().ok());
    Some((event_type, plate, confidence))
}

fn xml_tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let start = xml.find(&open)?;
    let after_lt = &xml[start..];
    let gt = after_lt.find('>')?;
    let content_start = start + gt + 1;
    let end = xml[content_start..].find(&close)? + content_start;
    Some(xml[content_start..end].trim().to_string())
}

pub fn fetch_snapshot(cfg: &AnprConfig) -> Result<Vec<u8>> {
    download_snapshot(cfg)
}

fn download_snapshot(cfg: &AnprConfig) -> Result<Vec<u8>> {
    let path = cfg.snapshot_path();
    let url = cfg.snapshot_url();
    debug!(%url, "ANPR snapshot GET");
    let agent = crate::http_client::agent(Duration::from_secs(8));

    let challenge = agent
        .get(&url)
        .call()
        .with_context(|| format!("ANPR snapshot probe {url}"))?;
    let status = challenge.status();
    debug!(%status, "ANPR snapshot probe");

    let mut body = if (200..300).contains(&status.as_u16()) {
        let (_, body) = challenge.into_parts();
        body
    } else if status == 401 {
        let www = challenge
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow::anyhow!("snapshot 401 missing WWW-Authenticate"))?
            .to_string();
        drop(challenge);
        let mut prompt = digest_auth::parse(&www)?;
        let context = AuthContext::new_with_method(
            &cfg.username,
            &cfg.password,
            &path,
            Option::<&[u8]>::None,
            HttpMethod::GET,
        );
        let answer = prompt.respond(&context)?.to_string();
        let response = agent
            .get(&url)
            .header("Authorization", &answer)
            .call()
            .with_context(|| format!("ANPR snapshot GET {url}"))?;
        let status = response.status();
        if !(200..300).contains(&status.as_u16()) {
            warn!(%url, %status, "ANPR snapshot failed");
            bail!("snapshot HTTP {status}");
        }
        debug!(%status, "ANPR snapshot authenticated");
        let (_, body) = response.into_parts();
        body
    } else {
        warn!(%url, %status, "ANPR snapshot unexpected status");
        bail!("snapshot HTTP {status}");
    };

    let mut bytes = body.read_to_vec().context("read snapshot body")?;
    if let Some(i) = find_jpeg_soi(&bytes) {
        bytes = bytes[i..].to_vec();
    }
    if bytes.len() < 100 {
        bail!("snapshot too small");
    }
    Ok(bytes)
}

fn find_jpeg_soi(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|w| w == [0xff, 0xd8])
}

pub fn play_sound(sound: &str, config_dir: &Path) -> Result<()> {
    play_alert_sound(sound, config_dir)
}

fn play_alert_sound(sound: &str, config_dir: &Path) -> Result<()> {
    let bytes = match sound.trim().to_ascii_lowercase().as_str() {
        "" | ANPR_SOUND_ALERT => ALERT_BYTES,
        ANPR_SOUND_KIM => KIM_BYTES,
        other => {
            let path = resolve_sound_path(other, config_dir);
            let data =
                std::fs::read(&path).with_context(|| format!("read sound {}", path.display()))?;
            return play_mp3_bytes(&data);
        }
    };
    play_mp3_bytes(bytes)
}

fn resolve_sound_path(sound: &str, config_dir: &Path) -> PathBuf {
    let p = Path::new(sound);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        config_dir.join(p)
    }
}

fn play_mp3_bytes(data: &[u8]) -> Result<()> {
    use rodio::{Decoder, OutputStream, Sink};
    use std::io::Cursor;

    let (_stream, handle) = OutputStream::try_default().context("open audio output")?;
    let sink = Sink::try_new(&handle).context("create audio sink")?;
    let source = Decoder::new(Cursor::new(data.to_vec())).context("decode mp3")?;
    sink.append(source);
    sink.sleep_until_end();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_anpr_xml() {
        let xml = r#"<?xml version="1.0"?>
<EventNotificationAlert>
  <eventType>ANPR</eventType>
  <ANPR>
    <licensePlate>ab 12 cde</licensePlate>
    <confidenceLevel>88</confidenceLevel>
  </ANPR>
</EventNotificationAlert>"#;
        let (ty, plate, conf) = parse_anpr_event(xml).unwrap();
        assert_eq!(ty, "ANPR");
        assert_eq!(plate, "ab 12 cde");
        assert_eq!(conf, Some(88));
        assert_eq!(AnprConfig::normalize_plate(&plate), "AB12CDE");
    }

    #[test]
    fn find_jpeg_soi_skips_prefix() {
        let mut data = b"noise".to_vec();
        data.extend_from_slice(&[0xff, 0xd8, 0xff, 0xe0]);
        assert_eq!(find_jpeg_soi(&data), Some(5));
    }
}
