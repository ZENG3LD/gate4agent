//! Kimi Code CLI: submit a prompt to a session hosted by the local Kimi
//! server (`kimi web` / `kimi rc`, Kimi Code 2.x).
//!
//! `POST {origin}/api/v1/sessions/{session_id}/prompts` with
//! `{"content":[{"type":"text","text":…}]}` and `Authorization: Bearer`.
//! The server answers `{"code":0,"data":{"prompt_id",…,"status":"running"|"queued"|"blocked"}}`.
//! Verified on the box against Kimi Code 2.1.1 (`kimi web`, loopback): the
//! prompt was accepted with `status: running`. The bearer is the server's
//! own token; the host injects it (never a literal, never logged).
//!
//! TODO(kimi-acp): a `kimi acp` host adapter (ACP `session/prompt`, never answering permission
//! requests) belongs here next to this one; see the docs handoff of 2026-10-13.
//!
//! TODO(kimi-tui): a plain `kimi` TUI does not expose this server. For TUI
//! sessions the fallback is the inbox + a `Stop` hook (`exit 2` with the
//! letter on stderr continues the turn once) or `UserPromptSubmit`; neither
//! wakes an idle TUI.

use std::time::Duration;

use serde_json::{json, Value};

use super::{
    wake_prompt, ProviderKind, ProviderSession, SessionKind, WakeAdapter, WakeError, WakeLetter,
    WakeOutcome,
};

/// Env var: Kimi server origin, e.g. `http://127.0.0.1:58627`.
pub const KIMI_SERVER_URL_ENV: &str = "M4A_KIMI_SERVER_URL";
/// Env var: Kimi server bearer, injected by the host keychain.
pub const KIMI_SERVER_TOKEN_ENV: &str = "M4A_KIMI_SERVER_TOKEN";

/// Client of the local Kimi server.
pub struct KimiServerAdapter {
    origin: Option<String>,
    bearer: Option<String>,
}

impl KimiServerAdapter {
    /// Adapter for a loopback `origin`. A non-loopback origin is dropped.
    pub fn new(origin: Option<String>, bearer: Option<String>) -> Self {
        let origin = origin
            .map(|o| o.trim().trim_end_matches('/').to_string())
            .filter(|o| is_loopback_http(o));
        Self {
            origin,
            bearer: bearer.filter(|b| !b.is_empty()),
        }
    }

    /// From [`KIMI_SERVER_URL_ENV`] / [`KIMI_SERVER_TOKEN_ENV`].
    pub fn from_env() -> Self {
        Self::new(
            std::env::var(KIMI_SERVER_URL_ENV).ok(),
            std::env::var(KIMI_SERVER_TOKEN_ENV).ok(),
        )
    }
}

impl KimiServerAdapter {
    /// Whether both origin and bearer are set.
    pub fn is_configured(&self) -> bool {
        self.origin.is_some() && self.bearer.is_some()
    }

    /// `(origin, bearer)`, for [`super::AdapterConfig`].
    pub fn into_parts(self) -> (Option<String>, Option<String>) {
        (self.origin, self.bearer)
    }
}

/// Finds a running `kimi web` / `kimi rc` server of this user: the newest
/// `~/.kimi-code/server/instances/*.json` whose pid is alive (Linux) and
/// whose host is loopback, plus the persistent `server.token` the Kimi
/// web UI itself uses. `home` overrides `KIMI_CODE_HOME` / `~/.kimi-code`.
pub fn discover_local_server(home: Option<&std::path::Path>) -> Option<KimiServerAdapter> {
    let home = home.map(std::path::Path::to_path_buf).or_else(|| {
        std::env::var_os("KIMI_CODE_HOME")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                    .map(|h| std::path::PathBuf::from(h).join(".kimi-code"))
            })
    })?;
    let mut instances: Vec<(std::time::SystemTime, Value)> =
        std::fs::read_dir(home.join("server").join("instances"))
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|item| {
                let modified = item.metadata().and_then(|m| m.modified()).ok()?;
                let value: Value =
                    serde_json::from_slice(&std::fs::read(item.path()).ok()?).ok()?;
                Some((modified, value))
            })
            .collect();
    instances.sort_by(|a, b| b.0.cmp(&a.0));
    let token = std::fs::read_to_string(home.join("server.token")).ok()?;
    let token = token.trim().to_string();
    for (_, instance) in instances {
        if let Some(pid) = instance["pid"].as_u64() {
            if cfg!(target_os = "linux") && !std::path::Path::new(&format!("/proc/{pid}")).exists()
            {
                continue;
            }
        }
        let Some(port) = instance["port"].as_u64() else {
            continue;
        };
        let host = instance["host"].as_str().unwrap_or("127.0.0.1");
        let host = if host == "0.0.0.0" || host.is_empty() {
            "127.0.0.1"
        } else {
            host
        };
        let adapter =
            KimiServerAdapter::new(Some(format!("http://{host}:{port}")), Some(token.clone()));
        if adapter.is_configured() {
            return Some(adapter);
        }
    }
    None
}

/// Request body for one text prompt.
pub fn prompt_body(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}]})
}

