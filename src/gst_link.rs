//! Decodebin pad linking (CPU and D3D11 post-process paths).

use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::str::FromStr;
use tracing::{debug, info, warn};

use crate::gst_env::d3d11_postproc_available;

pub(crate) fn is_audio_caps(caps: &gst::Caps) -> bool {
    caps.structure(0)
        .map(|s| s.name().starts_with("audio/"))
        .unwrap_or(false)
}

pub(crate) fn link_decodebin_video_pad(
    pipeline: &gst::Pipeline,
    src_pad: &gst::Pad,
    queue: &gst::Element,
    max_width: i32,
    max_height: i32,
) {
    let Some(queue_sink) = queue.static_pad("sink") else {
        return;
    };
    if queue_sink.is_linked() || src_pad.is_linked() {
        return;
    }

    let caps = src_pad
        .current_caps()
        .unwrap_or_else(|| src_pad.query_caps(None));
    if is_audio_caps(&caps) || caps.is_any() {
        return;
    }

    // Always try D3D11 first when plugins exist. Linking to d3d11convert
    // pulls DXVA decoders into memory:D3D11Memory; waiting for that feature
    // on the pad first often left us on the CPU NV12→RGBA path instead.
    if d3d11_postproc_available() {
        match link_d3d11_postproc(pipeline, src_pad, &queue_sink, max_width, max_height) {
            Ok(()) => {
                info!(
                    max_width,
                    max_height, "linked decodebin via D3D11 convert/scale/download"
                );
                return;
            }
            Err(err) => {
                warn!("D3D11 post-process link failed, using CPU path: {err:#}");
            }
        }
    }

    if let Err(err) = src_pad.link(&queue_sink) {
        warn!("decodebin → queue link failed: {err}");
    } else {
        debug!("linked decodebin video pad (CPU path)");
    }
}

fn link_d3d11_postproc(
    pipeline: &gst::Pipeline,
    src_pad: &gst::Pad,
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

    let conv_sink = convert
        .static_pad("sink")
        .ok_or_else(|| anyhow!("d3d11convert sink pad"))?;
    src_pad
        .link(&conv_sink)
        .context("link decodebin → d3d11convert")?;

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
