//! Shared test helpers: mock HTTP webhook server, mock WebSocket server,
//! in-process sip2agent server.
#![allow(dead_code)]

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use sip2agent::config::{Config, SharedConfig};
use sip2agent::sip::SipServer;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_util::sync::CancellationToken;

/// One request received by the mock HTTP server.
#[derive(Debug, Clone)]
pub struct HttpHit {
    pub path: String,
    pub body: Value,
}

pub type Responder = Arc<dyn Fn(&HttpHit) -> (u16, String) + Send + Sync>;

pub struct MockHttp {
    pub base: String,
    pub hits: mpsc::UnboundedReceiver<HttpHit>,
}

/// Minimal HTTP/1.1 server: records JSON bodies, answers via `responder`.
pub async fn mock_http(responder: Responder) -> MockHttp {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let tx = tx.clone();
            let responder = responder.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let header_end = loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = find(&buf, b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().ok())?
                    })
                    .unwrap_or(0);
                while buf.len() < header_end + len {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let body: Value = serde_json::from_slice(&buf[header_end..]).unwrap_or(Value::Null);
                let hit = HttpHit { path, body };
                let (status, text) = responder(&hit);
                let _ = tx.send(hit);
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    MockHttp {
        base: format!("http://{addr}"),
        hits: rx,
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// An accepted mock WebSocket connection plus its upgrade request info.
pub struct WsConn {
    pub ws: WebSocketStream<TcpStream>,
    pub uri: String,
    pub headers: Vec<(String, String)>,
}

impl WsConn {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }

    pub async fn send_json(&mut self, v: Value) {
        self.ws
            .send(Message::Text(v.to_string().into()))
            .await
            .unwrap();
    }

    /// Next text frame as JSON (panics on timeout/close).
    pub async fn next_json(&mut self) -> Value {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
                .await
                .expect("ws recv timeout")
                .expect("ws closed")
                .expect("ws error");
            if let Message::Text(t) = msg {
                return serde_json::from_str(&t).unwrap();
            }
        }
    }

    /// Next text frame whose type is not `input_audio_buffer.append`.
    pub async fn next_control(&mut self) -> Value {
        loop {
            let v = self.next_json().await;
            if v["type"] != "input_audio_buffer.append" {
                return v;
            }
        }
    }
}

/// Bind a mock WebSocket server; returns the ws:// base URL and a receiver of
/// accepted connections.
pub async fn mock_ws() -> (String, mpsc::UnboundedReceiver<WsConn>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut uri = String::new();
                let mut headers = Vec::new();
                let cb = |req: &Request, resp: Response| {
                    uri = req.uri().to_string();
                    headers = req
                        .headers()
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                        .collect();
                    Ok(resp)
                };
                if let Ok(ws) = tokio_tungstenite::accept_hdr_async(sock, cb).await {
                    let _ = tx.send(WsConn { ws, uri, headers });
                }
            });
        }
    });
    (format!("ws://{addr}"), rx)
}

/// Start sip2agent in-process on an ephemeral port.
pub async fn start_server(config: &str) -> (SocketAddr, CancellationToken) {
    let (addr, shutdown, _) = start_server_shared(config).await;
    (addr, shutdown)
}

/// Like [`start_server`], also returning the live config for reload tests.
pub async fn start_server_shared(
    config: &str,
) -> (SocketAddr, CancellationToken, Arc<SharedConfig>) {
    sip2agent::install_crypto_provider();
    let cfg = Config::parse(config).expect("config");
    let server = SipServer::bind_shared(Arc::new(SharedConfig::new(cfg)))
        .await
        .expect("bind");
    let addr = server.local_addr();
    let shared = server.shared_config();
    let shutdown = CancellationToken::new();
    let s = shutdown.clone();
    tokio::spawn(async move {
        server.run(s).await.ok();
    });
    (addr, shutdown, shared)
}

pub fn is_silent(pcm: &[i16]) -> bool {
    pcm.iter().all(|s| s.unsigned_abs() < 200)
}

pub fn tone(n: usize, rate: f32, hz: f32, amp: f32) -> Vec<i16> {
    (0..n)
        .map(|i| ((i as f32 * 2.0 * std::f32::consts::PI * hz / rate).sin() * amp) as i16)
        .collect()
}
