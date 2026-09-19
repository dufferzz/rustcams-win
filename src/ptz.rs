//! Hikvision ISAPI PTZ — digest client modeled on gogateopener (`digest_http.go`).
//!
//! Critical: lock the per-host digest session for the **entire** HTTP round-trip
//! (including I/O), same as Go's `digestSession.mu`.

use crate::config::PtzTarget;
use parking_lot::{Condvar, Mutex};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tracing::{info, warn};
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

/// Idle-return (park) action from `GET …/parkaction`.
#[derive(Debug, Clone)]
pub struct ParkAction {
    pub enabled: bool,
    pub park_time_sec: Option<u32>,
    pub action_type: Option<String>,
    pub action_num: Option<u32>,
}

/// Async snapshot of the last park-action fetch for a PTZ target.
#[derive(Debug, Clone)]
pub enum ParkActionStatus {
    Idle,
    Loading,
    Ready(ParkAction),
    Error(String),
}

/// Intrusion detection (`GET …/Smart/FieldDetection/{ch}`) shown as Tracking.
#[derive(Debug, Clone)]
pub struct Tracking {
    pub enabled: bool,
}

/// Hikvision `AbsoluteHigh` pose from `GET …/PTZCtrl/channels/{ch}/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AbsolutePtz {
    elevation: i32,
    azimuth: i32,
    zoom: i32,
}

/// Async snapshot of the last intrusion-detection fetch for a PTZ target.
#[derive(Debug, Clone)]
pub enum TrackingStatus {
    Idle,
    Loading,
    Ready(Tracking),
    Error(String),
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

#[derive(Default)]
struct ParkFetchState {
    key: String,
    status: ParkStatusInner,
    gen: u64,
}

#[derive(Clone)]
enum ParkStatusInner {
    Idle,
    Loading,
    Ready(ParkAction),
    Error(String),
}

impl Default for ParkStatusInner {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Default)]
struct TrackingFetchState {
    key: String,
    status: TrackingStatusInner,
    gen: u64,
}

#[derive(Clone)]
enum TrackingStatusInner {
    Idle,
    Loading,
    Ready(Tracking),
    Error(String),
}

impl Default for TrackingStatusInner {
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
    park: Mutex<ParkFetchState>,
    tracking: Mutex<TrackingFetchState>,
    /// Working (host, ISAPI root) after the first successful PTZ call.
    route: Mutex<HashMap<String, (PtzTarget, String)>>,
    /// Working `PUT …/focus` path after the first successful FocusData call.
    focus_route: Mutex<HashMap<String, (PtzTarget, String)>>,
    route_logged: Mutex<HashSet<String>>,
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
            park: Mutex::new(ParkFetchState::default()),
            tracking: Mutex::new(TrackingFetchState::default()),
            route: Mutex::new(HashMap::new()),
            focus_route: Mutex::new(HashMap::new()),
            route_logged: Mutex::new(HashSet::new()),
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
                let result = digest_ptz(&shared, "GET", &target, "presets", None, "")
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

    /// Fetch `GET /ISAPI/PTZCtrl/channels/{ch}/parkaction` in the background.
    pub fn fetch_park_action(&self, target: PtzTarget) {
        let key = Self::preset_key(&target);
        let gen = {
            let mut g = self.shared.park.lock();
            if g.key == key && matches!(g.status, ParkStatusInner::Loading) {
                return;
            }
            g.key = key.clone();
            g.status = ParkStatusInner::Loading;
            g.gen = g.gen.wrapping_add(1);
            g.gen
        };

        let shared = Arc::clone(&self.shared);
        thread::Builder::new()
            .name("ptz-park".into())
            .spawn(move || {
                let result = get_park_action(&shared, &target);
                let mut g = shared.park.lock();
                if g.gen != gen || g.key != key {
                    return;
                }
                g.status = match result {
                    Ok(action) => ParkStatusInner::Ready(action),
                    Err(err) => ParkStatusInner::Error(err.to_string()),
                };
            })
            .ok();
    }

