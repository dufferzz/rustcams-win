//! Shared ureq settings for ISAPI (NVR, PTZ, gate).

use std::time::Duration;
use ureq::Agent;

/// Digest client that does not follow redirects (avoids sending Authorization off-LAN).
pub fn agent(timeout: Duration) -> Agent {
    Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into()
}

pub fn ptz_agent() -> Agent {
    Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(3)))
        .http_status_as_error(false)
        .max_redirects(0)
        .max_idle_connections_per_host(4)
        .max_idle_age(Duration::from_secs(90))
        .build()
        .into()
}
