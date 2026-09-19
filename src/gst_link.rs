//! Video pad linking: explicit depay/parse/decode and optional D3D11 postproc.

use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use parking_lot::Mutex;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::gst_env::d3d11_postproc_available;

pub(crate) fn is_audio_caps(caps: &gst::Caps) -> bool {
    caps.structure(0)
        .map(|s| s.name().starts_with("audio/"))
        .unwrap_or(false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoCodec {
    H264,
    H265,
}

/// Prefer software decode (`avdec_*`). Default on for stutter A/B vs D3D11;
/// set `RUSTCAMS_DECODE=hw` to force hardware (D3D11/MF/NV/V4L2).
pub(crate) fn prefer_software_decode() -> bool {
    match std::env::var("RUSTCAMS_DECODE") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !(v == "hw" || v == "hardware" || v == "d3d11")
        }
        Err(_) => true,
    }
}

pub(crate) fn codec_from_rtp_caps(caps: &gst::Caps) -> Option<VideoCodec> {
    let s = caps.structure(0)?;
    let encoding = s
        .get::<String>("encoding-name")
        .ok()
        .or_else(|| s.get::<&str>("encoding-name").ok().map(|v| v.to_string()))?;
    match encoding.to_ascii_uppercase().as_str() {
        "H264" => Some(VideoCodec::H264),
        "H265" | "HEVC" => Some(VideoCodec::H265),
        _ => None,
    }
}

/// Link rtspsrc video pad → depay → parse → decoder → (optional D3D11) → queue.
pub(crate) fn link_explicit_video(
    pipeline: &gst::Pipeline,
    src_pad: &gst::Pad,
    queue: &gst::Element,
    max_width: i32,
    max_height: i32,
    decoder_name_out: &Arc<Mutex<String>>,
) -> Result<()> {
    let Some(queue_sink) = queue.static_pad("sink") else {
        return Err(anyhow!("queue missing sink pad"));
    };
    if queue_sink.is_linked() || src_pad.is_linked() {
        return Ok(());
    }

    let caps = src_pad
        .current_caps()
        .unwrap_or_else(|| src_pad.query_caps(None));
    if is_audio_caps(&caps) {
        return Ok(());
    }

    let codec = codec_from_rtp_caps(&caps).ok_or_else(|| {
        anyhow!(
            "unsupported RTSP video caps (need H264/H265): {}",
            caps.to_string()
        )
    })?;

    let force_sw = prefer_software_decode();
    let (depay, parse, decoder, used_hw) = match codec {
        VideoCodec::H264 => build_h264_chain(force_sw)?,
        VideoCodec::H265 => build_h265_chain(force_sw)?,
    };

    if let Some(factory) = decoder.factory() {
        let name = factory.name().to_string();
        *decoder_name_out.lock() = name.clone();
        info!(
            decoder = %name,
            codec = ?codec,
            hw = used_hw,
            "explicit decode chain"
        );
    }

    pipeline.add_many([&depay, &parse, &decoder])?;
    gst::Element::link_many([&depay, &parse, &decoder]).context("link depay → decoder")?;

    let depay_sink = depay
        .static_pad("sink")
        .ok_or_else(|| anyhow!("depay sink pad"))?;
    src_pad.link(&depay_sink).context("link rtspsrc → depay")?;

    if used_hw && d3d11_postproc_available() {
        match link_d3d11_postproc(pipeline, &decoder, &queue_sink, max_width, max_height) {
            Ok(()) => {
                info!(
                    max_width,
                    max_height, "linked explicit decode via D3D11 convert/scale/download"
                );
            }
            Err(err) => {
                warn!("D3D11 post-process link failed, using CPU after HW decode: {err:#}");
                link_decoder_to_queue(&decoder, &queue_sink)?;
            }
        }
    } else {
        link_decoder_to_queue(&decoder, &queue_sink)?;
        debug!("linked explicit decode (CPU path)");
    }

    for el in [&depay, &parse, &decoder] {
        el.sync_state_with_parent()
            .context("sync explicit decode element state")?;
    }
    Ok(())
}