    pub fn refresh_park_action(&self, target: PtzTarget) {
        {
            let mut g = self.shared.park.lock();
            g.key.clear();
            g.status = ParkStatusInner::Idle;
        }
        self.fetch_park_action(target);
    }

    pub fn park_status(&self, target: &PtzTarget) -> ParkActionStatus {
        let key = Self::preset_key(target);
        let g = self.shared.park.lock();
        if g.key != key {
            return ParkActionStatus::Idle;
        }
        match &g.status {
            ParkStatusInner::Idle => ParkActionStatus::Idle,
            ParkStatusInner::Loading => ParkActionStatus::Loading,
            ParkStatusInner::Ready(action) => ParkActionStatus::Ready(action.clone()),
            ParkStatusInner::Error(err) => ParkActionStatus::Error(err.clone()),
        }
    }

    /// GET current park XML, flip `<enabled>`, PUT it back, then re-GET.
    pub fn set_park_enabled(&self, target: PtzTarget, enabled: bool) {
        let key = Self::preset_key(&target);
        let gen = {
            let mut g = self.shared.park.lock();
            g.key = key.clone();
            g.status = ParkStatusInner::Loading;
            g.gen = g.gen.wrapping_add(1);
            g.gen
        };

        let shared = Arc::clone(&self.shared);
        thread::Builder::new()
            .name("ptz-park-set".into())
            .spawn(move || {
                let result = set_park_enabled(&shared, &target, enabled);
                let mut g = shared.park.lock();
                if g.gen != gen || g.key != key {
                    return;
                }
                g.status = match result {
                    Ok(action) => ParkStatusInner::Ready(action),
                    Err(err) => ParkStatusInner::Error(err.to_string()),
                };
            })
            .ok();
    }

    /// Fetch `GET /ISAPI/Smart/FieldDetection/{ch}` in the background.
    pub fn fetch_tracking(&self, target: PtzTarget) {
        let key = Self::preset_key(&target);
        let gen = {
            let mut g = self.shared.tracking.lock();
            if g.key == key && matches!(g.status, TrackingStatusInner::Loading) {
                return;
            }
            g.key = key.clone();
            g.status = TrackingStatusInner::Loading;
            g.gen = g.gen.wrapping_add(1);
            g.gen
        };

        let shared = Arc::clone(&self.shared);
        thread::Builder::new()
            .name("ptz-tracking".into())
            .spawn(move || {
                let result = get_tracking(&shared, &target);
                let mut g = shared.tracking.lock();
                if g.gen != gen || g.key != key {
                    return;
                }
                g.status = match result {
                    Ok(tracking) => TrackingStatusInner::Ready(tracking),
                    Err(err) => TrackingStatusInner::Error(err.to_string()),
                };
            })
            .ok();
    }

    pub fn refresh_tracking(&self, target: PtzTarget) {
        {
            let mut g = self.shared.tracking.lock();
            g.key.clear();
            g.status = TrackingStatusInner::Idle;
        }
        self.fetch_tracking(target);
    }

    pub fn tracking_status(&self, target: &PtzTarget) -> TrackingStatus {
        let key = Self::preset_key(target);
        let g = self.shared.tracking.lock();
        if g.key != key {
            return TrackingStatus::Idle;
        }
        match &g.status {
            TrackingStatusInner::Idle => TrackingStatus::Idle,
            TrackingStatusInner::Loading => TrackingStatus::Loading,
            TrackingStatusInner::Ready(tracking) => TrackingStatus::Ready(tracking.clone()),
            TrackingStatusInner::Error(err) => TrackingStatus::Error(err.clone()),
        }
    }