fn is_loopback_http(origin: &str) -> bool {
    let Some(rest) = origin.strip_prefix("http://") else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or("");
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

fn session_path_ok(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl WakeAdapter for KimiServerAdapter {
    fn kind(&self) -> SessionKind {
        SessionKind::local(ProviderKind::KimiCode)
    }

    fn probe(&self, session: &ProviderSession) -> Result<(), WakeError> {
        if self.origin.is_none() {
            return Err(WakeError::Unavailable(
                "kimi server url unset or not loopback".into(),
            ));
        }
        if self.bearer.is_none() {
            return Err(WakeError::Unavailable("kimi server token unset".into()));
        }
        if !session_path_ok(&session.session_id) {
            return Err(WakeError::Unavailable(
                "kimi session id is not a plain id".into(),
            ));
        }
        Ok(())
    }

    fn wake(
        &mut self,
        session: &ProviderSession,
        letter: &WakeLetter<'_>,
    ) -> Result<WakeOutcome, WakeError> {
        self.probe(session)?;
        let origin = self.origin.as_deref().unwrap_or_default();
        let bearer = self.bearer.as_deref().unwrap_or_default();
        let url = format!("{origin}/api/v1/sessions/{}/prompts", session.session_id);
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|err| WakeError::Transport(err.to_string()))?;
        let response = client
            .post(url)
            .bearer_auth(bearer)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(prompt_body(&wake_prompt(session, letter)).to_string())
            .send()
            .map_err(|err| WakeError::Unavailable(format!("kimi server: {}", err.without_url())))?;
        let status = response.status();
        let body: Value = response
            .bytes()
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null);
        if !status.is_success() || body["code"] != json!(0) {
            let msg = body["msg"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(120)
                .collect::<String>();
            return Err(WakeError::Transport(format!(
                "kimi prompt: http {} {msg}",
                status.as_u16()
            )));
        }
        match body["data"]["status"].as_str() {
            Some("running" | "queued") => Ok(WakeOutcome::Delivered),
            Some(other) => Err(WakeError::Transport(format!("kimi prompt status {other}"))),
            None => Err(WakeError::Transport("kimi prompt: no status".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{letter, session};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    fn fake(status: &'static str) -> (String, std::thread::JoinHandle<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut auth = String::new();
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if lower.starts_with("authorization:") {
                    auth = line.trim().to_string();
                }
                if let Some(v) = lower.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            let reply = format!(
                "{{\"code\":0,\"msg\":\"success\",\"data\":{{\"prompt_id\":\"p\",\"status\":\"{status}\"}}}}"
            );
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .unwrap();
            (
                format!("{} {auth}", request_line.trim()),
                String::from_utf8(body).unwrap(),
            )
        });
        (origin, handle)
    }

    #[test]
    fn posts_prompt_with_bearer_and_accepts_running() {
        let (origin, server) = fake("running");
        let mut adapter = KimiServerAdapter::new(Some(origin), Some("tok".into()));
        let mut s = session(SessionKind::local(ProviderKind::KimiCode));
        s.session_id = "session_abc-1".into();
        assert_eq!(
            adapter.wake(&s, &letter("hi")).unwrap(),
            WakeOutcome::Delivered
        );
        let (head, body) = server.join().unwrap();
        assert!(head.starts_with("POST /api/v1/sessions/session_abc-1/prompts HTTP/1.1"));
        assert!(head.ends_with("Bearer tok"));
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["content"][0]["type"], "text");
        assert!(body["content"][0]["text"].as_str().unwrap().ends_with("hi"));
    }

    #[test]
    fn blocked_is_an_error_and_remote_origin_is_refused() {
        let (origin, server) = fake("blocked");
        let mut adapter = KimiServerAdapter::new(Some(origin), Some("tok".into()));
        let s = session(SessionKind::local(ProviderKind::KimiCode));
        assert!(adapter.wake(&s, &letter("hi")).is_err());
        server.join().unwrap();
        let remote = KimiServerAdapter::new(Some("http://10.0.0.2:58627".into()), Some("t".into()));
        assert!(matches!(remote.probe(&s), Err(WakeError::Unavailable(_))));
        let mut bad = session(SessionKind::local(ProviderKind::KimiCode));
        bad.session_id = "../x".into();
        let ok = KimiServerAdapter::new(Some("http://127.0.0.1:1".into()), Some("t".into()));
        assert!(ok.probe(&bad).is_err());
    }

    /// Live: `M4A_LIVE_KIMI_URL`, `M4A_LIVE_KIMI_TOKEN`, `M4A_LIVE_KIMI_SESSION`.
    #[test]
    #[ignore]
    fn live_prompt() {
        let mut adapter = KimiServerAdapter::new(
            std::env::var("M4A_LIVE_KIMI_URL").ok(),
            std::env::var("M4A_LIVE_KIMI_TOKEN").ok(),
        );
        let mut s = session(SessionKind::local(ProviderKind::KimiCode));
        s.session_id = std::env::var("M4A_LIVE_KIMI_SESSION").unwrap();
        assert_eq!(
            adapter.wake(&s, &letter("live probe")).unwrap(),
            WakeOutcome::Delivered
        );
    }

    #[test]
    fn discovers_live_loopback_instance_and_skips_dead_pids() {
        let home = std::env::temp_dir().join(format!("m4a-kimi-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let inst = home.join("server").join("instances");
        std::fs::create_dir_all(&inst).unwrap();
        assert!(discover_local_server(Some(&home)).is_none());
        std::fs::write(home.join("server.token"), "tok\n").unwrap();
        std::fs::write(
            inst.join("dead.json"),
            r#"{"pid":4000000000,"host":"127.0.0.1","port":1}"#,
        )
        .unwrap();
        if cfg!(target_os = "linux") {
            assert!(discover_local_server(Some(&home)).is_none());
        }
        std::fs::write(
            inst.join("live.json"),
            format!(
                r#"{{"pid":{},"host":"127.0.0.1","port":58627}}"#,
                std::process::id()
            ),
        )
        .unwrap();
        let (url, token) = discover_local_server(Some(&home)).unwrap().into_parts();
        assert_eq!(url.as_deref(), Some("http://127.0.0.1:58627"));
        assert_eq!(token.as_deref(), Some("tok"));
        let _ = std::fs::remove_dir_all(&home);
    }
}