fn build_h264_chain(force_sw: bool) -> Result<(gst::Element, gst::Element, gst::Element, bool)> {
    let depay = gst::ElementFactory::make("rtph264depay")
        .name("depay")
        .build()
        .context("create rtph264depay")?;
    let parse = gst::ElementFactory::make("h264parse")
        .name("parse")
        .build()
        .context("create h264parse")?;
    let (decoder, hw) = make_decoder(
        force_sw,
        &[
            "d3d11h264dec",
            "mfh264dec",
            "nvh264dec",
            "v4l2slh264dec",
            "v4l2h264dec",
        ],
        "avdec_h264",
    )?;
    Ok((depay, parse, decoder, hw))
}

fn build_h265_chain(force_sw: bool) -> Result<(gst::Element, gst::Element, gst::Element, bool)> {
    let depay = gst::ElementFactory::make("rtph265depay")
        .name("depay")
        .build()
        .context("create rtph265depay")?;
    let parse = gst::ElementFactory::make("h265parse")
        .name("parse")
        .build()
        .context("create h265parse")?;
    let (decoder, hw) = make_decoder(
        force_sw,
        &[
            "d3d11h265dec",
            "mfh265dec",
            "nvh265dec",
            "v4l2slh265dec",
            "v4l2h265dec",
        ],
        "avdec_h265",
    )?;
    Ok((depay, parse, decoder, hw))
}

fn make_decoder(force_sw: bool, hw_names: &[&str], sw_name: &str) -> Result<(gst::Element, bool)> {
    if !force_sw {
        for name in hw_names {
            if let Ok(el) = gst::ElementFactory::make(name).name("dec").build() {
                return Ok((el, true));
            }
        }
    }
    let el = gst::ElementFactory::make(sw_name)
        .name("dec")
        .build()
        .with_context(|| format!("create {sw_name}"))?;
    Ok((el, false))
}

fn link_decoder_to_queue(decoder: &gst::Element, queue_sink: &gst::Pad) -> Result<()> {
    let dec_src = decoder
        .static_pad("src")
        .ok_or_else(|| anyhow!("decoder src pad"))?;
    dec_src.link(queue_sink).context("link decoder → queue")?;
    Ok(())
}

fn link_d3d11_postproc(
    pipeline: &gst::Pipeline,
    decoder: &gst::Element,
    queue_sink: &gst::Pad,
    max_width: i32,
    max_height: i32,
) -> Result<()> {
    let convert = gst::ElementFactory::make("d3d11convert")
        .name("d3d11conv")
        .build()
        .context("create d3d11convert")?;
    let scale = gst::ElementFactory::make("d3d11scale")
        .name("d3d11scale")
        .build()
        .context("create d3d11scale")?;
    // Scale + NV12→RGBA on the GPU, then download small RGBA frames.
    // Downstream videoconvert/videoscale become cheap passthroughs.
    let d3d_caps = gst::Caps::from_str(&format!(
        "video/x-raw(memory:D3D11Memory),format=RGBA,width=(int)[1,{max_width}],height=(int)[1,{max_height}]"
    ))
    .context("parse D3D11 scale caps")?;
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .name("d3d11caps")
        .property("caps", d3d_caps)
        .build()
        .context("create d3d11 capsfilter")?;
    let download = gst::ElementFactory::make("d3d11download")
        .name("d3d11dl")
        .build()
        .context("create d3d11download")?;

    pipeline.add_many([&convert, &scale, &capsfilter, &download])?;
    gst::Element::link_many([&convert, &scale, &capsfilter, &download])
        .context("link d3d11convert → d3d11download")?;

    let dec_src = decoder
        .static_pad("src")
        .ok_or_else(|| anyhow!("decoder src pad"))?;
    let conv_sink = convert
        .static_pad("sink")
        .ok_or_else(|| anyhow!("d3d11convert sink pad"))?;
    dec_src
        .link(&conv_sink)
        .context("link decoder → d3d11convert")?;

    let dl_src = download
        .static_pad("src")
        .ok_or_else(|| anyhow!("d3d11download src pad"))?;
    dl_src
        .link(queue_sink)
        .context("link d3d11download → queue")?;

    for el in [&convert, &scale, &capsfilter, &download] {
        el.sync_state_with_parent()
            .context("sync D3D11 element state")?;
    }
    Ok(())
}
