use crate::config::GateConfig;
use anyhow::{bail, Context, Result};
use digest_auth::{AuthContext, HttpMethod};
use std::time::Duration;
use tracing::{debug, info, warn};

/// `PUT /ISAPI/AccessControl/RemoteControl/door/{id}` with HTTP digest (same as curl `--digest`).
pub fn remote_control_door(cfg: &GateConfig) -> Result<()> {
    if !cfg.is_configured() {
        bail!("gate host/username is empty");
    }
    crate::config::validate_host(&cfg.host)?;
    let path = cfg.request_path();
    let url = cfg.request_url();
    let action = cfg.action.trim();
    let action = if action.is_empty() { "open" } else { action };

    info!(host = %cfg.host.trim(), door = %cfg.door_id.trim(), action, "opening gate");
    debug!(%url, %path, "gate PUT");

    let agent = crate::http_client::agent(Duration::from_secs(10));

    let mut challenge = agent
        .put(&url)
        .header("Content-Type", "text/plain")
        .send(action.as_bytes())
        .with_context(|| format!("gate probe PUT {url}"))?;

    let status = challenge.status().as_u16();
    let www = challenge
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let probe_body = challenge
        .body_mut()
        .read_to_string()
        .unwrap_or_default();
    debug!(
        status,
        body = %truncate(&probe_body, 200),
        "gate probe response"
    );
    if (200..300).contains(&status) {
        info!(host = %cfg.host.trim(), status, "gate opened (no digest)");
        return Ok(());
    }
    if status != 401 {
        warn!(
            %url,
            status,
            body = %truncate(&probe_body, 300),
            "gate probe unexpected status"
        );
        bail!("gate command expected HTTP 401 or 2xx from {url}, got {status}");
    }

    let www = www.ok_or_else(|| anyhow::anyhow!("gate 401 missing WWW-Authenticate header"))?;

    let mut prompt = digest_auth::parse(&www).context("parse WWW-Authenticate")?;
    let context = AuthContext::new_with_method(
        &cfg.username,
        &cfg.password,
        &path,
        Some(action.as_bytes()),
        HttpMethod::PUT,
    );
    let answer = prompt
        .respond(&context)
        .context("compute digest Authorization")?
        .to_string();

    let mut response = agent
        .put(&url)
        .header("Authorization", &answer)
        .header("Content-Type", "text/plain")
        .send(action.as_bytes())
        .with_context(|| format!("gate authenticated PUT {url}"))?;

    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().unwrap_or_default();
    debug!(
        status,
        body = %truncate(&body, 200),
        "gate authenticated response"
    );
    if !(200..300).contains(&status) {
        warn!(
            %url,
            status,
            body = %truncate(&body, 300),
            "gate command failed"
        );
        bail!("gate command failed: HTTP {status}");
    }
    info!(host = %cfg.host.trim(), status, "gate opened");
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        t.to_string()
    } else {
        let mut out: String = t.chars().take(max).collect();
        out.push('…');
        out
    }
}
