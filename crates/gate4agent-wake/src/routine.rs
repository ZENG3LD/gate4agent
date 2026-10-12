//! Routine webhook deliver channel: one JSON POST to a session's preconfigured routine URL.
//! Moved here from mail4agent's `post_routine_json` (2026-10-13): the messenger no longer makes
//! HTTP calls to routines; the host (hatchery node) delivers through this.
//!
//! One attempt, 8 seconds, no redirects. 2xx means the routine took the letter. The key goes as
//! `Authorization: Bearer` and `X-Automation-Key`; neither the key nor the URL is ever put in an
//! error.

use std::time::Duration;

/// Why a routine POST failed. `Display` never carries the URL or the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutineError {
    /// URL refused before any socket was opened.
    BadUrl,
    /// Key refused (not header-safe).
    BadBearer,
    /// Transport failure (connect, TLS, timeout), already redacted.
    Transport(String),
    /// Non-2xx answer.
    Status(u16),
}

impl std::fmt::Display for RoutineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadUrl => f.write_str("routine url refused"),
            Self::BadBearer => f.write_str("routine key refused"),
            Self::Transport(t) => write!(f, "routine transport: {t}"),
            Self::Status(s) => write!(f, "routine status {s}"),
        }
    }
}

impl std::error::Error for RoutineError {}

impl RoutineError {
    /// The HTTP status, when the routine answered.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Status(s) => Some(*s),
            _ => None,
        }
    }
}

const MAX_URL: usize = 2048;

/// `http(s)://host[...]`, no userinfo, no control characters, bounded length.
pub fn url_ok(url: &str) -> bool {
    if url.is_empty() || url.len() > MAX_URL || url.chars().any(char::is_control) {
        return false;
    }
    let Some(after) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) else { return false };
    let authority = after.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') || authority.contains(char::is_whitespace) {
        return false;
    }
    let host = authority.rsplit_once(']').map(|(h, _)| h).unwrap_or(authority);
    let host = host.strip_prefix('[').unwrap_or_else(|| host.split(':').next().unwrap_or(""));
    !host.is_empty()
}

/// `Some(token)` for a usable key, `None` for blank; `Err` for an unsafe one.
pub fn bearer_token(token: Option<&str>) -> Result<Option<&str>, RoutineError> {
    let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) else { return Ok(None) };
    if token.bytes().all(|b| b.is_ascii_graphic()) {
        Ok(Some(token))
    } else {
        Err(RoutineError::BadBearer)
    }
}

/// POSTs `body` once to `url` with the optional routine key.
pub fn post_routine_json(url: &str, body: &serde_json::Value, bearer: Option<&str>) -> Result<(), RoutineError> {
    if !url_ok(url) {
        return Err(RoutineError::BadUrl);
    }
    let target = reqwest::Url::parse(url).map_err(|_| RoutineError::BadUrl)?;
    if !matches!(target.scheme(), "http" | "https") || target.host_str().is_none() {
        return Err(RoutineError::BadUrl);
    }
    let token = bearer_token(bearer)?;
    let bytes = serde_json::to_vec(body).map_err(|e| RoutineError::Transport(e.to_string()))?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .use_rustls_tls()
        .build()
        .map_err(|e| RoutineError::Transport(redact(e.without_url().to_string(), url, token)))?;
    let mut req = client.post(target).header(reqwest::header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| RoutineError::BadBearer)?;
        auth.set_sensitive(true);
        let mut key = reqwest::header::HeaderValue::from_str(token).map_err(|_| RoutineError::BadBearer)?;
        key.set_sensitive(true);
        req = req.header(reqwest::header::AUTHORIZATION, auth).header("x-automation-key", key);
    }
    let resp = req.body(bytes).send().map_err(|e| RoutineError::Transport(redact(e.without_url().to_string(), url, token)))?;
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(RoutineError::Status(status))
    }
}

fn redact(mut text: String, url: &str, token: Option<&str>) -> String {
    if !url.is_empty() {
        text = text.replace(url, "[redacted]");
    }
    if let Some(t) = token.filter(|t| t.len() >= 4) {
        text = text.replace(t, "[redacted]");
    }
    text.chars().take(240).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn urls_and_keys_are_checked_before_any_socket() {
        assert!(url_ok("https://example.invalid/hook"));
        assert!(url_ok("http://127.0.0.1:9/x"));
        assert!(!url_ok("ftp://example.invalid"));
        assert!(!url_ok("https://user@example.invalid/"));
        assert!(!url_ok("https://"));
        assert_eq!(bearer_token(Some("  ")), Ok(None));
        assert_eq!(bearer_token(Some("bad key")), Err(RoutineError::BadBearer));
        assert_eq!(post_routine_json("nope", &serde_json::json!({}), None), Err(RoutineError::BadUrl));
    }

    fn serve_once(status: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/routine", l.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = s.read(&mut chunk).unwrap_or(0);
                if n == 0 { break; }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
                    if body.len() >= len { break; }
                }
            }
            let _ = write!(s, "HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            String::from_utf8_lossy(&buf).into_owned()
        });
        (url, h)
    }

    #[test]
    fn posts_json_with_both_key_headers_and_maps_status() {
        let (url, h) = serve_once("200 OK");
        post_routine_json(&url, &serde_json::json!({"body":"hi"}), Some("k-123456")).unwrap();
        let req = h.join().unwrap().to_ascii_lowercase();
        assert!(req.contains("authorization: bearer k-123456"));
        assert!(req.contains("x-automation-key: k-123456"));
        assert!(req.contains("\"body\":\"hi\""));

        let (url, h) = serve_once("403 Forbidden");
        let err = post_routine_json(&url, &serde_json::json!({}), Some("k-123456")).unwrap_err();
        h.join().unwrap();
        assert_eq!(err.status(), Some(403));
        assert!(!err.to_string().contains("k-123456"));
    }
}
