//! Listen to RTSP audio on the selected camera (toolbar volume toggle).

use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenTarget {
    pub id: String,
    pub url: String,
    pub protocols: Option<String>,
}

/// Separate RTSP pipeline that plays audio only for one camera at a time.
pub struct ListenAudio {
    enabled: bool,
    current: Option<ListenTarget>,
    pipeline: Option<gst::Pipeline>,
}

impl ListenAudio {
    pub fn new() -> Self {
        Self {
            enabled: false,
            current: None,
            pipeline: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, on: bool) {
        if self.enabled == on {
            return;
        }
        self.enabled = on;
        if !on {
            self.stop();
            info!("camera audio muted");
        } else {
            info!("camera audio unmuted — select a camera to listen");
            if let Some(t) = self.current.clone() {
                if let Err(err) = self.start(&t) {
                    warn!(camera = %t.id, error = %err, "camera audio start failed");
                    self.stop();
                }
            }
        }
    }

    /// Play audio for `target` when unmuted; `None` stops playback.
    pub fn sync(&mut self, target: Option<ListenTarget>) {
        let same = match (&self.current, &target) {
            (Some(a), Some(b)) => a.id == b.id && a.url == b.url,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }

        self.stop();
        self.current = target.clone();
        if !self.enabled {
            return;
        }
        let Some(t) = target else {
            return;
        };
        if let Err(err) = self.start(&t) {
            warn!(camera = %t.id, error = %err, "camera audio start failed");
            self.stop();
        }
    }

    pub fn stop(&mut self) {
        if let Some(pipeline) = self.pipeline.take() {
            let _ = pipeline.set_state(gst::State::Null);
            if let Some(id) = self.current.as_ref().map(|t| t.id.as_str()) {
                debug!(camera = %id, "camera audio stopped");
            }
        }
    }

    fn start(&mut self, target: &ListenTarget) -> Result<()> {
        self.stop();

        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("rtspsrc")
            .name("listen_src")
            .property("location", &target.url)
            .property("latency", 400u32)
            .property("drop-on-latency", false)
            .property("do-retransmission", false)
            .property("do-rtsp-keep-alive", true)
            .property("timeout", 5_000_000u64)
            .property("tcp-timeout", 5_000_000u64)
            .build()
            .context("create listen rtspsrc")?;

        let protocols = target
            .protocols
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .unwrap_or("tcp");
        src.set_property_from_str("protocols", protocols);

        pipeline.add(&src)?;
        attach_bus_watch(&pipeline, &target.id);

        let pipeline_weak = pipeline.downgrade();
        let cam = target.id.clone();
        let audio_linked = Arc::new(AtomicBool::new(false));
        src.connect_pad_added(move |_src, pad| {
            let Some(pipeline) = pipeline_weak.upgrade() else {
                return;
            };
            try_link_listen_pad(&pipeline, pad, &cam, &audio_linked);
        });

        pipeline
            .set_state(gst::State::Playing)
            .context("listen pipeline Playing")?;

        info!(
            camera = %target.id,
            transport = protocols,
            url = %crate::redact::redact_url(&target.url),
            "listening to camera audio"
        );
        self.pipeline = Some(pipeline);
        Ok(())
    }
}

impl Drop for ListenAudio {
    fn drop(&mut self) {
        self.stop();
    }
}

fn attach_bus_watch(pipeline: &gst::Pipeline, camera: &str) {
    let cam = camera.to_string();
    let Some(bus) = pipeline.bus() else {
        return;
    };
    // Sync handler works without a GLib main loop (eframe has none).
    bus.set_sync_handler(move |_bus, msg| {
        use gst::MessageView;
        match msg.view() {
            MessageView::Error(err) => {
                warn!(
                    camera = %cam,
                    error = %err.error(),
                    debug = %err.debug().unwrap_or_default(),
                    "camera audio bus error"
                );
            }
            MessageView::Warning(w) => {
                debug!(
                    camera = %cam,
                    warning = %w.error(),
                    "camera audio bus warning"
                );
            }
            MessageView::Eos(_) => {
                warn!(camera = %cam, "camera audio EOS");
            }
            _ => {}
        }
        gst::BusSyncReply::Pass
    });
}

fn try_link_listen_pad(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    camera: &str,
    audio_linked: &Arc<AtomicBool>,
) {
    let caps = pad.current_caps();
    if caps.as_ref().is_none_or(|c| c.is_any()) {
        let pipeline_weak = pipeline.downgrade();
        let cam = camera.to_string();
        let audio_linked = Arc::clone(audio_linked);
        pad.connect_notify(Some("caps"), move |pad, _| {
            let Some(pipeline) = pipeline_weak.upgrade() else {
                return;
            };
            try_link_listen_pad(&pipeline, pad, &cam, &audio_linked);
        });
        return;
    }

    let caps = caps.unwrap();
    if !crate::gst_link::is_rtp_or_raw_audio_caps(&caps) {
        // Unlinked video pads stall rtspsrc — always sink them.
        if let Err(err) = link_to_fakesink(pipeline, pad, camera) {
            debug!(camera = %camera, error = %err, "listen fakesink link failed");
        }
        return;
    }

    if audio_linked.swap(true, Ordering::AcqRel) {
        debug!(camera = %camera, "extra audio pad → fakesink");
        let _ = link_to_fakesink(pipeline, pad, camera);
        return;
    }

    if let Err(err) = link_audio_pad(pipeline, pad, &caps, camera) {
        audio_linked.store(false, Ordering::Release);
        warn!(
            camera = %camera,
            error = %format!("{err:#}"),
            caps = %caps,
            "camera audio pad link failed"
        );
    }
}

fn link_to_fakesink(pipeline: &gst::Pipeline, pad: &gst::Pad, camera: &str) -> Result<()> {
    let sink = gst::ElementFactory::make("fakesink")
        .name(&format!("listen_fake_{}", pad.name()))
        .property("sync", false)
        .property("async", false)
        .build()
        .context("create fakesink")?;
    pipeline.add(&sink)?;
    let sink_pad = sink
        .static_pad("sink")
        .ok_or_else(|| anyhow!("fakesink missing sink"))?;
    pad.link(&sink_pad).context("link pad → fakesink")?;
    sink.sync_state_with_parent()
        .context("sync fakesink state")?;
    debug!(camera = %camera, pad = %pad.name(), "listen pad → fakesink");
    Ok(())
}

fn make_audio_sink() -> Result<(gst::Element, &'static str)> {
    // Prefer Pulse/PipeWire over raw ALSA so we hit the user's default device.
    for name in ["pulsesink", "pipewiresink", "autoaudiosink"] {
        if let Ok(sink) = gst::ElementFactory::make(name).name("listen_sink").build() {
            let _ = sink.set_property("sync", false);
            if name == "pulsesink" {
                sink.set_property_from_str("client-name", "Citadel CCTV");
            }
            return Ok((sink, name));
        }
    }
    Err(anyhow!(
        "no audio sink (need pulsesink / pipewiresink / autoaudiosink)"
    ))
}

fn link_audio_pad(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    caps: &gst::Caps,
    camera: &str,
) -> Result<()> {
    let encoding = rtp_encoding_name(caps).unwrap_or_default();
    // payload 0 = PCMU, 8 = PCMA when encoding-name is missing
    let encoding = if encoding.is_empty() {
        match rtp_payload_type(caps) {
            Some(0) => "PCMU".to_string(),
            Some(8) => "PCMA".to_string(),
            _ => encoding,
        }
    } else {
        encoding
    };

    let (depay_name, dec_name) = audio_chain_for_encoding(&encoding)
        .ok_or_else(|| anyhow!("unsupported RTSP audio encoding {encoding:?} (need PCMU/PCMA)"))?;

    let (sink, sink_name) = make_audio_sink()?;

    info!(
        camera = %camera,
        encoding = %encoding,
        depay = depay_name,
        decoder = dec_name,
        sink = sink_name,
        "linking camera audio"
    );

    let depay = gst::ElementFactory::make(depay_name)
        .name("listen_depay")
        .build()
        .with_context(|| format!("create {depay_name}"))?;
    let dec = gst::ElementFactory::make(dec_name)
        .name("listen_dec")
        .build()
        .with_context(|| format!("create {dec_name}"))?;
    let convert = gst::ElementFactory::make("audioconvert")
        .name("listen_aconv")
        .build()
        .context("create audioconvert")?;
    let resample = gst::ElementFactory::make("audioresample")
        .name("listen_ares")
        .build()
        .context("create audioresample")?;
    // G.711 intercoms are often very quiet on desktop speakers.
    let volume = gst::ElementFactory::make("volume")
        .name("listen_vol")
        .property("volume", 6.0_f64)
        .build()
        .context("create volume")?;

    pipeline.add_many([&depay, &dec, &convert, &resample, &volume, &sink])?;
    gst::Element::link_many([&depay, &dec, &convert, &resample, &volume, &sink])
        .context("link audio chain")?;

    let sink_pad = depay
        .static_pad("sink")
        .ok_or_else(|| anyhow!("{depay_name} missing sink"))?;
    pad.link(&sink_pad).context("link rtspsrc → audio depay")?;

    for el in [&depay, &dec, &convert, &resample, &volume, &sink] {
        el.sync_state_with_parent()
            .context("sync audio element state")?;
    }

    info!(
        camera = %camera,
        encoding = %encoding,
        sink = sink_name,
        "camera audio playing"
    );
    Ok(())
}

fn rtp_encoding_name(caps: &gst::Caps) -> Option<String> {
    let s = caps.structure(0)?;
    s.get::<String>("encoding-name")
        .ok()
        .or_else(|| s.get::<&str>("encoding-name").ok().map(|v| v.to_string()))
}

fn rtp_payload_type(caps: &gst::Caps) -> Option<i32> {
    let s = caps.structure(0)?;
    s.get::<i32>("payload")
        .ok()
        .or_else(|| s.get::<u32>("payload").ok().map(|v| v as i32))
}

fn audio_chain_for_encoding(encoding: &str) -> Option<(&'static str, &'static str)> {
    match encoding.trim().to_ascii_uppercase().as_str() {
        "PCMU" | "G711U" | "G.711U" => Some(("rtppcmudepay", "mulawdec")),
        "PCMA" | "G711A" | "G.711A" => Some(("rtppcmadepay", "alawdec")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn detects_rtp_pcmu_caps() {
        let _ = gst::init();
        let caps = gst::Caps::from_str(
            "application/x-rtp, media=(string)audio, payload=(int)0, encoding-name=(string)PCMU, clock-rate=(int)8000",
        )
        .unwrap();
        assert!(crate::gst_link::is_rtp_or_raw_audio_caps(&caps));
        assert_eq!(rtp_encoding_name(&caps).as_deref(), Some("PCMU"));
        assert_eq!(
            audio_chain_for_encoding("PCMU"),
            Some(("rtppcmudepay", "mulawdec"))
        );
    }
}
