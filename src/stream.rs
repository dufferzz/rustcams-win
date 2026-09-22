use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSink, AppSinkCallbacks};
use gstreamer_video::VideoFrameExt;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::gst_link::DecodeBackend;
use crate::redact::{redact_secrets, redact_url};

#[derive(Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    /// Opaque RGBA (alpha = 255). Owned so the UI can take and upload without an
    /// extra pixel copy when this Arc is unique.
    pub rgba: Vec<u8>,
    pub seq: u64,
}

#[derive(Clone, Debug)]
pub struct StreamRequest {
    pub id: String,
    pub url: String,
    /// Optional override: "udp", "tcp", or "udp+tcp"
    pub protocols: Option<String>,
    /// Max decode/display width after videoscale (dense grids use smaller values).
    pub max_width: i32,
    /// Cap appsink delivery rate for multipane CPU (videorate).
    pub max_fps: i32,
    pub decode: DecodeBackend,
}

/// Live counters written on the appsink thread; cheap atomics for optimising.
struct SlotCounters {
    frames_out: AtomicU64,
    /// Frame replaced in the ring before the UI consumed it.
    frames_stale: AtomicU64,
    bytes_copied: AtomicU64,
    copy_ns: AtomicU64,
    /// appsink new_sample invocations (before drop filters).
    samples_in: AtomicU64,
    drop_rate: AtomicU64,
    drop_delta: AtomicU64,
    drop_key: AtomicU64,
    drop_corrupt: AtomicU64,
    /// Max gap between successful emits in this run (ms), reset by snapshot.
    emit_gap_max_ms: AtomicU64,
    width: AtomicU32,
    height: AtomicU32,
    decoder: Arc<Mutex<String>>,
}

impl Default for SlotCounters {
    fn default() -> Self {
        Self {
            frames_out: AtomicU64::new(0),
            frames_stale: AtomicU64::new(0),
            bytes_copied: AtomicU64::new(0),
            copy_ns: AtomicU64::new(0),
            samples_in: AtomicU64::new(0),
            drop_rate: AtomicU64::new(0),
            drop_delta: AtomicU64::new(0),
            drop_key: AtomicU64::new(0),
            drop_corrupt: AtomicU64::new(0),
            emit_gap_max_ms: AtomicU64::new(0),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            decoder: Arc::new(Mutex::new(String::new())),
        }
    }
}

#[derive(Clone, Debug)]
pub struct StreamDebugRow {
    pub id: String,
    pub running: bool,
    pub transport: &'static str,
    pub max_width: i32,
    pub max_fps: i32,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub stale_fps: f32,
    /// RGBA payload throughput from appsink copies.
    pub rgba_mbps: f32,
    pub avg_copy_us: f32,
    pub samples_in_fps: f32,
    pub drop_rate_fps: f32,
    pub drop_delta_fps: f32,
    pub drop_key_fps: f32,
    /// Worst emit-to-emit gap in the last sample window (ms).
    pub emit_gap_max_ms: u64,
    pub decoder: String,
    pub failures: u32,
    pub error: Option<String>,
    pub url_hint: String,
}

struct SlotRateWindow {
    frames: u64,
    stale: u64,
    bytes: u64,
    copy_ns: u64,
    samples_in: u64,
    drop_rate: u64,
    drop_delta: u64,
    drop_key: u64,
    at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransportMode {
    Udp,
    Tcp,
}

impl TransportMode {
    fn as_gst(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Udp => Self::Tcp,
            Self::Tcp => Self::Udp,
        }
    }

    fn from_config(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "udp" => Some(Self::Udp),
            "tcp" => Some(Self::Tcp),
            // Multi-protocol strings (e.g. "udp+tcp") are passed through to
            // rtspsrc as-is; reconnect state starts on UDP then can flip.
            other if other.contains('+') => Some(Self::Udp),
            _ => None,
        }
    }
}