    /// GET current FieldDetection XML, flip `<enabled>`, PUT it back, then re-GET.
    pub fn set_tracking_enabled(&self, target: PtzTarget, enabled: bool) {
        let key = Self::preset_key(&target);
        let gen = {
            let mut g = self.shared.tracking.lock();
            g.key = key.clone();
            g.status = TrackingStatusInner::Loading;
            g.gen = g.gen.wrapping_add(1);
            g.gen
        };

        let shared = Arc::clone(&self.shared);
        thread::Builder::new()
            .name("ptz-tracking-set".into())
            .spawn(move || {
                let result = set_tracking_enabled(&shared, &target, enabled);
                let mut g = shared.tracking.lock();
                if g.gen != gen || g.key != key {
                    return;
                }
                g.status = match result {
                    Ok(tracking) => TrackingStatusInner::Ready(tracking),
                    Err(err) => TrackingStatusInner::Error(err.to_string()),
                };
            })
            .ok();
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

    /// Warm digest + TCP keep-alive to the PTZ HTTP host (camera or NVR) in the background.
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
                stop_axes(&shared, t);
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
                stop_axes(&shared, prev);
            }
        }

        send_vector(&shared, &target, vec, last_sent.as_ref().map(|(_, v)| *v));
        last_sent = Some((target, vec));

        if shared.state.lock().gen != gen {
            continue;
        }
    }
}

fn ptz_roots(target: &PtzTarget) -> &'static [&'static str] {
    if target.via_nvr {
        &["ContentMgmt/PTZCtrlProxy", "PTZCtrl", "ContentMgmt/PTZCtrl"]
    } else {
        &["PTZCtrl"]
    }
}

fn ptz_candidates(target: &PtzTarget) -> Vec<(PtzTarget, &'static str)> {
    let mut out = Vec::new();
    for t in std::iter::once(target).chain(target.fallback.as_deref()) {
        for root in ptz_roots(t) {
            out.push((t.clone(), *root));
        }
    }
    out
}

fn ptz_path(root: &str, channel: u32, rel: &str) -> String {
    if rel.is_empty() {
        format!("/ISAPI/{root}/channels/{channel}")
    } else {
        format!("/ISAPI/{root}/channels/{channel}/{rel}")
    }
}

fn route_key(target: &PtzTarget) -> String {
    format!("{}|{}", target.host, target.channel)
}

fn digest_ptz(
    shared: &Shared,
    method: &str,
    target: &PtzTarget,
    rel: &str,
    body: Option<&[u8]>,
    content_type: &str,
) -> anyhow::Result<(u16, String)> {
    let key = route_key(target);
    if let Some((t, root)) = shared.route.lock().get(&key).cloned() {
        let path = ptz_path(&root, t.channel, rel);
        match digest_request(
            &shared.agent,
            &shared.sessions,
            method,
            &t,
            &path,
            body,
            content_type,
        ) {
            Ok(v) => return Ok(v),
            Err(err) => {
                shared.route.lock().remove(&key);
                warn!(
                    host = %t.host,
                    channel = t.channel,
                    path,
                    "PTZ route failed, trying alternatives: {err:#}"
                );
            }
        }
    }

    let mut last_err = None;
    for (t, root) in ptz_candidates(target) {
        let path = ptz_path(root, t.channel, rel);
        match digest_request(
            &shared.agent,
            &shared.sessions,
            method,
            &t,
            &path,
            body,
            content_type,
        ) {
            Ok(v) => {
                if shared.route_logged.lock().insert(key.clone()) {
                    info!(
                        host = %t.host,
                        channel = t.channel,
                        root,
                        "PTZ route selected"
                    );
                }
                shared.route.lock().insert(key, (t, root.to_string()));
                return Ok(v);
            }
            Err(err) => last_err = Some(err),
        }
    }

    let err = last_err.unwrap_or_else(|| anyhow::anyhow!("no PTZ ISAPI route"));
    if shared.route_logged.lock().insert(format!("fail|{key}")) {
        warn!(
            host = %target.host,
            channel = target.channel,
            "PTZ unavailable: {err:#}"
        );
    }
    Err(err)
}

fn stop_axes(shared: &Shared, target: &PtzTarget) {
    let _ = continuous_put(shared, target, PtzVector::STOP);
    let _ = focus_put(shared, target, 0);
}

