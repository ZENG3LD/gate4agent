//! Bounded typed HTTP/1.1 client for the local Gate4Agent C2 observer API.

use gate4agent_c2_protocol::{HealthResponse, ReadyResponse, StatusResponse, MAX_C2_RESPONSE_BYTES};
use serde::de::DeserializeOwned;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{connect_local, C2ControlError, C2ControlHandle, C2EventReceiver};

const MAX_RESPONSE_HEADERS: usize = 16 * 1024;

#[derive(Clone)]
pub struct C2Client {
    address: SocketAddr,
    token: String,
    deadline: Duration,
}

impl C2Client {
    pub fn new(address: SocketAddr, token: impl Into<String>) -> Result<Self, C2ClientError> {
        let token = token.into();
        validate_token(&token)?;
        Ok(Self { address, token, deadline: Duration::from_secs(3) })
    }

    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    pub async fn health(&self) -> Result<HealthResponse, C2ClientError> {
        self.get("/health", false, false).await
    }

    pub async fn ready(&self) -> Result<ReadyResponse, C2ClientError> {
        self.get("/ready", false, true).await
    }

    pub async fn status(&self) -> Result<StatusResponse, C2ClientError> {
        self.get("/status", true, false).await
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, authenticated: bool, allow_unready: bool) -> Result<T, C2ClientError> {
        timeout(self.deadline, self.get_inner(path, authenticated, allow_unready))
            .await
            .map_err(|_| C2ClientError::Deadline)?
    }

    async fn get_inner<T: DeserializeOwned>(&self, path: &str, authenticated: bool, allow_unready: bool) -> Result<T, C2ClientError> {
        let mut stream = TcpStream::connect(self.address).await?;
        let auth = if authenticated {
            format!("Authorization: Bearer {}\r\n", self.token)
        } else {
            String::new()
        };
        let request = format!("GET {path} HTTP/1.1\r\nHost: {}\r\n{auth}Connection: close\r\n\r\n", self.address);
        stream.write_all(request.as_bytes()).await?;
        let mut bytes = Vec::with_capacity(4096);
        let mut chunk = [0_u8; 4096];
        loop {
            let count = stream.read(&mut chunk).await?;
            if count == 0 { break; }
            if bytes.len().saturating_add(count) > MAX_C2_RESPONSE_BYTES + MAX_RESPONSE_HEADERS {
                return Err(C2ClientError::Oversized);
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
        let header_end = bytes.windows(4).position(|window| window == b"\r\n\r\n")
            .ok_or(C2ClientError::Malformed)?;
        if header_end > MAX_RESPONSE_HEADERS { return Err(C2ClientError::Oversized); }
        let headers = std::str::from_utf8(&bytes[..header_end]).map_err(|_| C2ClientError::Malformed)?;
        let mut lines = headers.split("\r\n");
        let status_line = lines.next().ok_or(C2ClientError::Malformed)?;
        let mut status_parts = status_line.split_whitespace();
        if status_parts.next() != Some("HTTP/1.1") { return Err(C2ClientError::Malformed); }
        let status = status_parts.next().ok_or(C2ClientError::Malformed)?
            .parse::<u16>().map_err(|_| C2ClientError::Malformed)?;
        let mut content_length = None;
        for line in lines {
            let (name, value) = line.split_once(':').ok_or(C2ClientError::Malformed)?;
            if name.eq_ignore_ascii_case("content-length") {
                if content_length.is_some() { return Err(C2ClientError::Malformed); }
                content_length = Some(value.trim().parse::<usize>().map_err(|_| C2ClientError::Malformed)?);
            }
        }
        let content_length = content_length.ok_or(C2ClientError::Malformed)?;
        if content_length > MAX_C2_RESPONSE_BYTES { return Err(C2ClientError::Oversized); }
        let body = &bytes[header_end + 4..];
        if body.len() != content_length { return Err(C2ClientError::Malformed); }
        if status == 401 { return Err(C2ClientError::Unauthorized); }
        if !(200..300).contains(&status) && !(allow_unready && status == 503) { return Err(C2ClientError::HttpStatus(status)); }
        serde_json::from_slice(body).map_err(C2ClientError::Json)
    }
}

impl std::fmt::Debug for C2Client {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("C2Client")
            .field("address", &self.address)
            .field("token", &"[REDACTED]")
            .field("deadline", &self.deadline)
            .finish()
    }
}

fn validate_token(token: &str) -> Result<(), C2ClientError> {
    if token.is_empty() || token.len() > 4096 || !token.bytes().all(|byte| matches!(byte, 0x21..=0x7e)) {
        return Err(C2ClientError::InvalidToken);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum C2ClientError {
    #[error("C2 bearer token must contain 1..=4096 visible ASCII bytes without whitespace")]
    InvalidToken,
    #[error("C2 HTTP I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("C2 request exceeded its deadline")]
    Deadline,
    #[error("C2 returned HTTP 401")]
    Unauthorized,
    #[error("C2 returned HTTP status {0}")]
    HttpStatus(u16),
    #[error("C2 returned malformed HTTP")]
    Malformed,
    #[error("C2 response exceeded the 8 MiB body bound")]
    Oversized,
    #[error("C2 returned malformed JSON: {0}")]
    Json(serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn one_response(response: Vec<u8>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(&response).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        address
    }

    #[test]
    fn bearer_rejects_header_injection_and_debug_redacts_secret() {
        let address = "127.0.0.1:1".parse().unwrap();
        assert!(matches!(C2Client::new(address, "safe\r\nInjected: yes"), Err(C2ClientError::InvalidToken)));
        let client = C2Client::new(address, "unique-secret").unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("unique-secret"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn http_client_rejects_unauthorized_malformed_and_oversized_responses() {
        let unauthorized = one_response(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()).await;
        assert!(matches!(C2Client::new(unauthorized, "token").unwrap().status().await, Err(C2ClientError::Unauthorized)));

        let malformed = one_response(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{}".to_vec()).await;
        assert!(matches!(C2Client::new(malformed, "token").unwrap().health().await, Err(C2ClientError::Malformed)));

        let oversized = one_response(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", MAX_C2_RESPONSE_BYTES + 1).into_bytes()).await;
        assert!(matches!(C2Client::new(oversized, "token").unwrap().health().await, Err(C2ClientError::Oversized)));
    }

    #[tokio::test]
    async fn typed_ready_decodes_bounded_503_initializing_response() {
        let body = br#"{"ready":false,"api_version":1,"configured_nodes":2,"attempted_nodes":0,"online_nodes":0,"offline_nodes":2,"parked_nodes":0}"#;
        let mut response = format!("HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        response.extend_from_slice(body);
        let address = one_response(response).await;
        let ready = C2Client::new(address, "token").unwrap().ready().await.unwrap();
        assert!(!ready.ready);
        assert_eq!(ready.configured_nodes, 2);
        assert_eq!(ready.attempted_nodes, 0);
    }
}
