use crate::config::{NvrConfig, StreamType};
use anyhow::{bail, Context, Result};
use digest_auth::{AuthContext, HttpMethod};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::time::Duration;
use tracing::{debug, warn};
use ureq::Agent;

/// One digital (IP proxy) channel discovered on the NVR.
#[derive(Debug, Clone)]
pub struct DiscoveredChannel {
    pub channel_id: u32,
    pub name: String,
    pub source_ip: Option<String>,
    /// Camera-side input port when present (multi-lens devices).
    pub src_input_port: Option<u32>,
}

/// Fetch InputProxy channels via ISAPI (HTTP digest auth).
pub fn list_input_proxy_channels(nvr: &NvrConfig) -> Result<Vec<DiscoveredChannel>> {
    let path = "/ISAPI/ContentMgmt/InputProxy/channels";
    let url = format!("http://{}:{}{}", nvr.host, nvr.http_port, path);

    let agent: Agent = Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into();

    let challenge = agent
        .get(&url)
        .call()
        .with_context(|| format!("NVR probe GET {url}"))?;

    let status = challenge.status();
    if status != 401 {
        bail!(
            "NVR digest challenge expected HTTP 401 from {}, got {}",
            url,
            status
        );
    }

    let www = challenge
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow::anyhow!("NVR 401 missing WWW-Authenticate header"))?
        .to_string();

    let mut prompt = digest_auth::parse(&www).context("parse WWW-Authenticate")?;
    let context = AuthContext::new_with_method(
        &nvr.username,
        &nvr.password,
        path,
        Option::<&[u8]>::None,
        HttpMethod::GET,
    );
    let answer = prompt
        .respond(&context)
        .context("compute digest Authorization")?
        .to_string();

    let mut response = agent
        .get(&url)
        .header("Authorization", &answer)
        .call()
        .with_context(|| format!("NVR authenticated GET {url}"))?;

    let status = response.status();
    if !(200..300).contains(&status.as_u16()) {
        bail!("NVR InputProxy list failed: HTTP {status}");
    }

    let body = response
        .body_mut()
        .read_to_string()
        .context("read InputProxy XML body")?;

    debug!(
        bytes = body.len(),
        "fetched InputProxy/channels from {}",
        nvr.host
    );
    parse_input_proxy_channels(&body)
}

/// Encode userinfo so `@ : / ? #` don't break the authority, but leave
/// typical Hikvision password punctuation (e.g. `!`) readable for rtspsrc.
const RTSP_USERINFO: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b':')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']');

/// Build an RTSP URL for a channel on the NVR.
pub fn build_rtsp_url(nvr: &NvrConfig, channel_id: u32, stream: StreamType) -> String {
    let user = utf8_percent_encode(&nvr.username, RTSP_USERINFO);
    let pass = utf8_percent_encode(&nvr.password, RTSP_USERINFO);
    let stream_id = channel_id * 100 + stream.as_digit();
    format!(
        "rtsp://{user}:{pass}@{}:{}/Streaming/Channels/{stream_id}",
        nvr.host, nvr.rtsp_port
    )
}

/// Direct-to-camera RTSP URL (LAN host, typically port 554).
pub fn build_camera_rtsp_url(
    host: &str,
    username: &str,
    password: &str,
    input_port: u32,
    stream: StreamType,
) -> String {
    let user = utf8_percent_encode(username, RTSP_USERINFO);
    let pass = utf8_percent_encode(password, RTSP_USERINFO);
    let stream_id = input_port.max(1) * 100 + stream.as_digit();
    format!("rtsp://{user}:{pass}@{host}:554/Streaming/Channels/{stream_id}")
}

/// Rewrite a Hikvision `/Streaming/Channels/N` URL to main (…1) or sub (…2).
pub fn rewrite_stream_digit(rtsp_url: &str, stream: StreamType) -> String {
    const MARKER: &str = "/Streaming/Channels/";
    let Some(idx) = rtsp_url.find(MARKER) else {
        return rtsp_url.to_string();
    };
    let prefix = &rtsp_url[..idx + MARKER.len()];
    let rest = &rtsp_url[idx + MARKER.len()..];
    let (digits, suffix) = match rest.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if digits.is_empty() {
        return rtsp_url.to_string();
    }
    let mut channel = digits.to_string();
    if let Some(last) = channel.pop() {
        if last.is_ascii_digit() {
            channel.push(char::from_digit(stream.as_digit(), 10).unwrap_or('1'));
            return format!("{prefix}{channel}{suffix}");
        }
        channel.push(last);
    }
    rtsp_url.to_string()
}