fn send_vector(shared: &Shared, target: &PtzTarget, vec: PtzVector, last: Option<PtzVector>) {
    let move_changed = last.map(|v| (v.pan, v.tilt, v.zoom)) != Some((vec.pan, vec.tilt, vec.zoom));
    let focus_changed = last.map(|v| v.focus) != Some(vec.focus);
    if move_changed {
        if let Err(err) = continuous_put(shared, target, vec) {
            warn!(
                host = %target.host,
                channel = target.channel,
                "PTZ continuous failed: {err:#}"
            );
        }
    }
    if focus_changed {
        if let Err(err) = focus_put(shared, target, vec.focus) {
            warn!(
                host = %target.host,
                channel = target.channel,
                focus = vec.focus,
                "PTZ focus failed: {err:#}"
            );
        }
    }
}

fn continuous_put(shared: &Shared, target: &PtzTarget, vec: PtzVector) -> anyhow::Result<()> {
    // Official ISAPI continuous PTZ is pan/tilt/zoom only. Focus is a separate
    // `/System/Video/inputs/channels/{ch}/focus` FocusData PUT.
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <PTZData>\
           <pan>{}</pan>\
           <tilt>{}</tilt>\
           <zoom>{}</zoom>\
         </PTZData>",
        vec.pan, vec.tilt, vec.zoom
    );
    digest_ptz(
        shared,
        "PUT",
        target,
        "continuous",
        Some(body.as_bytes()),
        "application/xml",
    )?;
    Ok(())
}

fn focus_body(speed: i32) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><FocusData><focus>{}</focus></FocusData>",
        speed.clamp(-100, 100)
    )
}

fn focus_candidates(target: &PtzTarget) -> Vec<(PtzTarget, String)> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for t in std::iter::once(target).chain(target.fallback.as_deref()) {
        let mut channels = vec![t.channel];
        if t.channel != 1 {
            channels.push(1);
        }
        for ch in channels {
            for tmpl in [
                "/ISAPI/System/Video/inputs/channels/{ch}/focus",
                "/ISAPI/Image/channels/{ch}/focus",
            ] {
                let path = tmpl.replace("{ch}", &ch.to_string());
                if seen.insert((t.host.clone(), path.clone())) {
                    out.push((t.clone(), path));
                }
            }
        }
    }
    out
}

