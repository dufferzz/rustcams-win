use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSink, AppSinkCallbacks};
use gstreamer_video::VideoFrameExt;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

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
}

/// Live counters written on the appsink thread; cheap atomics for optimising.
#[derive(Default)]
struct SlotCounters {
    frames_out: AtomicU64,
    /// Frame replaced in the ring before the UI consumed it.
    frames_stale: AtomicU64,
    bytes_copied: AtomicU64,
    copy_ns: AtomicU64,
    width: AtomicU32,
    height: AtomicU32,
    decoder: Mutex<String>,
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
    transport: TransportMode,
    frame: Arc<Mutex<Option<Arc<VideoFrame>>>>,
    latest_seq: Arc<AtomicU64>,
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
                let res_changed =
                    slot.max_width != req.max_width || slot.max_fps != req.max_fps;
                if url_changed || proto_changed || res_changed {
                    stop_slot_inner(slot);
                    slot.url = req.url.clone();
                    slot.protocols_override = req.protocols.clone();
                    slot.max_width = req.max_width;
                    slot.max_fps = req.max_fps;
                    slot.camera_id = req.id.clone();
                    slot.transport = initial_transport;
                    slot.failures = 0;
                    slot.reconnect_at = None;
                    slot.counters = Arc::new(SlotCounters::default());
                    slot.rate_window = SlotRateWindow::fresh();
                    if let Err(err) = start_pipeline(slot, &self.seq) {
                        *slot.error.lock() = Some(err.to_string());
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
                    transport: initial_transport,
                    frame: Arc::new(Mutex::new(None)),
                    latest_seq: Arc::new(AtomicU64::new(0)),
                    error: Arc::new(Mutex::new(None)),
                    counters: Arc::new(SlotCounters::default()),
                    rate_window: SlotRateWindow::fresh(),
                    pipeline: None,
                    reconnect_at: None,
                    failures: 0,
                };
                if let Err(err) = start_pipeline(&mut slot, &self.seq) {
                    *slot.error.lock() = Some(err.to_string());
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
                        *slot.error.lock() = Some(err.to_string());
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
                                let text = format!(
                                    "{} ({})",
                                    err.error(),
                                    err.debug().unwrap_or_default()
                                );
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

    /// Returns a frame only when `seq` is newer than `seen_seq` (Arc clone, not pixel copy).
    pub fn frame_if_newer(&self, camera_id: &str, seen_seq: u64) -> Option<Arc<VideoFrame>> {
        let slot = self.slots.get(camera_id)?;
        let seq = slot.latest_seq.load(Ordering::Acquire);
        if seq == 0 || seq == seen_seq {
            return None;
        }
        let guard = slot.frame.lock();
        let frame = guard.as_ref()?;
        if frame.seq == seen_seq {
            return None;
        }
        Some(Arc::clone(frame))
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

    /// Per-stream rates since the previous call (call ~1×/s from the UI).
    pub fn debug_snapshot(&mut self) -> Vec<StreamDebugRow> {
        let now = Instant::now();
        let mut rows = Vec::with_capacity(self.slots.len());
        for slot in self.slots.values_mut() {
            let frames = slot.counters.frames_out.load(Ordering::Relaxed);
            let stale = slot.counters.frames_stale.load(Ordering::Relaxed);
            let bytes = slot.counters.bytes_copied.load(Ordering::Relaxed);
            let copy_ns = slot.counters.copy_ns.load(Ordering::Relaxed);
            let dt = now
                .saturating_duration_since(slot.rate_window.at)
                .as_secs_f32()
                .max(0.001);
            let d_frames = frames.saturating_sub(slot.rate_window.frames) as f32;
            let d_stale = stale.saturating_sub(slot.rate_window.stale) as f32;
            let d_bytes = bytes.saturating_sub(slot.rate_window.bytes) as f32;
            let d_copy_ns = copy_ns.saturating_sub(slot.rate_window.copy_ns);
            let d_frame_i = d_frames.max(1.0);
            slot.rate_window = SlotRateWindow {
                frames,
                stale,
                bytes,
                copy_ns,
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
    info!(
        streams = n,
        running,
        decode_fps = format!("{fps:.1}"),
        stale_fps = format!("{stale:.1}"),
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

fn redact_url(url: &str) -> String {
    // Hide credentials if present: rtsp://user:pass@host/...
    if let Some(at) = url.find('@') {
        if let Some(scheme) = url.find("://") {
            return format!("{}://***@{}", &url[..scheme], &url[at + 1..]);
        }
    }
    url.chars().take(96).collect()
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
        text.lines().next().unwrap_or(text).chars().take(120).collect()
    }
}

fn stop_slot_inner(slot: &mut SlotState) {
    if let Some(pipeline) = slot.pipeline.take() {
        let _ = pipeline.set_state(gst::State::Null);
        debug!(camera = %slot.camera_id, "pipeline stopped");
    }
    *slot.frame.lock() = None;
    slot.latest_seq.store(0, Ordering::Release);
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

fn start_pipeline(slot: &mut SlotState, seq: &Arc<AtomicU64>) -> Result<()> {
    *slot.error.lock() = None;
    slot.reconnect_at = None;

    let pipeline = gst::Pipeline::new();

    // Low-latency RTSP; dense grids use smaller max_width / max_fps.
    const RTSP_LATENCY_MS: u32 = 200;
    const MAX_LATENESS_NS: i64 = 100_000_000;
    let max_width = slot.max_width.max(1);
    let max_fps = slot.max_fps.max(1);
    // Bound both axes so portrait/substreams don't stay huge after width-only scale
    // (e.g. 320×576 was still ~2× the pixel cost of 320×180).
    let max_height = max_width;
    let clock_sync = true;
    // GstVideoScaleMethod nick is "nearest-neighbour", not "nearest".
    let scale_method = if max_width <= 400 {
        "nearest-neighbour"
    } else {
        "bilinear"
    };

    let src = gst::ElementFactory::make("rtspsrc")
        .name("src")
        .property("location", &slot.url)
        .property("latency", RTSP_LATENCY_MS)
        .property_from_str("protocols", slot.transport.as_gst())
        .property("drop-on-latency", true)
        .property("do-retransmission", false)
        .property("do-rtsp-keep-alive", true)
        .property("timeout", 5_000_000u64)
        .property("tcp-timeout", 5_000_000u64)
        .build()
        .context("create rtspsrc")?;

    info!(
        camera = %slot.camera_id,
        transport = slot.transport.as_gst(),
        latency_ms = RTSP_LATENCY_MS,
        max_width,
        max_fps,
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

    let decode = gst::ElementFactory::make("decodebin")
        .name("decode")
        .build()
        .context("create decodebin")?;

    // Tiny leaky queue after decode/download: prefer latest frame over backlog
    // on low-power hosts (deep queues caused 100% stale + high RGBA copy).
    let queue = gst::ElementFactory::make("queue")
        .name("q")
        .property("max-size-buffers", 2u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .property_from_str("leaky", "downstream")
        .build()
        .context("create queue")?;

    // drop-only + max-rate: live RTSP buffers often lack DURATION; classic
    // videorate + framerate caps asserts and aborts the process.
    let rate = gst::ElementFactory::make("videorate")
        .name("rate")
        .property("skip-to-first", true)
        .property("drop-only", true)
        .property("max-rate", max_fps)
        .build()
        .context("create videorate")?;

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
        .property("max-lateness", MAX_LATENESS_NS)
        .property("emit-signals", false)
        .build()
        .context("create appsink")?;

    pipeline.add_many([
        &src, &decode, &queue, &rate, &convert, &scale, &caps, &sink,
    ])?;
    gst::Element::link_many([&queue, &rate, &convert, &scale, &caps, &sink])
        .context("link queue → appsink")?;

    let decode_weak = decode.downgrade();
    src.connect_pad_added(move |_src, pad| {
        let Some(decode) = decode_weak.upgrade() else {
            return;
        };
        let Some(sink_pad) = decode.static_pad("sink") else {
            return;
        };
        if sink_pad.is_linked() {
            return;
        }
        if let Err(err) = pad.link(&sink_pad) {
            warn!("rtspsrc → decodebin link failed: {err}");
        }
    });

    let pipeline_weak = pipeline.downgrade();
    let queue_weak = queue.downgrade();
    let max_width_link = max_width;
    let max_height_link = max_height;
    decode.connect_pad_added(move |_dbin, src_pad| {
        if let Some(caps) = src_pad.current_caps() {
            if crate::gst_link::is_audio_caps(&caps) {
                return;
            }
        }

        let caps = src_pad
            .current_caps()
            .unwrap_or_else(|| src_pad.query_caps(None));
        let need_caps_notify = caps.is_any() || src_pad.current_caps().is_none();
        if need_caps_notify {
            let pipeline_weak = pipeline_weak.clone();
            let queue_weak = queue_weak.clone();
            src_pad.connect_notify(Some("caps"), move |pad, _| {
                let Some(pipeline) = pipeline_weak.upgrade() else {
                    return;
                };
                let Some(queue) = queue_weak.upgrade() else {
                    return;
                };
                crate::gst_link::link_decodebin_video_pad(
                    &pipeline,
                    pad,
                    &queue,
                    max_width_link,
                    max_height_link,
                );
            });
            return;
        }

        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        let Some(queue) = queue_weak.upgrade() else {
            return;
        };
        crate::gst_link::link_decodebin_video_pad(
            &pipeline,
            src_pad,
            &queue,
            max_width_link,
            max_height_link,
        );
    });

    let appsink = sink
        .dynamic_cast::<AppSink>()
        .map_err(|_| anyhow!("appsink cast"))?;

    let frame_store = Arc::clone(&slot.frame);
    let latest_seq = Arc::clone(&slot.latest_seq);
    let counters = Arc::clone(&slot.counters);
    let seq = Arc::clone(seq);
    appsink.set_callbacks(
        AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                let info = gstreamer_video::VideoInfo::from_caps(caps)
                    .map_err(|_| gst::FlowError::Error)?;
                let t0 = Instant::now();
                // Avoid an extra full-buffer copy before reading plane data.
                let frame =
                    gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                        .map_err(|_| gst::FlowError::Error)?;
                let width = frame.width();
                let height = frame.height();
                let src = frame
                    .plane_data(0)
                    .map_err(|_| gst::FlowError::Error)?;
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
                counters.frames_out.fetch_add(1, Ordering::Relaxed);
                counters.bytes_copied.fetch_add(bytes, Ordering::Relaxed);
                counters.copy_ns.fetch_add(copy_ns, Ordering::Relaxed);
                counters.width.store(width, Ordering::Relaxed);
                counters.height.store(height, Ordering::Relaxed);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    let counters_dec = Arc::clone(&slot.counters);
    let cam_for_dec = slot.camera_id.clone();
    pipeline.connect_deep_element_added(move |_bin, _sub_bin, element| {
        let Some(factory) = element.factory() else {
            return;
        };
        let name = factory.name().to_string();
        let lname = name.to_ascii_lowercase();
        if lname.contains("decodebin") || !lname.contains("dec") {
            return;
        }
        *counters_dec.decoder.lock() = name.clone();
        info!(camera = %cam_for_dec, decoder = %name, "decode element selected");
    });

    pipeline
        .set_state(gst::State::Playing)
        .context("set pipeline Playing")?;

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

