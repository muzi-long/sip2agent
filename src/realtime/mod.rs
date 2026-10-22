//! Realtime speech-to-speech vendor adapters.
//!
//! Shape mirrors rustpbx's `RealtimeProtocol` + `run_realtime_bridge`: each
//! vendor adapter is a pure encode/decode object (unit-testable against
//! doc-shaped JSON), and [`bridge::run_bridge`] owns the WebSocket, the
//! 20 ms pacing, the playout buffer, barge-in and tool dispatch.

pub mod bridge;
pub mod doubao;
pub mod pcm_ws;
pub mod qwen_omni;

use crate::config::{AgentConfig, ProviderKind};
use anyhow::{Result, bail};

/// A message to write to the realtime WebSocket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UplinkMessage {
    Text(String),
    Binary(Vec<u8>),
}

impl UplinkMessage {
    pub fn json(v: serde_json::Value) -> Self {
        UplinkMessage::Text(v.to_string())
    }
}

/// One function call requested by the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    /// JSON-encoded arguments string, exactly as the model produced it.
    pub arguments: String,
}

/// Result of one tool call, returned to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub call_id: String,
    pub output: String,
}

/// Vendor-neutral downlink event.
#[derive(Debug, Clone, PartialEq)]
pub enum DownlinkEvent {
    /// Session configured; audio may be sent and the greeting triggered.
    SessionReady,
    /// Model audio, PCM16 mono at the protocol's `output_rate`.
    Audio(Vec<i16>),
    /// A new model response (turn) started.
    ResponseStarted,
    /// The current model response finished or was cancelled.
    ResponseDone,
    /// Caller started talking over the model: clear playout now.
    BargeIn,
    /// Final transcript of the caller's utterance.
    UserTranscript(String),
    /// Incremental assistant text / audio transcript.
    AssistantTextDelta(String),
    /// Assistant turn text finished (full text when the vendor provides it).
    AssistantTextDone(Option<String>),
    /// Function calls. `complete` = the batch is final (else more may follow
    /// until [`DownlinkEvent::ResponseDone`]).
    ToolCalls {
        calls: Vec<ToolCall>,
        complete: bool,
    },
    /// Server acknowledged our graceful close.
    SessionClosed,
    /// Endpoint asks us to hang up the call (after playout drains).
    Hangup,
    /// Vendor error event. `fatal` errors end the call.
    Error { message: String, fatal: bool },
}

/// Wire-protocol adapter for one realtime vendor.
pub trait RealtimeProtocol: Send + Sync {
    fn name(&self) -> &'static str;
    /// PCM rate the vendor expects on the uplink.
    fn input_rate(&self) -> u32;
    /// PCM rate of vendor audio on the downlink.
    fn output_rate(&self) -> u32;
    fn connect_url(&self) -> String;
    /// Upgrade request headers, including auth. Never logged.
    fn headers(&self) -> Vec<(String, String)>;
    /// Whether the session is usable right after the socket opens (else wait
    /// for [`DownlinkEvent::SessionReady`]).
    fn ready_on_open(&self) -> bool;
    /// Handshake messages sent immediately after connect.
    fn session_open(&self) -> Vec<UplinkMessage>;
    /// Opening-line messages, sent once when the session becomes ready.
    fn greeting(&self) -> Vec<UplinkMessage>;
    /// Encode one 20 ms uplink frame (PCM16 at `input_rate`).
    fn encode_audio(&self, pcm: &[i16]) -> Vec<UplinkMessage>;
    /// Messages to cancel the in-flight response on barge-in.
    fn cancel_response(&self) -> Vec<UplinkMessage>;
    /// Return tool results (one batch) to the model.
    fn encode_tool_outputs(&self, outputs: &[ToolOutput]) -> Vec<UplinkMessage>;
    /// Graceful-close messages. Non-empty means: wait for
    /// [`DownlinkEvent::SessionClosed`] (bounded) before closing the socket.
    fn close(&self) -> Vec<UplinkMessage>;
    fn decode_text(&self, text: &str) -> Vec<DownlinkEvent>;
    fn decode_binary(&self, bytes: &[u8]) -> Vec<DownlinkEvent>;
}

/// Build the adapter for an agent. Fails when a required API key is missing.
pub fn protocol_for(agent: &AgentConfig) -> Result<Box<dyn RealtimeProtocol>> {
    let key = agent.resolve_api_key();
    Ok(match agent.provider {
        ProviderKind::PcmWs => Box::new(pcm_ws::PcmWs::new(agent, key)),
        ProviderKind::QwenOmni => {
            let Some(key) = key else {
                bail!("agent '{}': API key not set", agent.name)
            };
            Box::new(qwen_omni::QwenOmni::new(agent, key))
        }
        ProviderKind::Doubao => {
            let Some(key) = key else {
                bail!("agent '{}': API key not set", agent.name)
            };
            Box::new(doubao::Doubao::new(agent, key))
        }
        ProviderKind::Echo => bail!("echo is not a realtime provider"),
    })
}

/// Parse a base64 PCM16-LE payload.
pub(crate) fn b64_pcm(b64: &str) -> Option<Vec<i16>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .ok()
        .map(|b| crate::audio::bytes_to_pcm(&b))
}

/// PCM16 → base64 of little-endian bytes.
pub(crate) fn pcm_b64(pcm: &[i16]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(crate::audio::pcm_to_bytes(pcm))
}

pub(crate) fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_string)
}