struct SlotState {
    camera_id: String,
    url: String,
    protocols_override: Option<String>,
    max_width: i32,
    max_fps: i32,
    decode: DecodeBackend,
    transport: TransportMode,
    frame: Arc<Mutex<Option<Arc<VideoFrame>>>>,
    latest_seq: Arc<AtomicU64>,
    /// False until appsink sees a non-DELTA (key) frame — avoids green mid-GOP flash.
    seen_keyframe: Arc<AtomicBool>,
    /// Wall-clock nanos of last appsink emit (rate limit without videorate/PTS).
    last_emit_ns: Arc<AtomicU64>,
    error: Arc<Mutex<Option<String>>>,
    counters: Arc<SlotCounters>,
    rate_window: SlotRateWindow,
    pipeline: Option<gst::Pipeline>,
    reconnect_at: Option<Instant>,
    failures: u32,
}

pub struct StreamManager {
    slots: HashMap<String, SlotState>,
    seq: Arc<AtomicU64>,
    /// True when D3D11 convert/scale/download plugins are present (HW decode path).
    d3d11_available: bool,
}

impl StreamManager {
    pub fn new() -> Result<Self> {
        crate::gst_env::configure_bundled_gstreamer();
        gst::init().context("gstreamer init")?;
        info!(
            version = %gst::version_string(),
            "gstreamer initialized"
        );
        crate::gst_env::prefer_hardware_decoders();
        crate::gst_env::log_decoder_availability();
        let d3d11_available = crate::gst_env::d3d11_postproc_available();
        Ok(Self {
            slots: HashMap::new(),
            seq: Arc::new(AtomicU64::new(1)),
            d3d11_available,
        })
    }

    /// Whether the D3D11 post-process / HW decode path is available.
    pub fn d3d11_available(&self) -> bool {
        self.d3d11_available
    }