fn focus_put(shared: &Shared, target: &PtzTarget, speed: i32) -> anyhow::Result<()> {
    let body = focus_body(speed);
    let key = route_key(target);
    if let Some((t, path)) = shared.focus_route.lock().get(&key).cloned() {
        match digest_request(
            &shared.agent,
            &shared.sessions,
            "PUT",
            &t,
            &path,
            Some(body.as_bytes()),
            "application/xml",
        ) {
            Ok((_status, resp)) if isapi_status_error(&resp).is_none() => return Ok(()),
            Ok((_status, resp)) => {
                shared.focus_route.lock().remove(&key);
                warn!(
                    host = %t.host,
                    path,
                    "PTZ focus route rejected, trying alternatives: {}",
                    isapi_status_error(&resp).unwrap_or_else(|| resp)
                );
            }
            Err(err) => {
                shared.focus_route.lock().remove(&key);
                warn!(
                    host = %t.host,
                    path,
                    "PTZ focus route failed, trying alternatives: {err:#}"
                );
            }
        }
    }

    let mut last_err = None;
    for (t, path) in focus_candidates(target) {
        match digest_request(
            &shared.agent,
            &shared.sessions,
            "PUT",
            &t,
            &path,
            Some(body.as_bytes()),
            "application/xml",
        ) {
            Ok((_status, resp)) if isapi_status_error(&resp).is_none() => {
                if shared.route_logged.lock().insert(format!("focus|{key}")) {
                    info!(host = %t.host, path, "PTZ focus route selected");
                }
                shared.focus_route.lock().insert(key, (t, path));
                return Ok(());
            }
            Ok((_status, resp)) => {
                last_err = Some(anyhow::anyhow!(
                    "{path}: {}",
                    isapi_status_error(&resp).unwrap_or_else(|| resp)
                ));
            }
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no ISAPI focus route")))
}

fn isapi_status_error(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    if !lower.contains("responsestatus") {
        return None;
    }
    let code = xml_tag_text(&lower, "statuscode")
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(1);
    if code <= 1 {
        return None;
    }
    let detail = xml_tag_text(&lower, "statusstring").unwrap_or_else(|| code.to_string());
    Some(format!("ISAPI statusCode {code} ({detail})"))
}

fn xml_tag_text(xml_lower: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml_lower.find(&open)? + open.len();
    let end = xml_lower[start..].find(&close)? + start;
    let text = xml_lower[start..end].trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn oneshot_put(shared: &Shared, target: &PtzTarget, suffix: &str) -> anyhow::Result<()> {
    digest_ptz(shared, "PUT", target, suffix, None, "")?;
    Ok(())
}

fn get_park_action(shared: &Shared, target: &PtzTarget) -> anyhow::Result<ParkAction> {
    let (_status, body) = digest_ptz(shared, "GET", target, "parkaction", None, "")?;
    parse_park_action(&body)
}

fn set_park_enabled(
    shared: &Shared,
    target: &PtzTarget,
    enabled: bool,
) -> anyhow::Result<ParkAction> {
    let (_status, body) = digest_ptz(shared, "GET", target, "parkaction", None, "")?;
    let xml = rewrite_park_enabled(&body, enabled)?;
    digest_ptz(
        shared,
        "PUT",
        target,
        "parkaction",
        Some(xml.as_bytes()),
        "application/xml",
    )?;
    get_park_action(shared, target)
}

fn parse_park_action(xml: &str) -> anyhow::Result<ParkAction> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut enabled = false;
    let mut saw_enabled = false;
    let mut park_time_sec = None;
    let mut action_type = None;
    let mut action_num = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                path.push(xml_local_name(e.name().as_ref()));
            }
            Ok(Event::End(_)) => {
                path.pop();
            }
            Ok(Event::Text(t)) => {
                if path.is_empty() {
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
                    .map(|s| s.to_ascii_lowercase());
                let leaf_l = leaf.to_ascii_lowercase();
                match (parent.as_deref(), leaf_l.as_str()) {
                    (Some("parkaction"), "enabled") => {
                        saw_enabled = true;
                        enabled = matches!(text.to_ascii_lowercase().as_str(), "true" | "1");
                    }
                    (Some("parkaction"), "parktime" | "returntime" | "time") => {
                        park_time_sec = text.parse().ok();
                    }
                    (Some("parkaction") | Some("action"), "actiontype" | "action") => {
                        action_type = Some(text);
                    }
                    (Some("parkaction") | Some("action"), "actionnum" | "actionid") => {
                        action_num = text.parse().ok();
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => anyhow::bail!("park XML parse error at {}: {e}", reader.buffer_position()),
            _ => {}
        }
        buf.clear();
    }

    if !saw_enabled && !xml.to_ascii_lowercase().contains("<parkaction") {
        anyhow::bail!("not a ParkAction response");
    }
    Ok(ParkAction {
        enabled,
        park_time_sec,
        action_type,
        action_num,
    })
}

fn rewrite_park_enabled(xml: &str, enabled: bool) -> anyhow::Result<String> {
    let value = if enabled { "true" } else { "false" };
    if let Some(out) = replace_first_tag_text(xml, "enabled", value) {
        return Ok(out);
    }
    anyhow::bail!("ParkAction XML has no <enabled> element")
}

fn field_detection_paths(target: &PtzTarget) -> [String; 2] {
    [
        format!("/ISAPI/Smart/FieldDetection/{}", target.channel),
        format!("/ISAPI/Smart/channels/{}/fieldDetection", target.channel),
    ]
}

fn get_field_detection_xml(
    shared: &Shared,
    target: &PtzTarget,
) -> anyhow::Result<(String, String)> {
    let mut last_err = None;
    for path in field_detection_paths(target) {
        match digest_request(
            &shared.agent,
            &shared.sessions,
            "GET",
            target,
            &path,
            None,
            "",
        ) {
            Ok((_, body)) => {
                if parse_tracking(&body).is_ok() {
                    return Ok((path, body));
                }
                last_err = Some(anyhow::anyhow!("GET {path}: not a FieldDetection response"));
            }
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("FieldDetection not available")))
}

fn snapshot_restore<T>(
    shared: &Shared,
    target: &PtzTarget,
    op: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let saved = match get_absolute_ptz(shared, target) {
        Ok(pos) => Some(pos),
        Err(err) => {
            warn!(
                host = %target.host,
                channel = target.channel,
                "PTZ status unavailable before tracking check: {err:#}"
            );
            None
        }
    };
    let result = op();
    if let Some(pos) = saved {
        // FieldDetection can start a slew after the HTTP response returns.
        thread::sleep(Duration::from_millis(250));
        if let Err(err) = put_absolute_ptz(shared, target, pos) {
            warn!(
                host = %target.host,
                channel = target.channel,
                "restore PTZ after tracking failed: {err:#}"
            );
        } else {
            thread::sleep(Duration::from_millis(400));
            let _ = put_absolute_ptz(shared, target, pos);
        }
    }
    result
}

fn get_absolute_ptz(shared: &Shared, target: &PtzTarget) -> anyhow::Result<AbsolutePtz> {
    let mut last_err = None;
    for rel in ["status", "absolute"] {
        match digest_ptz(shared, "GET", target, rel, None, "") {
            Ok((_, body)) => match parse_absolute_ptz(&body) {
                Ok(pos) => return Ok(pos),
                Err(err) => last_err = Some(err),
            },
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("PTZ status not available")))
}

fn put_absolute_ptz(
    shared: &Shared,
    target: &PtzTarget,
    pos: AbsolutePtz,
) -> anyhow::Result<()> {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<PTZData>\
<AbsoluteHigh>\
<elevation>{}</elevation>\
<azimuth>{}</azimuth>\
<absoluteZoom>{}</absoluteZoom>\
</AbsoluteHigh>\
</PTZData>",
        pos.elevation, pos.azimuth, pos.zoom
    );
    digest_ptz(
        shared,
        "PUT",
        target,
        "absolute",
        Some(body.as_bytes()),
        "application/xml",
    )?;
    Ok(())
}

fn parse_absolute_ptz(xml: &str) -> anyhow::Result<AbsolutePtz> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut elevation = None;
    let mut azimuth = None;
    let mut zoom = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                path.push(xml_local_name(e.name().as_ref()));
            }
            Ok(Event::End(_)) => {
                path.pop();
            }
            Ok(Event::Text(t)) => {
                if path.is_empty() {
                    continue;
                }
                let text = t.unescape().unwrap_or_default().into_owned();
                if text.is_empty() {
                    continue;
                }
                let leaf = path[path.len() - 1].to_ascii_lowercase();
                let val = text.parse::<i32>().ok();
                match leaf.as_str() {
                    "elevation" | "tilt" => elevation = val.or(elevation),
                    "azimuth" | "pan" => azimuth = val.or(azimuth),
                    "absolutezoom" | "zoom" => zoom = val.or(zoom),
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => anyhow::bail!("PTZ status XML parse error at {}: {e}", reader.buffer_position()),
            _ => {}
        }
        buf.clear();
    }

    match (elevation, azimuth, zoom) {
        (Some(elevation), Some(azimuth), Some(zoom)) => Ok(AbsolutePtz {
            elevation,
            azimuth,
            zoom,
        }),
        _ => anyhow::bail!("PTZ status XML missing elevation/azimuth/zoom"),
    }
}

