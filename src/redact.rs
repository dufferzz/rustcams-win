//! Strip URL userinfo from logs and GStreamer error strings.

use url::Url;

/// Log-safe camera URL (`rtsp://***@host/path`).
pub fn redact_url(url: &str) -> String {
    if let Ok(u) = Url::parse(url) {
        if !u.username().is_empty() || u.password().is_some() {
            let host = u.host_str().unwrap_or("?");
            let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
            let mut path = u.path().to_string();
            if let Some(q) = u.query() {
                path.push('?');
                path.push_str(q);
            }
            return format!("{}://***@{host}{port}{path}", u.scheme());
        }
        return url.chars().take(96).collect();
    }
    redact_secrets(url).chars().take(96).collect()
}

/// Replace `scheme://user:pass@` in free-form text (GStreamer debug, anyhow).
pub fn redact_secrets(text: &str) -> String {
    let mut out = text.to_string();
    for scheme in ["rtsps://", "rtsp://", "https://", "http://"] {
        out = redact_userinfo_after_scheme(&out, scheme);
    }
    out
}

fn redact_userinfo_after_scheme(text: &str, scheme: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(scheme) {
        result.push_str(&rest[..i]);
        result.push_str(scheme);
        rest = &rest[i + scheme.len()..];
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | '\'' | '"' | ',' | ';' | ']'))
            .unwrap_or(rest.len());
        let token = &rest[..end];
        if let Some(at) = token.find('@') {
            let userinfo = &token[..at];
            if userinfo.contains(':') {
                result.push_str("***@");
                result.push_str(&token[at + 1..]);
            } else {
                result.push_str(token);
            }
        } else {
            result.push_str(token);
        }
        rest = &rest[end..];
    }
    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_rtsp_userinfo() {
        assert_eq!(
            redact_url("rtsp://admin:p%40ss@192.0.2.10:554/Streaming/Channels/101"),
            "rtsp://***@192.0.2.10:554/Streaming/Channels/101"
        );
    }

    #[test]
    fn password_with_at_sign_does_not_keep_username() {
        let s = redact_url("rtsp://admin:a@b@192.0.2.10/Streaming/Channels/101");
        assert!(s.contains("***@"), "{s}");
        assert!(!s.contains("admin:"), "{s}");
    }

    #[test]
    fn redacts_embedded_gstreamer_url() {
        let raw = "Could not open resource for reading and writing: rtsp://admin:secret@192.0.2.10:554/Streaming/Channels/102 (generic error)";
        let out = redact_secrets(raw);
        assert!(!out.contains("secret"), "{out}");
        assert!(
            out.contains("rtsp://***@192.0.2.10:554/Streaming/Channels/102"),
            "{out}"
        );
    }
}
