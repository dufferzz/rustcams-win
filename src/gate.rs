use crate::config::GateConfig;
use anyhow::{bail, Context, Result};
use digest_auth::{AuthContext, HttpMethod};
use std::time::Duration;
use tracing::info;
use ureq::Agent;

/// `PUT /ISAPI/AccessControl/RemoteControl/door/{id}` with HTTP digest (same as curl `--digest`).
pub fn remote_control_door(cfg: &GateConfig) -> Result<()> {
    if !cfg.is_configured() {
        bail!("gate host/username is empty");
    }
    let path = cfg.request_path();
    let url = cfg.request_url();
    let action = cfg.action.trim();
    let action = if action.is_empty() { "open" } else { action };

    info!(host = %cfg.host.trim(), door = %cfg.door_id.trim(), action, "opening gate");

    let agent: Agent = Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .into();

    let challenge = agent
        .put(&url)
        .header("Content-Type", "text/plain")
        .send(action.as_bytes())
        .with_context(|| format!("gate probe PUT {url}"))?;

    let status = challenge.status();
    if (200..300).contains(&status.as_u16()) {
        return Ok(());
    }
    if status != 401 {
        bail!("gate command expected HTTP 401 or 2xx from {url}, got {status}");
    }

    let www = challenge
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow::anyhow!("gate 401 missing WWW-Authenticate header"))?
        .to_string();

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

    let response = agent
        .put(&url)
        .header("Authorization", &answer)
        .header("Content-Type", "text/plain")
        .send(action.as_bytes())
        .with_context(|| format!("gate authenticated PUT {url}"))?;

    let status = response.status();
    if !(200..300).contains(&status.as_u16()) {
        bail!("gate command failed: HTTP {status}");
    }
    Ok(())
}
