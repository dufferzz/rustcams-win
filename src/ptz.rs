//! Hikvision ISAPI PTZ — digest client modeled on gogateopener (`digest_http.go`).
//!
//! Critical: lock the per-host digest session for the **entire** HTTP round-trip
//! (including I/O), same as Go's `digestSession.mu`.

use crate::config::PtzTarget;
use parking_lot::{Condvar, Mutex};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use ureq::Agent;

/// Match typical Hikvision PTZ defaults (move 30 / zoom 25).
pub const PTZ_MOVE_SPEED: i32 = 30;
pub const PTZ_ZOOM_SPEED: i32 = 25;

/// Continuous PTZ speeds for Hikvision ISAPI (`-100..=100`, 0 = stop).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PtzVector {
    pub pan: i32,
    pub tilt: i32,
    pub zoom: i32,
    pub focus: i32,
}

/// One preset from `GET …/presets`.
#[derive(Debug, Clone)]
pub struct PtzPreset {
    pub id: u32,
    pub name: String,
}

/// Async snapshot of the last preset-list fetch for a PTZ target.
#[derive(Debug, Clone)]
pub enum PresetListStatus {
    Idle,
    Loading,
    Ready(Vec<PtzPreset>),
    Error(String),
}

#[derive(Default)]
struct PresetFetchState {
    /// `host|channel` of the request in flight / last completed.
    key: String,
    status: PresetListStatusInner,
    /// Bumped on each fetch request so stale responses are ignored.
    gen: u64,
}

#[derive(Clone)]
enum PresetListStatusInner {
    Idle,
    Loading,
    Ready(Vec<PtzPreset>),
    Error(String),
}

impl Default for PresetListStatusInner {
    fn default() -> Self {
        Self::Idle
    }
}

impl PtzVector {
    pub const STOP: Self = Self {
        pan: 0,
        tilt: 0,
        zoom: 0,
        focus: 0,
    };

    pub fn clamp(self) -> Self {
        Self {
            pan: self.pan.clamp(-100, 100),
            tilt: self.tilt.clamp(-100, 100),
            zoom: self.zoom.clamp(-100, 100),
            focus: self.focus.clamp(-100, 100),
        }
    }

    pub fn is_stop(self) -> bool {
        self.pan == 0 && self.tilt == 0 && self.zoom == 0 && self.focus == 0
    }
}

#[derive(Debug, Clone)]
enum OneShot {
    Home { target: PtzTarget },
    GotoPreset { target: PtzTarget, preset: u32 },
    StartPatrol { target: PtzTarget, patrol: u32 },
}

#[derive(Default)]
struct SharedState {
    desired: Option<(PtzTarget, PtzVector)>,
    oneshot: Option<OneShot>,
    gen: u64,
}

struct Shared {
    state: Mutex<SharedState>,
    cv: Condvar,
    sessions: Mutex<HashMap<String, Arc<Mutex<DigestSession>>>>,
    presets: Mutex<PresetFetchState>,
    agent: Agent,
}

pub struct PtzWorker {
    shared: Arc<Shared>,
}

