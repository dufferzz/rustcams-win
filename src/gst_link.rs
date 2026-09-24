//! Video pad linking: explicit depay/parse/decode and optional D3D11 postproc.

use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use parking_lot::Mutex;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{debug, warn};

use crate::gst_env::{cuda_postproc_available, d3d11_postproc_available};

pub(crate) fn is_audio_caps(caps: &gst::Caps) -> bool {
    caps.structure(0)
        .map(|s| s.name().starts_with("audio/"))
        .unwrap_or(false)
}

/// RTSP audio pads are usually `application/x-rtp, media=(string)audio`, not `audio/*`.
pub(crate) fn is_rtp_or_raw_audio_caps(caps: &gst::Caps) -> bool {
    let Some(s) = caps.structure(0) else {
        return false;
    };
    if s.name().starts_with("audio/") {
        return true;
    }
    if s.name() != "application/x-rtp" {
        return false;
    }
    s.get::<String>("media")
        .ok()
        .or_else(|| s.get::<&str>("media").ok().map(|v| v.to_string()))
        .is_some_and(|m| m.eq_ignore_ascii_case("audio"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoCodec {
    H264,
    H265,
}

/// How to pick the H.264/H.265 decoder. Settings persist this; `RUSTCAMS_DECODE`
/// overrides when set (`sw`, `nvdec`, `hw`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecodeBackend {
    #[default]
    Software,
    Nvdec,
    Hardware,
}

impl DecodeBackend {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sw" | "software" | "avdec" => Some(Self::Software),
            "nv" | "nvdec" | "nvidia" | "nvcodec" => Some(Self::Nvdec),
            "hw" | "hardware" | "d3d11" | "auto" => Some(Self::Hardware),
            _ => None,
        }
    }

    pub fn from_env() -> Option<Self> {
        std::env::var("RUSTCAMS_DECODE")
            .ok()
            .as_deref()
            .and_then(Self::parse)
    }

    /// Env var wins when set; otherwise the saved settings value.
    pub fn resolve(settings: Self) -> Self {
        Self::from_env().unwrap_or(settings)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Software => "software",
            Self::Nvdec => "nvdec",
            Self::Hardware => "hardware",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Software => "Software",
            Self::Nvdec => "NVDEC",
            Self::Hardware => "Auto hardware",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Software => "libav CPU decode (avdec). Default; usually smoothest on mixed GPUs.",
            Self::Nvdec => "NVIDIA NVDEC (nvh264dec / nvh265dec). Needs gst-plugin-nvcodec.",
            Self::Hardware => {
                "Try D3D11, Media Foundation, NVDEC, then V4L2; fall back to software."
            }
        }
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

/// Link rtspsrc video pad → depay → parse → decode → (optional GPU postproc) → queue.
pub(crate) fn link_explicit_video(
    pipeline: &gst::Pipeline,
    src_pad: &gst::Pad,
    queue: &gst::Element,
    max_width: i32,
    max_height: i32,
    decoder_name_out: &Arc<Mutex<String>>,
    backend: DecodeBackend,
    camera: &str,
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
        debug!(camera = %camera, caps = %caps, "skipping RTSP audio pad");
        return Ok(());
    }

    let codec = codec_from_rtp_caps(&caps).ok_or_else(|| {
        anyhow!(
            "unsupported RTSP video caps (need H264/H265): {}",
            caps.to_string()
        )
    })?;

    let (depay, parse, decoder, used_hw) = match codec {
        VideoCodec::H264 => build_h264_chain(backend)?,
        VideoCodec::H265 => build_h265_chain(backend)?,
    };

    if let Some(factory) = decoder.factory() {
        let name = factory.name().to_string();
        *decoder_name_out.lock() = name.clone();
        debug!(
            camera = %camera,
            decoder = %name,
            codec = ?codec,
            hw = used_hw,
            backend = backend.as_str(),
            "explicit decode chain"
        );
    }

    pipeline.add_many([&depay, &parse, &decoder])?;
    gst::Element::link_many([&depay, &parse, &decoder]).context("link depay → decoder")?;

    let depay_sink = depay
        .static_pad("sink")
        .ok_or_else(|| anyhow!("depay sink pad"))?;
    src_pad.link(&depay_sink).context("link rtspsrc → depay")?;

    if used_hw && decoder_is_nv(&decoder) && cuda_postproc_available() {
        match link_cuda_postproc(pipeline, &decoder, &queue_sink, max_width, max_height) {
            Ok(()) => {
                debug!(
                    camera = %camera,
                    max_width,
                    max_height,
                    "linked explicit decode via CUDA convert/scale/download"
                );
            }
            Err(err) => {
                warn!(
                    camera = %camera,
                    error = %format!("{err:#}"),
                    "CUDA post-process link failed, using CPU after NVDEC"
                );
                link_decoder_to_queue(&decoder, &queue_sink)?;
            }
        }
    } else if used_hw && d3d11_postproc_available() {
        match link_d3d11_postproc(pipeline, &decoder, &queue_sink, max_width, max_height) {
            Ok(()) => {
                debug!(
                    camera = %camera,
                    max_width,
                    max_height,
                    "linked explicit decode via D3D11 convert/scale/download"
                );
            }
            Err(err) => {
                warn!(
                    camera = %camera,
                    error = %format!("{err:#}"),
                    "D3D11 post-process link failed, using CPU after HW decode"
                );
                link_decoder_to_queue(&decoder, &queue_sink)?;
            }
        }
    } else {
        link_decoder_to_queue(&decoder, &queue_sink)?;
        debug!(camera = %camera, "linked explicit decode (CPU path)");
    }

    for el in [&depay, &parse, &decoder] {
        el.sync_state_with_parent()
            .context("sync explicit decode element state")?;
    }
    Ok(())
}