pub fn parse_input_proxy_channels(xml: &str) -> Result<Vec<DiscoveredChannel>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut channels = Vec::new();
    let mut buf = Vec::new();
    let mut path: Vec<String> = Vec::new();

    let mut in_channel = false;
    let mut cur_id: Option<u32> = None;
    let mut cur_name: Option<String> = None;
    let mut cur_ip: Option<String> = None;
    let mut cur_port: Option<u32> = None;
    let mut disabled = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name(e.name().as_ref());
                if local == "InputProxyChannel" {
                    in_channel = true;
                    cur_id = None;
                    cur_name = None;
                    cur_ip = None;
                    cur_port = None;
                    disabled = false;
                }
                path.push(local);
            }
            Ok(Event::End(e)) => {
                let local = local_name(e.name().as_ref());
                if local == "InputProxyChannel" && in_channel {
                    match cur_id {
                        Some(id) if disabled => {
                            debug!(channel = id, "skipping disabled InputProxy channel");
                        }
                        Some(id) => {
                            channels.push(DiscoveredChannel {
                                channel_id: id,
                                name: cur_name
                                    .clone()
                                    .unwrap_or_else(|| format!("Channel {id}")),
                                source_ip: cur_ip.clone(),
                                src_input_port: cur_port,
                            });
                        }
                        None => warn!("InputProxyChannel missing <id>; skipped"),
                    }
                    in_channel = false;
                }
                if path.last().map(|s| s.as_str()) == Some(local.as_str()) {
                    path.pop();
                }
            }
            Ok(Event::Text(t)) => {
                if !in_channel || path.is_empty() {
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
                    (Some("InputProxyChannel"), "id") => {
                        cur_id = text.parse().ok();
                    }
                    (Some("InputProxyChannel"), "name") => {
                        cur_name = Some(text);
                    }
                    (_, "ipAddress" | "ipv4Address") if cur_ip.is_none() => {
                        cur_ip = Some(text);
                    }
                    (_, "srcInputPort") => {
                        cur_port = text.parse().ok();
                    }
                    (_, "enableVideo" | "adminEnable") => {
                        if matches!(text.as_str(), "false" | "0") {
                            disabled = true;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => bail!("XML parse error at {}: {e}", reader.buffer_position()),
            _ => {}
        }
        buf.clear();
    }

    if channels.is_empty() {
        bail!("InputProxy/channels returned no usable channels");
    }
    Ok(channels)
}

fn local_name(qname: &[u8]) -> String {
    let s = String::from_utf8_lossy(qname);
    s.rsplit('}')
        .next()
        .unwrap_or(&s)
        .rsplit(':')
        .next()
        .unwrap_or(&s)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_input_proxy_list() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<InputProxyChannelList version="2.0" xmlns="http://www.hikvision.com/ver20/XMLSchema">
  <InputProxyChannel>
    <id>1</id>
    <name>Front</name>
    <sourceInputPortDescriptor>
      <ipAddress>192.0.2.10</ipAddress>
      <srcInputPort>1</srcInputPort>
    </sourceInputPortDescriptor>
    <enableVideo>true</enableVideo>
  </InputProxyChannel>
  <InputProxyChannel>
    <id>2</id>
    <name>Hall</name>
    <sourceInputPortDescriptor>
      <ipAddress>192.0.2.20</ipAddress>
      <srcInputPort>1</srcInputPort>
    </sourceInputPortDescriptor>
    <enableVideo>false</enableVideo>
  </InputProxyChannel>
</InputProxyChannelList>"#;

        let chans = parse_input_proxy_channels(xml).unwrap();
        assert_eq!(chans.len(), 1);
        assert_eq!(chans[0].channel_id, 1);
        assert_eq!(chans[0].name, "Front");
        assert_eq!(chans[0].source_ip.as_deref(), Some("192.0.2.10"));
        assert_eq!(chans[0].src_input_port, Some(1));
    }

    #[test]
    fn builds_rtsp_url_with_encoded_password() {
        let nvr = NvrConfig {
            host: "198.51.100.20".into(),
            http_port: 49000,
            rtsp_port: 554,
            username: "admin".into(),
            password: "Secret!".into(),
            stream: StreamType::Sub,
            protocols: None,
        };
        let url = build_rtsp_url(&nvr, 3, StreamType::Sub);
        assert_eq!(
            url,
            "rtsp://admin:Secret!@198.51.100.20:554/Streaming/Channels/302"
        );
    }

    #[test]
    fn rewrites_to_main_stream() {
        let sub = "rtsp://admin:x@192.0.2.10:554/Streaming/Channels/102";
        assert_eq!(
            rewrite_stream_digit(sub, StreamType::Main),
            "rtsp://admin:x@192.0.2.10:554/Streaming/Channels/101"
        );
        let dual = "rtsp://admin:x@192.0.2.10:554/Streaming/Channels/202";
        assert_eq!(
            rewrite_stream_digit(dual, StreamType::Main),
            "rtsp://admin:x@192.0.2.10:554/Streaming/Channels/201"
        );
    }

    #[test]
    fn builds_direct_camera_url() {
        let url = build_camera_rtsp_url(
            "192.0.2.10",
            "admin",
            "secret",
            1,
            StreamType::Main,
        );
        assert_eq!(
            url,
            "rtsp://admin:secret@192.0.2.10:554/Streaming/Channels/101"
        );
    }
}