fn get_tracking(shared: &Shared, target: &PtzTarget) -> anyhow::Result<Tracking> {
    snapshot_restore(shared, target, || {
        let (_path, body) = get_field_detection_xml(shared, target)?;
        parse_tracking(&body)
    })
}

fn set_tracking_enabled(
    shared: &Shared,
    target: &PtzTarget,
    enabled: bool,
) -> anyhow::Result<Tracking> {
    snapshot_restore(shared, target, || {
        let (path, body) = get_field_detection_xml(shared, target)?;
        let xml = rewrite_tracking_enabled(&body, enabled)?;
        digest_request(
            &shared.agent,
            &shared.sessions,
            "PUT",
            target,
            &path,
            Some(xml.as_bytes()),
            "application/xml",
        )?;
        let (_path, body) = get_field_detection_xml(shared, target)?;
        parse_tracking(&body)
    })
}

fn parse_tracking(xml: &str) -> anyhow::Result<Tracking> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut enabled = false;
    let mut saw_enabled = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                path.push(xml_local_name(e.name().as_ref()));
            }
            Ok(Event::End(_)) => {
                path.pop();
            }
            Ok(Event::Text(t)) => {
                if path.is_empty() {
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
                    .map(|s| s.to_ascii_lowercase());
                let leaf_l = leaf.to_ascii_lowercase();
                if !saw_enabled
                    && leaf_l == "enabled"
                    && matches!(
                        parent.as_deref(),
                        Some("fielddetection" | "intrusiondetection")
                    )
                {
                    saw_enabled = true;
                    enabled = matches!(text.to_ascii_lowercase().as_str(), "true" | "1");
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => anyhow::bail!(
                "FieldDetection XML parse error at {}: {e}",
                reader.buffer_position()
            ),
            _ => {}
        }
        buf.clear();
    }

    if !saw_enabled {
        anyhow::bail!("not a FieldDetection response");
    }
    Ok(Tracking { enabled })
}

