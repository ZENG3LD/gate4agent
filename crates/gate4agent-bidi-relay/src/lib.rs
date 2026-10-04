//! Dumb byte relay for the bidirectional browser stream.
//!
//! HTTP `POST /session/{id}` opens a slot. WebSocket
//! `/session/{id}/node` and `/session/{id}/hq` are the two directions.
//! Binary frames are copied from one socket to the other. This crate does
//! not decode images, clicks, or key names. Logs are `bytes=N` only.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

const MAX_PENDING: usize = 4;
const HEADER_LIMIT: usize = 65_536;

#[derive(Clone)]
pub struct RelayLog {
    inner: Arc<dyn Fn(String) + Send + Sync>,
}

impl RelayLog {
    pub fn new<F>(log: F) -> Self
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        Self {
            inner: Arc::new(log),
        }
    }

    pub fn forward(&self, session: &str, dir: &str, bytes: usize) {
        (self.inner)(format_forward(session, dir, bytes));
    }
}

/// The only line this relay emits about a payload. No fields, no body.
pub fn format_forward(session: &str, dir: &str, bytes: usize) -> String {
    format!("bidi-relay session={session} dir={dir} bytes={bytes}")
}

pub async fn serve(listener: TcpListener, log: RelayLog) -> io::Result<()> {
    let sessions: Arc<Mutex<HashMap<String, Session>>> = Arc::new(Mutex::new(HashMap::new()));
    loop {
        let (sock, _) = listener.accept().await?;
        let sessions = Arc::clone(&sessions);
        let log = log.clone();
        tokio::spawn(async move {
            if let Err(err) = handle(sock, sessions, log).await {
                eprintln!("bidi-relay conn: {err}");
            }
        });
    }
}

struct Session {
    node: Slot,
    hq: Slot,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            node: Slot::default(),
            hq: Slot::default(),
        }
    }
}

struct Slot {
    tx: Option<mpsc::Sender<Vec<u8>>>,
    pending: VecDeque<Vec<u8>>,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            tx: None,
            pending: VecDeque::new(),
        }
    }
}

#[derive(Clone, Copy)]
enum Role {
    Node,
    Hq,
}

impl Role {
    fn incoming_dir(self) -> &'static str {
        match self {
            // Bytes written onto the HQ socket came from the node.
            Role::Hq => "node->hq",
            Role::Node => "hq->node",
        }
    }
}

async fn handle(
    mut sock: TcpStream,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    log: RelayLog,
) -> io::Result<()> {
    let header = read_headers(&mut sock).await?;
    let head = std::str::from_utf8(&header).map_err(|err| {
        io::Error::new(io::ErrorKind::InvalidData, format!("headers are not utf-8: {err}"))
    })?;
    let (method, path) = request_line(head)?;
    if method == "POST" {
        let session = session_from_post(path)?;
        {
            let mut map = sessions.lock().await;
            map.entry(session).or_default();
        }
        let body = b"{\"ok\":true}";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        use tokio::io::AsyncWriteExt;
        sock.write_all(response.as_bytes()).await?;
        sock.write_all(body).await?;
        let _ = sock.shutdown().await;
        return Ok(());
    }
    if method == "GET" {
        let (session, role) = session_from_ws(path)?;
        if !head.to_ascii_lowercase().contains("upgrade: websocket") {
            return write_status(&mut sock, 400, "websocket upgrade required").await;
        }
        let prefixed = PrefixedStream {
            prefix: header,
            pos: 0,
            inner: sock,
        };
        let ws = tokio_tungstenite::accept_async(prefixed)
            .await
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        pump(ws, session, role, sessions, log).await;
        return Ok(());
    }
    write_status(&mut sock, 405, "method not allowed").await
}

async fn pump<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    session: String,
    role: Role,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    log: RelayLog,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut write, mut read) = ws.split();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(16);
    {
        let mut map = sessions.lock().await;
        let slot = map.entry(session.clone()).or_default().slot_mut(role);
        slot.tx = Some(tx);
        let queued: Vec<Vec<u8>> = slot.pending.drain(..).collect();
        if let Some(tx) = &slot.tx {
            for msg in queued {
                if tx.try_send(msg).is_err() {
                    break;
                }
            }
        }
    }

    let dir = role.incoming_dir();
    loop {
        tokio::select! {
            incoming = read.next() => {
                match incoming {
                    Some(Ok(Message::Binary(data))) => {
                        forward(&sessions, &session, role, data.to_vec()).await;
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if write.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {
                        // Text and other opcodes are not a browser stream.
                        // Drop them without reading fields out of the body.
                    }
                }
            }
            outgoing = rx.recv() => {
                match outgoing {
                    Some(bytes) => {
                        let n = bytes.len();
                        if write.send(Message::binary(bytes)).await.is_err() {
                            break;
                        }
                        log.forward(&session, dir, n);
                    }
                    None => break,
                }
            }
        }
    }
}

async fn forward(
    sessions: &Mutex<HashMap<String, Session>>,
    session: &str,
    from: Role,
    bytes: Vec<u8>,
) {
    let tx = {
        let mut map = sessions.lock().await;
        let Some(slot_session) = map.get_mut(session) else {
            return;
        };
        slot_session.slot_mut(peer(from)).tx.clone()
    };
    if let Some(tx) = tx {
        let _ = tx.send(bytes).await;
        return;
    }
    let mut map = sessions.lock().await;
    let Some(slot_session) = map.get_mut(session) else {
        return;
    };
    let dest = slot_session.slot_mut(peer(from));
    if let Some(tx) = dest.tx.clone() {
        drop(map);
        let _ = tx.send(bytes).await;
        return;
    }
    dest.pending.push_back(bytes);
    while dest.pending.len() > MAX_PENDING {
        dest.pending.pop_front();
    }
}