impl PtzWorker {
    pub fn spawn() -> Self {
        let agent: Agent = Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(3)))
            .http_status_as_error(false)
            .max_idle_connections_per_host(4)
            .max_idle_age(Duration::from_secs(90))
            .build()
            .into();

        let shared = Arc::new(Shared {
            state: Mutex::new(SharedState::default()),
            cv: Condvar::new(),
            sessions: Mutex::new(HashMap::new()),
            presets: Mutex::new(PresetFetchState::default()),
            agent,
        });
        let worker = Arc::clone(&shared);
        thread::Builder::new()
            .name("ptz-worker".into())
            .spawn(move || worker_loop(worker))
            .expect("spawn ptz-worker");
        Self { shared }
    }

    fn preset_key(target: &PtzTarget) -> String {
        format!("{}|{}", target.host, target.channel)
    }

    /// Fetch `GET /ISAPI/PTZCtrl/channels/{ch}/presets` in the background.
    pub fn fetch_presets(&self, target: PtzTarget) {
        let key = Self::preset_key(&target);
        let gen = {
            let mut g = self.shared.presets.lock();
            if g.key == key && matches!(g.status, PresetListStatusInner::Loading) {
                return;
            }
            g.key = key.clone();
            g.status = PresetListStatusInner::Loading;
            g.gen = g.gen.wrapping_add(1);
            g.gen
        };

        let shared = Arc::clone(&self.shared);
        thread::Builder::new()
            .name("ptz-presets".into())
            .spawn(move || {
                let path = format!("/ISAPI/PTZCtrl/channels/{}/presets", target.channel);
                let result = digest_request(
                    &shared.agent,
                    &shared.sessions,
                    "GET",
                    &target,
                    &path,
                    None,
                    "",
                )
                .and_then(|(_status, body)| parse_ptz_presets(&body));

                let mut g = shared.presets.lock();
                if g.gen != gen || g.key != key {
                    return;
                }
                g.status = match result {
                    Ok(list) => PresetListStatusInner::Ready(list),
                    Err(err) => PresetListStatusInner::Error(err.to_string()),
                };
            })
            .ok();
    }

    /// Force a fresh preset fetch even if one is already cached/loading.
    pub fn refresh_presets(&self, target: PtzTarget) {
        {
            let mut g = self.shared.presets.lock();
            g.key.clear();
            g.status = PresetListStatusInner::Idle;
        }
        self.fetch_presets(target);
    }

    pub fn preset_status(&self, target: &PtzTarget) -> PresetListStatus {
        let key = Self::preset_key(target);
        let g = self.shared.presets.lock();
        if g.key != key {
            return PresetListStatus::Idle;
        }
        match &g.status {
            PresetListStatusInner::Idle => PresetListStatus::Idle,
            PresetListStatusInner::Loading => PresetListStatus::Loading,
            PresetListStatusInner::Ready(list) => PresetListStatus::Ready(list.clone()),
            PresetListStatusInner::Error(err) => PresetListStatus::Error(err.clone()),
        }
    }

    fn notify_desired(&self, target: PtzTarget, vec: PtzVector) {
        let mut g = self.shared.state.lock();
        g.desired = Some((target, vec.clamp()));
        g.gen = g.gen.wrapping_add(1);
        self.shared.cv.notify_one();
    }

    pub fn set(&self, target: PtzTarget, vec: PtzVector) {
        self.notify_desired(target, vec);
    }

    pub fn stop(&self) {
        let mut g = self.shared.state.lock();
        let Some((t, cur)) = g.desired.clone() else {
            return;
        };
        if cur.is_stop() {
            return;
        }
        g.desired = Some((t, PtzVector::STOP));
        g.gen = g.gen.wrapping_add(1);
        self.shared.cv.notify_one();
    }

    /// Warm digest + TCP keep-alive to the **camera** (not NVR) in the background.
    pub fn prewarm(&self, target: PtzTarget) {
        let shared = Arc::clone(&self.shared);
        thread::Builder::new()
            .name("ptz-prewarm".into())
            .spawn(move || {
                let _ = digest_request(
                    &shared.agent,
                    &shared.sessions,
                    "GET",
                    &target,
                    "/ISAPI/System/deviceInfo",
                    None,
                    "",
                );
            })
            .ok();
    }

    pub fn home(&self, target: PtzTarget) {
        let mut g = self.shared.state.lock();
        g.desired = Some((target.clone(), PtzVector::STOP));
        g.oneshot = Some(OneShot::Home { target });
        g.gen = g.gen.wrapping_add(1);
        self.shared.cv.notify_one();
    }

    pub fn goto_preset(&self, target: PtzTarget, preset: u32) {
        let mut g = self.shared.state.lock();
        g.desired = Some((target.clone(), PtzVector::STOP));
        g.oneshot = Some(OneShot::GotoPreset { target, preset });
        g.gen = g.gen.wrapping_add(1);
        self.shared.cv.notify_one();
    }

    pub fn start_patrol(&self, target: PtzTarget, patrol: u32) {
        let mut g = self.shared.state.lock();
        g.desired = Some((target.clone(), PtzVector::STOP));
        g.oneshot = Some(OneShot::StartPatrol { target, patrol });
        g.gen = g.gen.wrapping_add(1);
        self.shared.cv.notify_one();
    }
}