fn rewrite_tracking_enabled(xml: &str, enabled: bool) -> anyhow::Result<String> {
    let value = if enabled { "true" } else { "false" };
    if let Some(out) = replace_first_tag_text(xml, "enabled", value) {
        return Ok(out);
    }
    anyhow::bail!("FieldDetection XML has no <enabled> element")
}

/// Replace the text of the first `<tag>…</tag>` (case-insensitive local name).
fn replace_first_tag_text(xml: &str, local_name: &str, new_text: &str) -> Option<String> {
    let lower = xml.to_ascii_lowercase();
    let open = format!("<{}", local_name.to_ascii_lowercase());
    let mut search_from = 0usize;
    while let Some(rel) = lower[search_from..].find(&open) {
        let abs = search_from + rel;
        let after_name = abs + open.len();
        let next = xml.get(after_name..)?.chars().next()?;
        if !matches!(next, '>' | ' ' | '\t' | '\n' | '\r' | '/') {
            search_from = after_name;
            continue;
        }
        let gt_rel = xml[after_name..].find('>')?;
        let open_end = after_name + gt_rel;
        if xml[abs..=open_end].trim_end().ends_with("/>") {
            search_from = open_end + 1;
            continue;
        }
        let content_start = open_end + 1;
        let close = format!("</{}", local_name.to_ascii_lowercase());
        let close_rel = lower[content_start..].find(&close)?;
        let content_end = content_start + close_rel;
        let mut out = String::with_capacity(xml.len() + new_text.len());
        out.push_str(&xml[..content_start]);
        out.push_str(new_text);
        out.push_str(&xml[content_end..]);
        return Some(out);
    }
    None
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

    #[test]
    fn focus_xml_uses_focusdata() {
        let xml = focus_body(-25);
        assert!(xml.contains("<FocusData>"));
        assert!(xml.contains("<focus>-25</focus>"));
        assert!(!xml.contains("PTZData"));
    }

    #[test]
    fn isapi_ok_response_is_not_an_error() {
        let xml = r#"<ResponseStatus>
  <statusCode>1</statusCode>
  <statusString>OK</statusString>
</ResponseStatus>"#;
        assert_eq!(isapi_status_error(xml), None);
    }

    #[test]
    fn isapi_invalid_operation_is_an_error() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ResponseStatus>
  <requestURL>/ISAPI/System/Video/inputs/channels/1/focus</requestURL>
  <statusCode>4</statusCode>
  <statusString>Invalid Operation</statusString>
</ResponseStatus>"#;
        let err = isapi_status_error(xml).unwrap();
        assert!(err.contains("4"));
        assert!(err.to_ascii_lowercase().contains("invalid operation"));
    }

    #[test]
    fn parses_nested_park_action() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ParkAction version="2.0" xmlns="http://www.isapi.org/ver20/XMLSchema">
  <enabled>true</enabled>
  <Parktime>120</Parktime>
  <Action>
    <ActionType>preset</ActionType>
    <ActionNum>2</ActionNum>
  </Action>