    pub fn sync_active(&mut self, desired: &[StreamRequest]) {
        let desired_ids: Vec<&str> = desired.iter().map(|d| d.id.as_str()).collect();

        let to_remove: Vec<String> = self
            .slots
            .keys()
            .filter(|id| !desired_ids.contains(&id.as_str()))
            .cloned()
            .collect();
        for id in to_remove {
            self.stop_slot(&id);
            self.slots.remove(&id);
        }

        for req in desired {
            let initial_transport = req
                .protocols
                .as_deref()
                .and_then(TransportMode::from_config)
                .unwrap_or(TransportMode::Udp);

            if let Some(slot) = self.slots.get_mut(&req.id) {
                let url_changed = slot.url != req.url;
                let proto_changed = slot.protocols_override != req.protocols;
                let res_changed = slot.max_width != req.max_width || slot.max_fps != req.max_fps;
                let dec_changed = slot.decode != req.decode;
                if url_changed || proto_changed || res_changed || dec_changed {
                    stop_slot_inner(slot);
                    slot.url = req.url.clone();
                    slot.protocols_override = req.protocols.clone();
                    slot.max_width = req.max_width;
                    slot.max_fps = req.max_fps;
                    slot.decode = req.decode;
                    slot.camera_id = req.id.clone();
                    slot.transport = initial_transport;
                    slot.failures = 0;
                    slot.reconnect_at = None;
                    slot.counters = Arc::new(SlotCounters::default());
                    slot.rate_window = SlotRateWindow::fresh();
                    if let Err(err) = start_pipeline(slot, &self.seq) {
                        *slot.error.lock() = Some(redact_secrets(&err.to_string()));
                        schedule_reconnect(slot);
                    }
                }
            } else {
                let mut slot = SlotState {
                    camera_id: req.id.clone(),
                    url: req.url.clone(),
                    protocols_override: req.protocols.clone(),
                    max_width: req.max_width,
                    max_fps: req.max_fps,
                    decode: req.decode,
                    transport: initial_transport,
                    frame: Arc::new(Mutex::new(None)),
                    latest_seq: Arc::new(AtomicU64::new(0)),
                    seen_keyframe: Arc::new(AtomicBool::new(false)),
                    last_emit_ns: Arc::new(AtomicU64::new(0)),
                    error: Arc::new(Mutex::new(None)),
                    counters: Arc::new(SlotCounters::default()),
                    rate_window: SlotRateWindow::fresh(),
                    pipeline: None,
                    reconnect_at: None,
                    failures: 0,
                };
                if let Err(err) = start_pipeline(&mut slot, &self.seq) {
                    *slot.error.lock() = Some(redact_secrets(&err.to_string()));
                    schedule_reconnect(&mut slot);
                }
                self.slots.insert(req.id.clone(), slot);
            }
        }

        let now = Instant::now();
        let ids: Vec<String> = self.slots.keys().cloned().collect();
        for id in ids {
            let needs = self
                .slots
                .get(&id)
                .map(|s| s.pipeline.is_none() && s.reconnect_at.map(|t| now >= t).unwrap_or(false))
                .unwrap_or(false);
            if needs {
                if let Some(slot) = self.slots.get_mut(&id) {
                    info!(
                        camera = %slot.camera_id,
                        transport = slot.transport.as_gst(),
                        "reconnecting stream"
                    );
                    if let Err(err) = start_pipeline(slot, &self.seq) {
                        *slot.error.lock() = Some(redact_secrets(&err.to_string()));
                        schedule_reconnect(slot);
                    }
                }
            }
        }

        let mut failed: HashSet<String> = HashSet::new();
        for (id, slot) in &self.slots {
            if let Some(pipeline) = &slot.pipeline {
                if let Some(bus) = pipeline.bus() {
                    while let Some(msg) = bus.pop() {
                        use gst::MessageView;
                        match msg.view() {
                            MessageView::Error(err) => {
                                let text = redact_secrets(&format!(
                                    "{} ({})",
                                    err.error(),
                                    err.debug().unwrap_or_default()
                                ));
                                // Log once per camera per cascade
                                if !failed.contains(id) {
                                    warn!(camera = %id, error = %text, "pipeline error");
                                    *slot.error.lock() = Some(short_error(&text));
                                }
                                failed.insert(id.clone());
                            }
                            MessageView::Eos(_) => {
                                if !failed.contains(id) {
                                    warn!(camera = %id, "pipeline EOS");
                                    *slot.error.lock() = Some("stream ended".into());
                                }
                                failed.insert(id.clone());
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        for id in failed {
            if let Some(slot) = self.slots.get_mut(&id) {
                stop_slot_inner(slot);
                // Only switch transport after repeated failures — flipping every
                // glitch causes reconnect stutter.
                let locked = slot
                    .protocols_override
                    .as_deref()
                    .and_then(TransportMode::from_config)
                    .is_some();
                if !locked && slot.failures >= 2 {
                    slot.transport = slot.transport.next();
                }
                schedule_reconnect(slot);
            }
        }
    }

    pub fn stop_all(&mut self) {
        let ids: Vec<String> = self.slots.keys().cloned().collect();
        for id in ids {
            self.stop_slot(&id);
        }
        self.slots.clear();
    }

    /// Takes the latest frame when `seq` is newer than `seen_seq` so the UI can
    /// `try_unwrap` without an RGBA clone. Clears the ring slot until appsink
    /// writes the next frame.
    pub fn frame_if_newer(&self, camera_id: &str, seen_seq: u64) -> Option<Arc<VideoFrame>> {
        let slot = self.slots.get(camera_id)?;
        let seq = slot.latest_seq.load(Ordering::Acquire);
        if seq == 0 || seq == seen_seq {
            return None;
        }
        let mut guard = slot.frame.lock();
        let frame = guard.as_ref()?;
        if frame.seq == seen_seq {
            return None;
        }
        guard.take()
    }

    pub fn error(&self, camera_id: &str) -> Option<String> {
        self.slots.get(camera_id)?.error.lock().clone()
    }

    pub fn is_running(&self, camera_id: &str) -> bool {
        self.slots
            .get(camera_id)
            .map(|s| s.pipeline.is_some())
            .unwrap_or(false)
    }

    /// True once a keyframe has been delivered for the current pipeline run.
    pub fn has_live_frame(&self, camera_id: &str) -> bool {
        self.slots
            .get(camera_id)
            .map(|s| s.latest_seq.load(Ordering::Acquire) > 0)
            .unwrap_or(false)
    }

    /// Pipeline is down but a reconnect is scheduled (error/eos backoff).
    pub fn is_reconnecting(&self, camera_id: &str) -> bool {
        self.slots
            .get(camera_id)
            .map(|s| s.pipeline.is_none() && s.reconnect_at.is_some())
            .unwrap_or(false)
    }

    /// Per-stream rates since the previous call (call ~1×/s from the UI).
    pub fn debug_snapshot(&mut self) -> Vec<StreamDebugRow> {
        let now = Instant::now();
        let mut rows = Vec::with_capacity(self.slots.len());
        for slot in self.slots.values_mut() {
            let frames = slot.counters.frames_out.load(Ordering::Relaxed);
            let stale = slot.counters.frames_stale.load(Ordering::Relaxed);
            let bytes = slot.counters.bytes_copied.load(Ordering::Relaxed);
            let copy_ns = slot.counters.copy_ns.load(Ordering::Relaxed);
            let samples_in = slot.counters.samples_in.load(Ordering::Relaxed);
            let drop_rate = slot.counters.drop_rate.load(Ordering::Relaxed);
            let drop_delta = slot.counters.drop_delta.load(Ordering::Relaxed);
            let drop_key = slot.counters.drop_key.load(Ordering::Relaxed);
            let emit_gap_max_ms = slot.counters.emit_gap_max_ms.swap(0, Ordering::Relaxed);
            let dt = now
                .saturating_duration_since(slot.rate_window.at)
                .as_secs_f32()
                .max(0.001);
            let d_frames = frames.saturating_sub(slot.rate_window.frames) as f32;
            let d_stale = stale.saturating_sub(slot.rate_window.stale) as f32;
            let d_bytes = bytes.saturating_sub(slot.rate_window.bytes) as f32;
            let d_copy_ns = copy_ns.saturating_sub(slot.rate_window.copy_ns);
            let d_samples = samples_in.saturating_sub(slot.rate_window.samples_in) as f32;
            let d_drop_rate = drop_rate.saturating_sub(slot.rate_window.drop_rate) as f32;
            let d_drop_delta = drop_delta.saturating_sub(slot.rate_window.drop_delta) as f32;
            let d_drop_key = drop_key.saturating_sub(slot.rate_window.drop_key) as f32;
            let d_frame_i = d_frames.max(1.0);
            slot.rate_window = SlotRateWindow {
                frames,
                stale,
                bytes,
                copy_ns,
                samples_in,
                drop_rate,
                drop_delta,
                drop_key,
                at: now,
            };

            rows.push(StreamDebugRow {
                id: slot.camera_id.clone(),
                running: slot.pipeline.is_some(),
                transport: slot.transport.as_gst(),
                max_width: slot.max_width,
                max_fps: slot.max_fps,
                width: slot.counters.width.load(Ordering::Relaxed),
                height: slot.counters.height.load(Ordering::Relaxed),
                fps: d_frames / dt,
                stale_fps: d_stale / dt,
                rgba_mbps: (d_bytes / dt) / (1024.0 * 1024.0),
                avg_copy_us: (d_copy_ns as f32 / d_frame_i) / 1000.0,
                samples_in_fps: d_samples / dt,
                drop_rate_fps: d_drop_rate / dt,
                drop_delta_fps: d_drop_delta / dt,
                drop_key_fps: d_drop_key / dt,
                emit_gap_max_ms,
                decoder: slot.counters.decoder.lock().clone(),
                failures: slot.failures,
                error: slot.error.lock().clone(),
                url_hint: redact_url(&slot.url),
            });
        }
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        rows
    }

    fn stop_slot(&mut self, id: &str) {
        if let Some(slot) = self.slots.get_mut(id) {
            stop_slot_inner(slot);
        }
    }
}

impl Drop for StreamManager {
    fn drop(&mut self) {
        self.stop_all();
    }
}

impl SlotRateWindow {
    fn fresh() -> Self {
        Self {
            frames: 0,
            stale: 0,
            bytes: 0,
            copy_ns: 0,
            samples_in: 0,
            drop_rate: 0,
            drop_delta: 0,
            drop_key: 0,
            at: Instant::now(),
        }
    }
}

pub fn log_stream_debug_rows(rows: &[StreamDebugRow]) {
    let n = rows.len();
    let running = rows.iter().filter(|r| r.running).count();
    let fps: f32 = rows.iter().map(|r| r.fps).sum();
    let mbps: f32 = rows.iter().map(|r| r.rgba_mbps).sum();
    let stale: f32 = rows.iter().map(|r| r.stale_fps).sum();
    let drop_rate: f32 = rows.iter().map(|r| r.drop_rate_fps).sum();
    let drop_delta: f32 = rows.iter().map(|r| r.drop_delta_fps).sum();
    let gap_max = rows.iter().map(|r| r.emit_gap_max_ms).max().unwrap_or(0);
    info!(
        streams = n,
        running,
        decode_fps = format!("{fps:.1}"),
        stale_fps = format!("{stale:.1}"),
        drop_rate_fps = format!("{drop_rate:.1}"),
        drop_delta_fps = format!("{drop_delta:.1}"),
        emit_gap_max_ms = gap_max,
        rgba_mib_s = format!("{mbps:.1}"),
        "perf summary"
    );
    for row in rows {
        info!(
            camera = %row.id,
            running = row.running,
            transport = row.transport,
            tier = format!("{}@{}fps", row.max_width, row.max_fps),
            size = format!("{}x{}", row.width, row.height),
            fps = format!("{:.1}", row.fps),
            stale = format!("{:.1}", row.stale_fps),
            in_fps = format!("{:.1}", row.samples_in_fps),
            drop_rate = format!("{:.1}", row.drop_rate_fps),
            drop_delta = format!("{:.1}", row.drop_delta_fps),
            drop_key = format!("{:.1}", row.drop_key_fps),
            gap_ms = row.emit_gap_max_ms,
            rgba_mib_s = format!("{:.2}", row.rgba_mbps),
            copy_us = format!("{:.0}", row.avg_copy_us),
            decoder = %row.decoder,
            failures = row.failures,
            err = row.error.as_deref().unwrap_or("-"),
            url = %row.url_hint,
            "perf stream"
        );
    }
}

fn short_error(text: &str) -> String {
    if text.contains("500") {
        "RTSP 500 from camera (will retry / switch transport)".into()
    } else if text.contains("401") || text.contains("Unauthorized") {
        "RTSP auth failed".into()
    } else if text.contains("Could not open resource") || text.contains("Failed to connect") {
        "Cannot connect to camera".into()
    } else {
        // Keep first line only
        text.lines()
            .next()
            .unwrap_or(text)
            .chars()
            .take(120)
            .collect()
    }
}

fn stop_slot_inner(slot: &mut SlotState) {
    if let Some(pipeline) = slot.pipeline.take() {
        let _ = pipeline.set_state(gst::State::Null);
        debug!(camera = %slot.camera_id, "pipeline stopped");
    }
    *slot.frame.lock() = None;
    slot.latest_seq.store(0, Ordering::Release);
    slot.seen_keyframe.store(false, Ordering::Release);
    slot.last_emit_ns.store(0, Ordering::Release);
}

fn schedule_reconnect(slot: &mut SlotState) {
    slot.failures = slot.failures.saturating_add(1);
    let secs = match slot.failures {
        0 | 1 => 2,
        2 => 5,
        3..=5 => 10,
        _ => 20,
    };
    slot.reconnect_at = Some(Instant::now() + Duration::from_secs(secs));
}

fn try_explicit_link(
    pad: &gst::Pad,
    pipeline_weak: &gst::glib::object::WeakRef<gst::Pipeline>,
    queue_weak: &gst::glib::object::WeakRef<gst::Element>,
    max_width: i32,
    max_height: i32,
    decoder_name_out: &Arc<Mutex<String>>,
    camera_id: &str,
    link_once: &Arc<AtomicBool>,
    decode: DecodeBackend,
) {
    if link_once.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(pipeline) = pipeline_weak.upgrade() else {
        link_once.store(false, Ordering::Release);
        return;
    };
    let Some(queue) = queue_weak.upgrade() else {
        link_once.store(false, Ordering::Release);
        return;
    };
    if let Err(err) = crate::gst_link::link_explicit_video(
        &pipeline,
        pad,
        &queue,
        max_width,
        max_height,
        decoder_name_out,
        decode,
        camera_id,
    ) {
        warn!(
            camera = %camera_id,
            error = %format!("{err:#}"),
            "explicit video link failed"
        );
        link_once.store(false, Ordering::Release);
    }
}

fn start_pipeline(slot: &mut SlotState, seq: &Arc<AtomicU64>) -> Result<()> {
    *slot.error.lock() = None;
    slot.reconnect_at = None;
    slot.seen_keyframe.store(false, Ordering::Release);
    slot.latest_seq.store(0, Ordering::Release);
    slot.last_emit_ns.store(0, Ordering::Release);
    *slot.frame.lock() = None;

    let pipeline = gst::Pipeline::new();

    // Low-latency RTSP; dense grids use smaller max_width / max_fps.
    let max_width = slot.max_width.max(1);
    let max_fps = slot.max_fps.max(1);
    // Bound both axes so portrait/substreams don't stay huge after width-only scale
    // (e.g. 320×576 was still ~2× the pixel cost of 320×180).
    let max_height = max_width;
    // Live CCTV: never clock-sync the sink. Sync holds for running-time then
    // drops late frames in bursts — feels like a hitch every ~GOP (often 1s).
    let clock_sync = false;
    let max_lateness: i64 = -1;
    // Smooth ~1s GOP / I-frame spikes: give the jitterbuffer ~half a second and
    // do not drop-on-latency (that was clipping around keyframes → gap_ms 300–900).
    let rtsp_latency_ms: u32 = if max_width > 640 { 500 } else { 450 };
    // GstVideoScaleMethod nick is "nearest-neighbour", not "nearest".
    let scale_method = if max_width <= 400 {
        "nearest-neighbour"
    } else {
        "bilinear"
    };

    let src = gst::ElementFactory::make("rtspsrc")
        .name("src")
        .property("location", &slot.url)
        .property("latency", rtsp_latency_ms)
        .property_from_str("protocols", slot.transport.as_gst())
        .property("drop-on-latency", false)
        .property("do-retransmission", false)
        .property("do-rtsp-keep-alive", true)
        .property("timeout", 5_000_000u64)
        .property("tcp-timeout", 5_000_000u64)
        .build()
        .context("create rtspsrc")?;

    info!(
        camera = %slot.camera_id,
        transport = slot.transport.as_gst(),
        latency_ms = rtsp_latency_ms,
        drop_on_latency = false,
        max_width,
        max_fps,
        decode = slot.decode.as_str(),
        sync = clock_sync,
        "starting pipeline"
    );

    // Reconnect alternates TransportMode (udp↔tcp). Multi-protocol overrides
    // like "udp+tcp" bypass that and are set directly on rtspsrc.
    if let Some(proto) = slot.protocols_override.as_deref() {
        let p = proto.trim().to_ascii_lowercase();
        if p.contains('+') || p == "udp-mcast" {
            src.set_property_from_str("protocols", &p);
        }
    }

    // Short leaky queue after decode/download: absorb I-frame decode bursts
    // without a deep backlog (deep queues used to drive 100% stale).
    let queue = gst::ElementFactory::make("queue")
        .name("q")
        .property("max-size-buffers", 4u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .property_from_str("leaky", "downstream")
        .build()
        .context("create queue")?;

    // No videorate: live PTS/DISCONT around IDR made it clump frames (gap_ms
    // 300–900 while out≈15). FPS is already limited by substream/tier + appsink drop.
    let convert = gst::ElementFactory::make("videoconvert")
        .name("convert")
        .property_from_str("n-threads", "0")
        .property("qos", false)
        .build()
        .context("create videoconvert")?;

    let scale = gst::ElementFactory::make("videoscale")
        .name("scale")
        .property_from_str("method", scale_method)
        .property("add-borders", false)
        .property("qos", false)
        .build()
        .context("create videoscale")?;

    let caps = gst::ElementFactory::make("capsfilter")
        .name("caps")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .field("width", gst::IntRange::<i32>::new(1, max_width))
                .field("height", gst::IntRange::<i32>::new(1, max_height))
                .build(),
        )
        .build()
        .context("create capsfilter")?;

    let sink = gst::ElementFactory::make("appsink")
        .name("sink")
        .property("max-buffers", 1u32)
        .property("drop", true)
        .property("sync", clock_sync)
        .property("qos", false)
        .property("max-lateness", max_lateness)
        .property("emit-signals", false)
        .build()
        .context("create appsink")?;

    pipeline.add_many([&src, &queue, &convert, &scale, &caps, &sink])?;
    gst::Element::link_many([&queue, &convert, &scale, &caps, &sink])
        .context("link queue → appsink")?;

    // Drop mid-GOP / corrupt buffers as early as possible (before convert), so
    // appsink never uploads green flash frames after connect/reconnect.
    let seen_kf_probe = Arc::clone(&slot.seen_keyframe);
    if let Some(queue_sink) = queue.static_pad("sink") {
        queue_sink.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            let Some(buffer) = info.buffer() else {
                return gst::PadProbeReturn::Ok;
            };
            let flags = buffer.flags();
            if seen_kf_probe.load(Ordering::Acquire) {
                if flags.contains(gst::BufferFlags::CORRUPTED) {
                    return gst::PadProbeReturn::Drop;
                }
                return gst::PadProbeReturn::Ok;
            }
            if flags.contains(gst::BufferFlags::DELTA_UNIT)
                || flags.contains(gst::BufferFlags::CORRUPTED)
            {
                return gst::PadProbeReturn::Drop;
            }
            seen_kf_probe.store(true, Ordering::Release);
            gst::PadProbeReturn::Ok
        });
    }

    // Explicit depay/parse/decode (no decodebin). Decoder from Settings or
    // RUSTCAMS_DECODE (sw / nvdec / hw).
    let pipeline_weak = pipeline.downgrade();
    let queue_weak = queue.downgrade();
    let max_width_link = max_width;
    let max_height_link = max_height;
    let decoder_name_out = Arc::clone(&slot.counters.decoder);
    let cam_for_link = slot.camera_id.clone();
    let link_once = Arc::new(AtomicBool::new(false));
    let decode_link = slot.decode;
    src.connect_pad_added(move |_src, pad| {
        if let Some(caps) = pad.current_caps() {
            if crate::gst_link::is_audio_caps(&caps) {
                return;
            }
        }

        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        if caps.is_any() || pad.current_caps().is_none() {
            let pipeline_weak = pipeline_weak.clone();
            let queue_weak = queue_weak.clone();
            let decoder_name_out = Arc::clone(&decoder_name_out);
            let cam_for_link = cam_for_link.clone();
            let link_once = Arc::clone(&link_once);
            pad.connect_notify(Some("caps"), move |pad, _| {
                try_explicit_link(
                    pad,
                    &pipeline_weak,
                    &queue_weak,
                    max_width_link,
                    max_height_link,
                    &decoder_name_out,
                    &cam_for_link,
                    &link_once,
                    decode_link,
                );
            });
            return;
        }
        try_explicit_link(
            pad,
            &pipeline_weak,
            &queue_weak,
            max_width_link,
            max_height_link,
            &decoder_name_out,
            &cam_for_link,
            &link_once,
            decode_link,
        );
    });

    let appsink = sink
        .dynamic_cast::<AppSink>()
        .map_err(|_| anyhow!("appsink cast"))?;

    let frame_store = Arc::clone(&slot.frame);
    let latest_seq = Arc::clone(&slot.latest_seq);
    let seen_keyframe = Arc::clone(&slot.seen_keyframe);
    let last_emit_ns = Arc::clone(&slot.last_emit_ns);
    let counters = Arc::clone(&slot.counters);
    let seq = Arc::clone(seq);
    appsink.set_callbacks(
        AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                counters.samples_in.fetch_add(1, Ordering::Relaxed);
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                // Wait for a keyframe after connect/reconnect. Mid-GOP joins often
                // yield green/corrupt decoded frames until the next IDR.
                let flags = buffer.flags();
                if !seen_keyframe.load(Ordering::Acquire) {
                    if flags.contains(gst::BufferFlags::DELTA_UNIT)
                        || flags.contains(gst::BufferFlags::CORRUPTED)
                    {
                        counters.drop_key.fetch_add(1, Ordering::Relaxed);
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    seen_keyframe.store(true, Ordering::Release);
                } else if flags.contains(gst::BufferFlags::CORRUPTED) {
                    counters.drop_corrupt.fetch_add(1, Ordering::Relaxed);
                    return Ok(gst::FlowSuccess::Ok);
                }

                let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                let info = gstreamer_video::VideoInfo::from_caps(caps)
                    .map_err(|_| gst::FlowError::Error)?;
                let t0 = Instant::now();
                // Avoid an extra full-buffer copy before reading plane data.
                let frame = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                    .map_err(|_| gst::FlowError::Error)?;
                let width = frame.width();
                let height = frame.height();
                let src = frame.plane_data(0).map_err(|_| gst::FlowError::Error)?;
                let stride = frame.plane_stride()[0] as usize;
                let row_bytes = (width as usize).saturating_mul(4);
                let rgba = copy_rgba_plane(src, stride, row_bytes, height as usize)
                    .ok_or(gst::FlowError::Error)?;
                let copy_ns = t0.elapsed().as_nanos() as u64;
                let bytes = rgba.len() as u64;
                let seq_n = seq.fetch_add(1, Ordering::Relaxed);
                let shared = Arc::new(VideoFrame {
                    width,
                    height,
                    rgba,
                    seq: seq_n,
                });
                {
                    let mut guard = frame_store.lock();
                    if guard.is_some() {
                        counters.frames_stale.fetch_add(1, Ordering::Relaxed);
                    }
                    *guard = Some(shared);
                }
                // Publish the sequence only after the corresponding frame is visible.
                // Otherwise the UI can observe a new sequence with the old frame and
                // unnecessarily defer presentation until its next repaint.
                latest_seq.store(seq_n, Ordering::Release);
                let now_ns = mono_ns();
                let last = last_emit_ns.swap(now_ns, Ordering::Relaxed);
                if last != 0 {
                    let gap_ms = now_ns.saturating_sub(last) / 1_000_000;
                    let mut prev = counters.emit_gap_max_ms.load(Ordering::Relaxed);
                    while gap_ms > prev {
                        match counters.emit_gap_max_ms.compare_exchange_weak(
                            prev,
                            gap_ms,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break,
                            Err(p) => prev = p,
                        }
                    }
                }
                counters.frames_out.fetch_add(1, Ordering::Relaxed);
                counters.bytes_copied.fetch_add(bytes, Ordering::Relaxed);
                counters.copy_ns.fetch_add(copy_ns, Ordering::Relaxed);
                counters.width.store(width, Ordering::Relaxed);
                counters.height.store(height, Ordering::Relaxed);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline
        .set_state(gst::State::Playing)
        .context("set pipeline Playing")?;

    // Request an IDR ASAP after mid-stream join (PLI/FIR via rtspsrc when possible).
    if let Some(sink_pad) = appsink.static_pad("sink") {
        let ev = gstreamer_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        let _ = sink_pad.send_event(ev);
    }

    slot.pipeline = Some(pipeline);
    info!(
        camera = %slot.camera_id,
        max_width = slot.max_width,
        max_fps = slot.max_fps,
        transport = slot.transport.as_gst(),
        url = %redact_url(&slot.url),
        "pipeline started"
    );
    Ok(())
}

/// Monotonic nanoseconds since first call (wall-clock rate limit; not PTS).
fn mono_ns() -> u64 {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

fn copy_rgba_plane(src: &[u8], stride: usize, row_bytes: usize, height: usize) -> Option<Vec<u8>> {
    let total = row_bytes.checked_mul(height)?;
    if stride == row_bytes {
        if src.len() < total {
            return None;
        }
        return Some(src[..total].to_vec());
    }
    let mut rgba = Vec::with_capacity(total);
    for y in 0..height {
        let start = y.checked_mul(stride)?;
        let end = start.checked_add(row_bytes)?;
        if end > src.len() {
            return None;
        }
        rgba.extend_from_slice(&src[start..end]);
    }
    Some(rgba)
}