fn peer(role: Role) -> Role {
    match role {
        Role::Node => Role::Hq,
        Role::Hq => Role::Node,
    }
}

impl Session {
    fn slot_mut(&mut self, role: Role) -> &mut Slot {
        match role {
            Role::Node => &mut self.node,
            Role::Hq => &mut self.hq,
        }
    }
}

async fn read_headers(sock: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(buf);
        }
        if buf.len() > HEADER_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "headers too large",
            ));
        }
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "eof before headers",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn request_line(head: &str) -> io::Result<(&str, &str)> {
    let line = head.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing method")
    })?;
    let path = parts.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing path")
    })?;
    Ok((method, path))
}

fn session_from_post(path: &str) -> io::Result<String> {
    let rest = path.strip_prefix("/session/").ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "POST path must be /session/{id}")
    })?;
    validate_session(rest)
}

fn session_from_ws(path: &str) -> io::Result<(String, Role)> {
    let rest = path.strip_prefix("/session/").ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "WS path must be /session/{id}/{role}")
    })?;
    let (id, role) = rest.split_once('/').ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "WS path needs a role")
    })?;
    let id = validate_session(id)?;
    let role = match role {
        "node" => Role::Node,
        "hq" => Role::Hq,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "role must be node or hq",
            ))
        }
    };
    Ok((id, role))
}

fn validate_session(session: &str) -> io::Result<String> {
    if session.is_empty()
        || session.len() > 64
        || !session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad session id",
        ));
    }
    Ok(session.to_owned())
}

async fn write_status(sock: &mut TcpStream, status: u16, reason: &str) -> io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let body = reason.as_bytes();
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(response.as_bytes()).await?;
    sock.write_all(body).await?;
    let _ = sock.shutdown().await;
    Ok(())
}

struct PrefixedStream {
    prefix: Vec<u8>,
    pos: usize,
    inner: TcpStream,
}

impl AsyncRead for PrefixedStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.pos < this.prefix.len() {
            let n = std::cmp::min(buf.remaining(), this.prefix.len() - this.pos);
            buf.put_slice(&this.prefix[this.pos..this.pos + n]);
            this.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for PrefixedStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

pub async fn bind(addr: SocketAddr) -> io::Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    Ok((listener, local))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    #[test]
    fn log_line_is_counts_only() {
        let line = format_forward("proof1", "hq->node", 21);
        assert_eq!(line, "bidi-relay session=proof1 dir=hq->node bytes=21");
        assert!(!line.contains("button"));
        assert!(!line.contains("x="));
        assert!(!line.contains("y="));
    }

    #[tokio::test]
    async fn forwards_opaque_bytes_and_logs_counts_only() {
        let lines = Arc::new(StdMutex::new(Vec::<String>::new()));
        let lines_log = Arc::clone(&lines);
        let log = RelayLog::new(move |line| {
            lines_log.lock().unwrap().push(line);
        });
        let (listener, addr) = bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = serve(listener, log).await;
        });

        post_session(addr, "proof1").await;
        let mut node = ws_connect(addr, "proof1", "node").await;
        let mut hq = ws_connect(addr, "proof1", "hq").await;

        let frame = b"PNG-not-really button=left x=1 y=2".to_vec();
        node.send(Message::binary(frame.clone())).await.unwrap();
        let got = next_binary(&mut hq).await;
        assert_eq!(got, frame);

        let click = b"button=left x=120 y=90".to_vec();
        hq.send(Message::binary(click.clone())).await.unwrap();
        let got_click = next_binary(&mut node).await;
        assert_eq!(got_click, click);

        hq.send(Message::text("button=left x=9 y=8")).await.unwrap();
        let leaked = tokio::time::timeout(Duration::from_millis(300), next_binary(&mut node)).await;
        assert!(leaked.is_err(), "text payload must not be forwarded");

        let logged = lines.lock().unwrap().clone();
        assert!(
            logged.iter().any(|l| l == "bidi-relay session=proof1 dir=node->hq bytes=34"),
            "frame log missing: {logged:?}"
        );
        assert!(
            logged.iter().any(|l| l == "bidi-relay session=proof1 dir=hq->node bytes=22"),
            "input log missing: {logged:?}"
        );
        let joined = logged.join("\n");
        assert!(!joined.contains("button"));
        assert!(!joined.contains("x="));
        assert!(!joined.contains("y="));
    }

    async fn post_session(addr: SocketAddr, session: &str) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut sock = TcpStream::connect(addr).await.unwrap();
        let req = format!("POST /session/{session} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n");
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        sock.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf);
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        assert!(text.contains("{\"ok\":true}"));
    }

    async fn ws_connect(
        addr: SocketAddr,
        session: &str,
        role: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>> {
        let url = format!("ws://{addr}/session/{session}/{role}");
        let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        ws
    }

    async fn next_binary<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> Vec<u8>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Binary(data) => return data.to_vec(),
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected message: {other:?}"),
            }
        }
    }
}