fn build_h264_chain(
    backend: DecodeBackend,
) -> Result<(gst::Element, gst::Element, gst::Element, bool)> {
    let depay = gst::ElementFactory::make("rtph264depay")
        .name("depay")
        .build()
        .context("create rtph264depay")?;
    let parse = gst::ElementFactory::make("h264parse")
        .name("parse")
        .build()
        .context("create h264parse")?;
    let (decoder, hw) = make_decoder(backend, VideoCodec::H264)?;
    Ok((depay, parse, decoder, hw))
}

fn build_h265_chain(
    backend: DecodeBackend,
) -> Result<(gst::Element, gst::Element, gst::Element, bool)> {
    let depay = gst::ElementFactory::make("rtph265depay")
        .name("depay")
        .build()
        .context("create rtph265depay")?;
    let parse = gst::ElementFactory::make("h265parse")
        .name("parse")
        .build()
        .context("create h265parse")?;
    let (decoder, hw) = make_decoder(backend, VideoCodec::H265)?;
    Ok((depay, parse, decoder, hw))
}

fn hw_decoder_names(backend: DecodeBackend, codec: VideoCodec) -> &'static [&'static str] {
    match (backend, codec) {
        (DecodeBackend::Software, _) => &[],
        (DecodeBackend::Nvdec, VideoCodec::H264) => &["nvh264dec", "nvcudah264dec"],
        (DecodeBackend::Nvdec, VideoCodec::H265) => &["nvh265dec", "nvcudah265dec"],
        (DecodeBackend::Hardware, VideoCodec::H264) => &[
            "d3d11h264dec",
            "mfh264dec",
            "nvh264dec",
            "nvcudah264dec",
            "v4l2slh264dec",
            "v4l2h264dec",
        ],
        (DecodeBackend::Hardware, VideoCodec::H265) => &[
            "d3d11h265dec",
            "mfh265dec",
            "nvh265dec",
            "nvcudah265dec",
            "v4l2slh265dec",
            "v4l2h265dec",
        ],
    }
}

fn sw_decoder_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "avdec_h264",
        VideoCodec::H265 => "avdec_h265",
    }
}

