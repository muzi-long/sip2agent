//! Realtime bridge loop: one task per call that owns the vendor WebSocket.
//!
//! Every 20 ms tick it:
//! 1. sends exactly one uplink frame to the vendor (caller audio, or digital
//!    silence when no RTP arrived — Doubao needs real-time pacing/keepalive);
//!    while the adapter's `hold_uplink()` is true the frame is buffered
//!    instead and the backlog is replayed when the hold ends,
//! 2. plays exactly one 160-sample frame from the playout buffer to the
//!    caller (silence when empty), giving a steady 20 ms RTP clock.
//!
//! Vendor audio is resampled into the 8 kHz playout buffer; on barge-in the
//! buffer is cleared immediately and the in-flight response is cancelled
//! where the vendor needs it. Tool calls run through the webhook in a side
//! task; results are fed back in one batch.

use super::{DownlinkEvent, RealtimeProtocol, ToolCall, ToolOutput, UplinkMessage};
use crate::audio::resample::{resample_linear, resample_linear_into};
use crate::audio::{FRAME_MS, SIP_FRAME_SAMPLES, SIP_RATE, frame_samples};
use crate::rtp::MediaPort;
use crate::webhook::{ToolDispatcher, TranscriptEntry, now_rfc3339};
use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use std::collections::VecDeque;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long to wait for `session.closed` after a graceful close request.
pub const CLOSE_ACK_TIMEOUT: Duration = Duration::from_secs(2);
/// After an endpoint `hangup`, wait at most this long for playout to drain.
pub const HANGUP_DRAIN_MAX: Duration = Duration::from_secs(10);
/// Upper bound on buffered model audio (8 kHz samples): 10 minutes.
const PLAYOUT_MAX_SAMPLES: usize = SIP_RATE as usize * 600;
/// Uplink jitter cap: older frames beyond this backlog are dropped.
const UPLINK_MAX_BACKLOG: usize = 5;
/// Caller audio held while the adapter asks to (e.g. Qwen's greeting): 30 s.
const HELD_MAX_MS: usize = 30_000;
/// Held audio is replayed in chunks of this length.
const REPLAY_CHUNK_MS: usize = 100;

/// Why the bridge ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeEnd {
    /// Cancelled locally (caller hung up / shutdown); vendor session closed gracefully.
    Cancelled,
    /// Endpoint asked to hang up.
    AgentHangup,
    /// Vendor closed the WebSocket.
    WsClosed,
    /// Transport error on the WebSocket.
    WsError(String),
    /// Vendor reported a fatal error event.
    VendorError(String),
    /// Session never became ready.
    ReadyTimeout,
}

impl BridgeEnd {
    /// Hangup reason string used in logs and the call-end webhook.
    pub fn reason(&self) -> &'static str {
        match self {
            BridgeEnd::Cancelled => "cancelled",
            BridgeEnd::AgentHangup => "agent_hangup",
            BridgeEnd::WsClosed => "ws_closed",
            BridgeEnd::WsError(_) => "ws_error",
            BridgeEnd::VendorError(_) => "vendor_error",
            BridgeEnd::ReadyTimeout => "session_ready_timeout",
        }
    }
}

pub struct BridgeResult {
    pub end: BridgeEnd,
    pub transcripts: Vec<TranscriptEntry>,
}

/// Connect to the vendor. Credentials only travel in upgrade headers.
pub async fn connect(protocol: &dyn RealtimeProtocol, timeout: Duration) -> Result<WsStream> {
    crate::install_crypto_provider();
    let url = protocol.connect_url();
    let mut request = url
        .as_str()
        .into_client_request()
        .with_context(|| format!("invalid realtime url '{}'", redact_url(&url)))?;
    for (name, value) in protocol.headers() {
        let n = name
            .parse::<tokio_tungstenite::tungstenite::http::HeaderName>()
            .with_context(|| format!("invalid header name '{name}'"))?;
        let v = value
            .parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()
            .with_context(|| format!("invalid header value for '{name}'"))?;
        request.headers_mut().insert(n, v);
    }
    let (ws, resp) = tokio::time::timeout(timeout, tokio_tungstenite::connect_async(request))
        .await
        .map_err(|_| anyhow::anyhow!("realtime connect timed out after {timeout:?}"))?
        .map_err(|e| anyhow::anyhow!("realtime connect failed: {e}"))?;
    let logid = resp
        .headers()
        .get("X-Tt-Logid")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    info!(provider = protocol.name(), url = %redact_url(&url), logid, "realtime websocket connected");
    Ok(ws)
}