</ParkAction>"#;
        let park = parse_park_action(xml).unwrap();
        assert!(park.enabled);
        assert_eq!(park.park_time_sec, Some(120));
        assert_eq!(park.action_type.as_deref(), Some("preset"));
        assert_eq!(park.action_num, Some(2));
    }

    #[test]
    fn parses_disabled_flat_park_action() {
        let xml = r#"<ParkAction>
  <enabled>false</enabled>
  <returnTime>5</returnTime>
  <actionType>patrol</actionType>
  <actionNum>1</actionNum>
</ParkAction>"#;
        let park = parse_park_action(xml).unwrap();
        assert!(!park.enabled);
        assert_eq!(park.park_time_sec, Some(5));
        assert_eq!(park.action_type.as_deref(), Some("patrol"));
        assert_eq!(park.action_num, Some(1));
    }

    #[test]
    fn rewrites_enabled_preserving_rest() {
        let xml = "<ParkAction><enabled>true</enabled><Parktime>30</Parktime></ParkAction>";
        let out = rewrite_park_enabled(xml, false).unwrap();
        assert!(out.contains("<enabled>false</enabled>"));
        assert!(out.contains("<Parktime>30</Parktime>"));
        let park = parse_park_action(&out).unwrap();
        assert!(!park.enabled);
        assert_eq!(park.park_time_sec, Some(30));
    }

    #[test]
    fn parses_field_detection_ignoring_region_enabled() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<FieldDetection version="2.0" xmlns="http://www.isapi.org/ver20/XMLSchema">
  <id>1</id>
  <enabled>false</enabled>
  <FieldDetectionRegionList>
    <FieldDetectionRegion>
      <id>1</id>
      <enabled>true</enabled>
    </FieldDetectionRegion>
  </FieldDetectionRegionList>
</FieldDetection>"#;
        let tracking = parse_tracking(xml).unwrap();
        assert!(!tracking.enabled);
    }

    #[test]
    fn rewrites_field_detection_top_enabled_only() {
        let xml = "<FieldDetection><enabled>true</enabled><FieldDetectionRegion><enabled>true</enabled></FieldDetectionRegion></FieldDetection>";
        let out = rewrite_tracking_enabled(xml, false).unwrap();
        assert!(out.starts_with("<FieldDetection><enabled>false</enabled>"));
        assert!(out.contains("<FieldDetectionRegion><enabled>true</enabled>"));
        let tracking = parse_tracking(&out).unwrap();
        assert!(!tracking.enabled);
    }

    #[test]
    fn parses_absolute_high_status() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<PTZStatus version="2.0" xmlns="http://www.isapi.org/ver20/XMLSchema">
  <AbsoluteHigh>
    <elevation>450</elevation>
    <azimuth>1350</azimuth>
    <absoluteZoom>10</absoluteZoom>
  </AbsoluteHigh>
</PTZStatus>"#;
        let pos = parse_absolute_ptz(xml).unwrap();
        assert_eq!(
            pos,
            AbsolutePtz {
                elevation: 450,
                azimuth: 1350,
                zoom: 10
            }
        );
    }
}