impl Drop for PtzWorker {
    fn drop(&mut self) {
        self.stop();
        self.shared.cv.notify_one();
    }
}

struct DigestSession {
    username: String,
    password: String,
    realm: String,
    nonce: String,
    qop: String,
    nc: u32,
    ha1: String,
}

impl DigestSession {
    fn new(username: String, password: String) -> Self {
        Self {
            username,
            password,
            realm: String::new(),
            nonce: String::new(),
            qop: "auth".into(),
            nc: 0,
            ha1: String::new(),
        }
    }

    fn apply_challenge(&mut self, www: &str) -> anyhow::Result<()> {
        let realm = extract_auth_param(www, "realm")
            .ok_or_else(|| anyhow::anyhow!("digest missing realm"))?;
        let nonce = extract_auth_param(www, "nonce")
            .ok_or_else(|| anyhow::anyhow!("digest missing nonce"))?;
        let mut qop = extract_auth_param(www, "qop").unwrap_or_else(|| "auth".into());
        if qop.contains(',') {
            qop = if qop.split(',').any(|p| p.trim() == "auth") {
                "auth".into()
            } else {
                qop.split(',').next().unwrap_or("auth").trim().into()
            };
        }
        self.realm = realm;
        self.nonce = nonce;
        self.qop = qop;
        self.ha1 = md5_hex(&format!(
            "{}:{}:{}",
            self.username, self.realm, self.password
        ));
        self.nc = 0;
        Ok(())
    }

    fn build_header(&mut self, method: &str, path: &str) -> String {
        self.nc = self.nc.wrapping_add(1);
        let nc = format!("{:08x}", self.nc);
        let cnonce = random_cnonce();
        let ha2 = md5_hex(&format!("{method}:{path}"));
        let response = md5_hex(&format!(
            "{}:{}:{}:{}:{}:{}",
            self.ha1, self.nonce, nc, cnonce, self.qop, ha2
        ));
        format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", qop={}, nc={}, cnonce=\"{}\", response=\"{}\"",
            self.username, self.realm, self.nonce, path, self.qop, nc, cnonce, response
        )
    }

    fn is_warm(&self) -> bool {
        !self.realm.is_empty() && !self.nonce.is_empty()
    }

    fn clear(&mut self) {
        self.realm.clear();
        self.nonce.clear();
        self.ha1.clear();
        self.nc = 0;
    }
}

fn session_handle(
    sessions: &Mutex<HashMap<String, Arc<Mutex<DigestSession>>>>,
    target: &PtzTarget,
) -> Arc<Mutex<DigestSession>> {
    let key = format!("{}|{}", target.host, target.username);
    let mut map = sessions.lock();
    map.entry(key)
        .or_insert_with(|| {
            Arc::new(Mutex::new(DigestSession::new(
                target.username.clone(),
                target.password.clone(),
            )))
        })
        .clone()
}

/// Full digest request with session mutex held for all I/O (matches Go).
/// Returns `(status, response body)`.
fn digest_request(
    agent: &Agent,
    sessions: &Mutex<HashMap<String, Arc<Mutex<DigestSession>>>>,
    method: &str,
    target: &PtzTarget,
    path: &str,
    body: Option<&[u8]>,
    content_type: &str,
) -> anyhow::Result<(u16, String)> {
    let url = format!("http://{}{}", target.host, path);

    let handle = session_handle(sessions, target);
    let mut sess = handle.lock();

    if sess.password != target.password {
        sess.password = target.password.clone();
        sess.clear();
    }

    for attempt in 0..2 {
        if !sess.is_warm() {
            let (status, www, resp_body) = send(agent, method, &url, body, content_type, None)?;
            if status != 401 {
                if (200..300).contains(&status) {
                    return Ok((status, resp_body));
                }
                anyhow::bail!("{method} {path} HTTP {status}");
            }
            let www = www.ok_or_else(|| anyhow::anyhow!("401 missing WWW-Authenticate"))?;
            sess.apply_challenge(&www)?;
        }

        let auth = sess.build_header(method, path);
        let (status, www, resp_body) = send(agent, method, &url, body, content_type, Some(&auth))?;

        if status == 401 && attempt == 0 {
            sess.clear();
            if let Some(www) = www {
                let _ = sess.apply_challenge(&www);
            }
            continue;
        }
        if !(200..300).contains(&status) {
            anyhow::bail!("{method} {path} HTTP {status}");
        }
        return Ok((status, resp_body));
    }
    anyhow::bail!("digest auth failed")
}