/// Strip query strings from URLs before logging.
fn redact_url(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// Downlink resampler keeping a remainder so integer-ratio decimation never
/// drops samples between chunks.
struct DownResampler {
    src: u32,
    dst: u32,
    step: usize,
    carry: Vec<i16>,
}

impl DownResampler {
    fn new(src: u32, dst: u32) -> Self {
        let g = gcd(src, dst).max(1);
        Self {
            src,
            dst,
            step: (src / g) as usize,
            carry: Vec::new(),
        }
    }

    fn push(&mut self, pcm: &[i16]) -> Vec<i16> {
        if self.src == self.dst {
            return pcm.to_vec();
        }
        self.carry.extend_from_slice(pcm);
        let usable = self.carry.len() / self.step * self.step;
        if usable == 0 {
            return Vec::new();
        }
        let chunk: Vec<i16> = self.carry.drain(..usable).collect();
        resample_linear(&chunk, self.src, self.dst)
    }

    fn clear(&mut self) {
        self.carry.clear();
    }
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

type WsSink = futures::stream::SplitSink<WsStream, Message>;

async fn send_all(ws: &mut WsSink, msgs: Vec<UplinkMessage>) -> Result<(), String> {
    for m in msgs {
        let wire = match m {
            UplinkMessage::Text(t) => Message::Text(t.into()),
            UplinkMessage::Binary(b) => Message::Binary(b.into()),
        };
        ws.feed(wire).await.map_err(|e| e.to_string())?;
    }
    ws.flush().await.map_err(|e| e.to_string())
}

struct Transcripts {
    entries: Vec<TranscriptEntry>,
    assistant_buf: String,
}

impl Transcripts {
    fn push(&mut self, role: &str, text: String) {
        let text = text.trim().to_string();
        if !text.is_empty() {
            self.entries.push(TranscriptEntry {
                role: role.into(),
                text,
                time: now_rfc3339(),
            });
        }
    }

    fn flush_assistant(&mut self, full: Option<String>) {
        let buf = std::mem::take(&mut self.assistant_buf);
        let text = full.filter(|t| !t.trim().is_empty()).unwrap_or(buf);
        self.push("assistant", text);
    }
}

/// Run the bridge until cancelled or the vendor ends the session.
pub async fn run_bridge(
    mut protocol: Box<dyn RealtimeProtocol>,
    ws: WsStream,
    mut media: MediaPort,
    tools: ToolDispatcher,
    ready_timeout: Duration,
    cancel: CancellationToken,
) -> BridgeResult {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let in_rate = protocol.input_rate();
    let mut down = DownResampler::new(protocol.output_rate(), SIP_RATE);
    let mut transcripts = Transcripts {
        entries: Vec::new(),
        assistant_buf: String::new(),
    };
    let mut playout: VecDeque<i16> = VecDeque::new();
    let mut up_buf: Vec<i16> = Vec::with_capacity(frame_samples(in_rate));
    let mut held: VecDeque<i16> = VecDeque::new();
    let held_max = in_rate as usize * HELD_MAX_MS / 1000;
    let silence = vec![0i16; SIP_FRAME_SAMPLES];
    let (tool_tx, mut tool_rx) = mpsc::channel::<Vec<ToolOutput>>(8);
    let mut pending_calls: Vec<ToolCall> = Vec::new();

    let mut ready = protocol.ready_on_open();
    let ready_deadline = Instant::now() + ready_timeout;
    let mut response_active = false;
    // After we cancel a response, drop its trailing audio until the vendor
    // acknowledges (response done/cancelled) or starts a new response.
    let mut suppress_audio = false;
    let mut hangup_at: Option<Instant> = None;

    if let Err(e) = send_all(&mut ws_tx, protocol.session_open()).await {
        return BridgeResult {
            end: BridgeEnd::WsError(e),
            transcripts: transcripts.entries,
        };
    }
    if ready && let Err(e) = send_all(&mut ws_tx, protocol.greeting()).await {
        return BridgeResult {
            end: BridgeEnd::WsError(e),
            transcripts: transcripts.entries,
        };
    }

    let mut ticker = tokio::time::interval(Duration::from_millis(FRAME_MS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Burst);

    let end = 'run: loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break 'run BridgeEnd::Cancelled,

            _ = ticker.tick() => {
                // ── uplink: exactly one frame per tick ──
                let mut frame = media.uplink.try_recv().ok();
                while media.uplink.len() > UPLINK_MAX_BACKLOG {
                    frame = media.uplink.try_recv().ok();
                }
                if ready {
                    let pcm = frame.as_deref().unwrap_or(&silence);
                    resample_linear_into(pcm, SIP_RATE, in_rate, &mut up_buf);
                    if protocol.hold_uplink() {
                        held.extend(up_buf.iter().copied());
                        if held.len() > held_max {
                            let excess = held.len() - held_max;
                            held.drain(..excess);
                        }
                    } else {
                        let mut msgs = Vec::new();
                        if !held.is_empty() {
                            debug!(samples = held.len(), "realtime: replaying held uplink");
                            let chunk = in_rate as usize * REPLAY_CHUNK_MS / 1000;
                            let backlog: Vec<i16> = held.drain(..).collect();
                            for c in backlog.chunks(chunk) {
                                msgs.extend(protocol.encode_audio(c));
                            }
                        }
                        msgs.extend(protocol.encode_audio(&up_buf));
                        if let Err(e) = send_all(&mut ws_tx, msgs).await {
                            break 'run BridgeEnd::WsError(e);
                        }
                    }
                } else if Instant::now() >= ready_deadline {
                    break 'run BridgeEnd::ReadyTimeout;
                }

                // ── downlink: exactly one frame per tick ──
                let out: Vec<i16> = if playout.is_empty() {
                    silence.clone()
                } else {
                    let n = playout.len().min(SIP_FRAME_SAMPLES);
                    let mut f: Vec<i16> = playout.drain(..n).collect();
                    f.resize(SIP_FRAME_SAMPLES, 0);
                    f
                };
                let _ = media.downlink.try_send(out);

                if let Some(at) = hangup_at
                    && (playout.is_empty() || Instant::now() >= at)
                {
                    break 'run BridgeEnd::AgentHangup;
                }
            }

            msg = ws_rx.next() => {
                let msg = match msg {
                    None => break 'run BridgeEnd::WsClosed,
                    Some(Err(e)) => break 'run BridgeEnd::WsError(e.to_string()),
                    Some(Ok(m)) => m,
                };
                let events = match &msg {
                    Message::Text(t) => protocol.decode_text(t),
                    Message::Binary(b) => protocol.decode_binary(b),
                    Message::Close(frame) => {
                        info!(?frame, "realtime: server closed websocket");
                        break 'run BridgeEnd::WsClosed;
                    }
                    _ => Vec::new(),
                };
                let reactions = protocol.take_uplink();
                if !reactions.is_empty()
                    && let Err(e) = send_all(&mut ws_tx, reactions).await
                {
                    break 'run BridgeEnd::WsError(e);
                }
                for ev in events {
                    match ev {
                        DownlinkEvent::SessionReady => {
                            if !ready {
                                ready = true;
                                info!(provider = protocol.name(), "realtime session ready");
                                if let Err(e) = send_all(&mut ws_tx, protocol.greeting()).await {
                                    break 'run BridgeEnd::WsError(e);
                                }
                            }
                        }
                        DownlinkEvent::Audio(pcm) => {
                            if suppress_audio {
                                continue;
                            }
                            response_active = true;
                            let pcm8 = down.push(&pcm);
                            if playout.len() + pcm8.len() <= PLAYOUT_MAX_SAMPLES {
                                playout.extend(pcm8);
                            }
                        }
                        DownlinkEvent::ResponseStarted => {
                            suppress_audio = false;
                            response_active = true;
                        }
                        DownlinkEvent::ResponseDone => {
                            suppress_audio = false;
                            response_active = false;
                            if !pending_calls.is_empty() {
                                spawn_tools(&tools, std::mem::take(&mut pending_calls), &tool_tx);
                            }
                        }
                        DownlinkEvent::BargeIn => {
                            let speaking = response_active || !playout.is_empty();
                            debug!(speaking, "realtime: barge-in");
                            playout.clear();
                            down.clear();
                            if speaking {
                                let cancel_msgs = protocol.cancel_response();
                                if !cancel_msgs.is_empty() {
                                    suppress_audio = true;
                                    if let Err(e) = send_all(&mut ws_tx, cancel_msgs).await {
                                        break 'run BridgeEnd::WsError(e);
                                    }
                                }
                            }
                            response_active = false;
                        }
                        DownlinkEvent::UserTranscript(t) => transcripts.push("user", t),
                        DownlinkEvent::AssistantTextDelta(d) => transcripts.assistant_buf.push_str(&d),
                        DownlinkEvent::AssistantTextDone(full) => transcripts.flush_assistant(full),
                        DownlinkEvent::ToolCalls { calls, complete } => {
                            pending_calls.extend(calls);
                            if complete {
                                spawn_tools(&tools, std::mem::take(&mut pending_calls), &tool_tx);
                            }
                        }
                        DownlinkEvent::SessionClosed => break 'run BridgeEnd::WsClosed,
                        DownlinkEvent::Hangup => {
                            info!("realtime: endpoint requested hangup");
                            hangup_at.get_or_insert(Instant::now() + HANGUP_DRAIN_MAX);
                        }
                        DownlinkEvent::Error { message, fatal } => {
                            warn!(provider = protocol.name(), %message, fatal, "realtime vendor error");
                            if fatal {
                                break 'run BridgeEnd::VendorError(message);
                            }
                        }
                    }
                }
            }

            Some(outputs) = tool_rx.recv() => {
                if let Err(e) = send_all(&mut ws_tx, protocol.encode_tool_outputs(&outputs)).await {
                    break 'run BridgeEnd::WsError(e);
                }
            }
        }
    };

    if !transcripts.assistant_buf.trim().is_empty() {
        transcripts.flush_assistant(None);
    }

    // Graceful vendor close when we initiated the end.
    if matches!(
        end,
        BridgeEnd::Cancelled | BridgeEnd::AgentHangup | BridgeEnd::ReadyTimeout
    ) {
        let close_msgs = protocol.close();
        let wait_ack = !close_msgs.is_empty();
        if send_all(&mut ws_tx, close_msgs).await.is_ok() && wait_ack {
            let _ = tokio::time::timeout(CLOSE_ACK_TIMEOUT, async {
                while let Some(Ok(msg)) = ws_rx.next().await {
                    if let Message::Text(t) = &msg
                        && protocol
                            .decode_text(t)
                            .contains(&DownlinkEvent::SessionClosed)
                    {
                        debug!("realtime: session closed acknowledged");
                        break;
                    }
                    if matches!(msg, Message::Close(_)) {
                        break;
                    }
                }
            })
            .await;
        }
        let _ = ws_tx.send(Message::Close(None)).await;
    }
    let _ = ws_tx.close().await;

    info!(provider = protocol.name(), reason = end.reason(), detail = ?end, "realtime bridge ended");
    BridgeResult {
        end,
        transcripts: transcripts.entries,
    }
}

fn spawn_tools(tools: &ToolDispatcher, calls: Vec<ToolCall>, tx: &mpsc::Sender<Vec<ToolOutput>>) {
    let tools = tools.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let outputs = tools.execute_all(&calls).await;
        let _ = tx.send(outputs).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_resampler_keeps_remainder() {
        let mut r = DownResampler::new(24000, 8000);
        let a = r.push(&[0, 1, 2, 3, 4]); // 3 usable → 1 sample, carry 2
        assert_eq!(a, vec![0]);
        let b = r.push(&[5]); // carry 3,4,5 → 1 sample
        assert_eq!(b, vec![3]);
        let mut same = DownResampler::new(8000, 8000);
        assert_eq!(same.push(&[1, 2, 3]), vec![1, 2, 3]);
    }

    #[test]
    fn end_reasons() {
        assert_eq!(BridgeEnd::WsClosed.reason(), "ws_closed");
        assert_eq!(BridgeEnd::AgentHangup.reason(), "agent_hangup");
    }
}