fn make_decoder(backend: DecodeBackend, codec: VideoCodec) -> Result<(gst::Element, bool)> {
    let sw_name = sw_decoder_name(codec);
    for name in hw_decoder_names(backend, codec) {
        if let Ok(el) = gst::ElementFactory::make(name).name("dec").build() {
            return Ok((el, true));
        }
    }
    if backend == DecodeBackend::Nvdec {
        warn!("NVDEC requested but nvh264/nvh265 decoder missing; using {sw_name}");
    }
    let el = gst::ElementFactory::make(sw_name)
        .name("dec")
        .build()
        .with_context(|| format!("create {sw_name}"))?;
    Ok((el, false))
}

fn decoder_is_nv(decoder: &gst::Element) -> bool {
    decoder
        .factory()
        .map(|f| {
            let n = f.name();
            n.starts_with("nv") || n.starts_with("cuda")
        })
        .unwrap_or(false)
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

fn link_cuda_postproc(
    pipeline: &gst::Pipeline,
    decoder: &gst::Element,
    queue_sink: &gst::Pad,
    max_width: i32,
    max_height: i32,
) -> Result<()> {
    let download = gst::ElementFactory::make("cudadownload")
        .name("cudadl")
        .build()
        .context("create cudadownload")?;

    let have_gpu_scale = gst::ElementFactory::find("cudaconvert").is_some()
        && gst::ElementFactory::find("cudascale").is_some();

    if have_gpu_scale {
        let convert = gst::ElementFactory::make("cudaconvert")
            .name("cudaconv")
            .build()
            .context("create cudaconvert")?;
        let scale = gst::ElementFactory::make("cudascale")
            .name("cudascale")
            .build()
            .context("create cudascale")?;
        let cuda_caps = gst::Caps::from_str(&format!(
            "video/x-raw(memory:CUDAMemory),format=RGBA,width=(int)[1,{max_width}],height=(int)[1,{max_height}]"
        ))
        .context("parse CUDA scale caps")?;
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name("cudacaps")
            .property("caps", cuda_caps)
            .build()
            .context("create CUDA capsfilter")?;

        pipeline.add_many([&convert, &scale, &capsfilter, &download])?;
        gst::Element::link_many([&convert, &scale, &capsfilter, &download])
            .context("link cudaconvert → cudadownload")?;

        let dec_src = decoder
            .static_pad("src")
            .ok_or_else(|| anyhow!("decoder src pad"))?;
        let conv_sink = convert
            .static_pad("sink")
            .ok_or_else(|| anyhow!("cudaconvert sink pad"))?;
        dec_src
            .link(&conv_sink)
            .context("link decoder → cudaconvert")?;

        let dl_src = download
            .static_pad("src")
            .ok_or_else(|| anyhow!("cudadownload src pad"))?;
        dl_src
            .link(queue_sink)
            .context("link cudadownload → queue")?;

        for el in [&convert, &scale, &capsfilter, &download] {
            el.sync_state_with_parent()
                .context("sync CUDA element state")?;
        }
        return Ok(());
    }

    pipeline.add(&download)?;
    let dec_src = decoder
        .static_pad("src")
        .ok_or_else(|| anyhow!("decoder src pad"))?;
    let dl_sink = download
        .static_pad("sink")
        .ok_or_else(|| anyhow!("cudadownload sink pad"))?;
    dec_src
        .link(&dl_sink)
        .context("link decoder → cudadownload")?;
    let dl_src = download
        .static_pad("src")
        .ok_or_else(|| anyhow!("cudadownload src pad"))?;
    dl_src
        .link(queue_sink)
        .context("link cudadownload → queue")?;
    download
        .sync_state_with_parent()
        .context("sync CUDA element state")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::DecodeBackend;

    #[test]
    fn parse_decode_backend_aliases() {
        assert_eq!(DecodeBackend::parse("sw"), Some(DecodeBackend::Software));
        assert_eq!(DecodeBackend::parse("NVDEC"), Some(DecodeBackend::Nvdec));
        assert_eq!(DecodeBackend::parse("nvidia"), Some(DecodeBackend::Nvdec));
        assert_eq!(DecodeBackend::parse("hw"), Some(DecodeBackend::Hardware));
        assert_eq!(DecodeBackend::parse("nope"), None);
    }
}