fn send(
    agent: &Agent,
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    content_type: &str,
    auth: Option<&str>,
) -> anyhow::Result<(u16, Option<String>, String)> {
    let mut resp = match method {
        "PUT" | "POST" => {
            let mut req = if method == "PUT" {
                agent.put(url)
            } else {
                agent.post(url)
            };
            if let Some(a) = auth {
                req = req.header("Authorization", a);
            }
            if !content_type.is_empty() {
                req = req.header("Content-Type", content_type);
            }
            req = req.header("Expect", "");
            if let Some(b) = body {
                req.send(b)
            } else {
                req.send_empty()
            }
        }
        _ => {
            let mut req = agent.get(url);
            if let Some(a) = auth {
                req = req.header("Authorization", a);
            }
            req.call()
        }
    }
    .map_err(|e| anyhow::anyhow!("{method} {url}: {e}"))?;

    let www = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let status = resp.status().as_u16();
    // Drain body so keep-alive connections can be reused.
    let resp_body = resp.body_mut().read_to_string().unwrap_or_default();
    Ok((status, www, resp_body))
}

fn worker_loop(shared: Arc<Shared>) {
    let mut last_sent: Option<(PtzTarget, PtzVector)> = None;
    let mut seen_gen = 0u64;

    loop {
        let (desired, oneshot, gen) = {
            let mut g = shared.state.lock();
            while g.gen == seen_gen {
                shared.cv.wait(&mut g);
            }
            seen_gen = g.gen;
            let oneshot = g.oneshot.take();
            (g.desired.clone(), oneshot, g.gen)
        };

        if let Some(shot) = oneshot {
            if let Some((t, _)) = &desired {
                let _ = continuous_put(&shared, t, PtzVector::STOP);
                last_sent = Some((t.clone(), PtzVector::STOP));
            }
            match shot {
                OneShot::Home { target } => {
                    let _ = oneshot_put(&shared, &target, "homeposition/goto");
                }
                OneShot::GotoPreset { target, preset } => {
                    let path = format!("presets/{preset}/goto");
                    let _ = oneshot_put(&shared, &target, &path);
                }
                OneShot::StartPatrol { target, patrol } => {
                    let path = format!("patrols/{patrol}/start");
                    let _ = oneshot_put(&shared, &target, &path);
                }
            }
            if shared.state.lock().gen != gen {
                continue;
            }
        }

        let Some((target, vec)) = desired else {
            continue;
        };

        if last_sent
            .as_ref()
            .is_some_and(|(t, v)| t == &target && *v == vec)
        {
            continue;
        }

        if let Some((prev, prev_vec)) = &last_sent {
            if prev != &target && !prev_vec.is_stop() {
                let _ = continuous_put(&shared, prev, PtzVector::STOP);
            }
        }

        if continuous_put(&shared, &target, vec).is_ok() {
            last_sent = Some((target, vec));
        }

        if shared.state.lock().gen != gen {
            continue;
        }
    }
}

fn continuous_put(shared: &Shared, target: &PtzTarget, vec: PtzVector) -> anyhow::Result<()> {
    let path = format!("/ISAPI/PTZCtrl/channels/{}/continuous", target.channel);
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <PTZData>\
           <pan>{}</pan>\
           <tilt>{}</tilt>\
           <zoom>{}</zoom>\
           <focus>{}</focus>\
         </PTZData>",
        vec.pan, vec.tilt, vec.zoom, vec.focus
    );
    digest_request(
        &shared.agent,
        &shared.sessions,
        "PUT",
        target,
        &path,
        Some(body.as_bytes()),
        "application/xml",
    )?;
    Ok(())
}

fn oneshot_put(shared: &Shared, target: &PtzTarget, suffix: &str) -> anyhow::Result<()> {
    let path = format!("/ISAPI/PTZCtrl/channels/{}/{}", target.channel, suffix);
    digest_request(
        &shared.agent,
        &shared.sessions,
        "PUT",
        target,
        &path,
        None,
        "",
    )?;
    Ok(())
}

fn parse_ptz_presets(xml: &str) -> anyhow::Result<Vec<PtzPreset>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut presets = Vec::new();
    let mut buf = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut in_preset = false;
    let mut cur_id: Option<u32> = None;
    let mut cur_name: Option<String> = None;
    let mut enabled = true;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = xml_local_name(e.name().as_ref());
                if local == "PTZPreset" {
                    in_preset = true;
                    cur_id = None;
                    cur_name = None;
                    enabled = true;
                }
                path.push(local);
            }
            Ok(Event::End(e)) => {
                let local = xml_local_name(e.name().as_ref());
                if local == "PTZPreset" && in_preset {
                    if enabled {
                        if let Some(id) = cur_id {
                            let name = cur_name
                                .take()
                                .filter(|n| !n.is_empty())
                                .unwrap_or_else(|| format!("Preset {id}"));
                            presets.push(PtzPreset { id, name });
                        }
                    }
                    in_preset = false;
                }
                if path.last().map(|s| s.as_str()) == Some(local.as_str()) {
                    path.pop();
                }
            }
            Ok(Event::Text(t)) => {
                if !in_preset || path.is_empty() {
                    continue;
                }
                let text = t.unescape().unwrap_or_default().into_owned();
                if text.is_empty() {
                    continue;
                }
                let leaf = path[path.len() - 1].as_str();
                let parent = path
                    .len()
                    .checked_sub(2)
                    .and_then(|i| path.get(i))
                    .map(|s| s.as_str());
                match (parent, leaf) {
                    (Some("PTZPreset"), "id") => cur_id = text.parse().ok(),
                    (Some("PTZPreset"), "presetName") => cur_name = Some(text),
                    (Some("PTZPreset"), "enabled") => {
                        enabled = !matches!(text.as_str(), "false" | "0");
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => anyhow::bail!("preset XML parse error at {}: {e}", reader.buffer_position()),
            _ => {}
        }
        buf.clear();
    }

    presets.sort_by_key(|p| p.id);
    Ok(presets)
}

fn xml_local_name(qname: &[u8]) -> String {
    let s = String::from_utf8_lossy(qname);
    s.rsplit('}')
        .next()
        .unwrap_or(&s)
        .rsplit(':')
        .next()
        .unwrap_or(&s)
        .to_string()
}

fn extract_auth_param(header: &str, key: &str) -> Option<String> {
    let key_eq = format!("{key}=");
    let lower = header.to_ascii_lowercase();
    let key_lower = key_eq.to_ascii_lowercase();
    let idx = lower.find(&key_lower)?;
    let mut rest = &header[idx + key_eq.len()..];
    rest = rest.trim_start();
    if rest.starts_with('"') {
        rest = &rest[1..];
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    } else {
        let end = rest
            .find([',', ' '])
            .unwrap_or(rest.len());
        Some(rest[..end].trim().to_string())
    }
}

fn md5_hex(s: &str) -> String {
    format!("{:x}", md5::compute(s.as_bytes()))
}

fn random_cnonce() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_enabled_presets() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<PTZPresetList version="2.0" xmlns="http://www.std-cgi.com/ver20/XMLSchema">
  <PTZPreset>
    <enabled>true</enabled>
    <id>1</id>
    <presetName>Gate</presetName>
  </PTZPreset>
  <PTZPreset>
    <enabled>false</enabled>
    <id>2</id>
    <presetName>Unset</presetName>
  </PTZPreset>
  <PTZPreset>
    <enabled>true</enabled>
    <id>10</id>
    <presetName>Parking</presetName>
  </PTZPreset>
</PTZPresetList>"#;
        let list = parse_ptz_presets(xml).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, 1);
        assert_eq!(list[0].name, "Gate");
        assert_eq!(list[1].id, 10);
        assert_eq!(list[1].name, "Parking");
    }
}
