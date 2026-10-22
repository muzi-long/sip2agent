//! Generic raw-PCM WebSocket protocol (same wire as rustpbx `pcm.rs`, no DTMF).
//!
//! - Uplink: binary frames, headerless PCM16-LE mono at `sample_rate`
//!   (default 8000 → no resampling), one frame per 20 ms including silence.
//! - Downlink: binary frames, same encoding.
//! - Text `{"type":"clear"}` from the server = barge-in (clear playout).
//! - Text `{"type":"hangup"}` from the server = hang up the call.

use super::{DownlinkEvent, RealtimeProtocol, ToolOutput, UplinkMessage};
use crate::audio::{bytes_to_pcm, pcm_to_bytes};
use crate::config::AgentConfig;

pub const DEFAULT_RATE: u32 = 8000;

pub struct PcmWs {
    url: String,
    rate: u32,
    headers: Vec<(String, String)>,
}

impl PcmWs {
    pub fn new(agent: &AgentConfig, api_key: Option<String>) -> Self {
        let mut headers: Vec<(String, String)> = agent
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if let Some(key) = api_key {
            headers.push(("Authorization".into(), format!("Bearer {key}")));
        }
        Self {
            url: agent.url.clone().unwrap_or_default(),
            rate: agent.sample_rate.unwrap_or(DEFAULT_RATE),
            headers,
        }
    }
}

impl RealtimeProtocol for PcmWs {
    fn name(&self) -> &'static str {
        "pcm_ws"
    }
    fn input_rate(&self) -> u32 {
        self.rate
    }
    fn output_rate(&self) -> u32 {
        self.rate
    }
    fn connect_url(&self) -> String {
        self.url.clone()
    }
    fn headers(&self) -> Vec<(String, String)> {
        self.headers.clone()
    }
    fn ready_on_open(&self) -> bool {
        true
    }
    fn session_open(&self) -> Vec<UplinkMessage> {
        Vec::new()
    }
    fn greeting(&self) -> Vec<UplinkMessage> {
        Vec::new()
    }
    fn encode_audio(&self, pcm: &[i16]) -> Vec<UplinkMessage> {
        vec![UplinkMessage::Binary(pcm_to_bytes(pcm))]
    }
    fn cancel_response(&self) -> Vec<UplinkMessage> {
        Vec::new()
    }
    fn encode_tool_outputs(&self, _outputs: &[ToolOutput]) -> Vec<UplinkMessage> {
        Vec::new()
    }
    fn close(&self) -> Vec<UplinkMessage> {
        Vec::new()
    }
    fn decode_text(&self, text: &str) -> Vec<DownlinkEvent> {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
            return Vec::new();
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("clear") => vec![DownlinkEvent::BargeIn],
            Some("hangup") => vec![DownlinkEvent::Hangup],
            _ => Vec::new(),
        }
    }
    fn decode_binary(&self, bytes: &[u8]) -> Vec<DownlinkEvent> {
        let pcm = bytes_to_pcm(bytes);
        if pcm.is_empty() {
            return Vec::new();
        }
        vec![DownlinkEvent::Audio(pcm)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn agent(extra: &str) -> AgentConfig {
        let cfg = Config::parse(&format!(
            "[[agent]]\nname='p'\nmatch='*'\nprovider='pcm_ws'\nurl='ws://127.0.0.1:9/x'\n{extra}"
        ))
        .unwrap();
        cfg.agents[0].clone()
    }

    #[test]
    fn uplink_is_headerless_pcm16le() {
        let p = PcmWs::new(&agent(""), None);
        assert_eq!(
            p.encode_audio(&[1, -2, 3]),
            vec![UplinkMessage::Binary(vec![1, 0, 0xFE, 0xFF, 3, 0])]
        );
        assert_eq!(p.input_rate(), 8000);
        assert_eq!(p.output_rate(), 8000);
        assert!(p.ready_on_open());
        assert!(p.session_open().is_empty());
    }

    #[test]
    fn downlink_binary_and_control_text() {
        let p = PcmWs::new(&agent(""), None);
        assert_eq!(
            p.decode_binary(&[7, 0, 0, 0]),
            vec![DownlinkEvent::Audio(vec![7, 0])]
        );
        assert!(p.decode_binary(&[0xAB]).is_empty());
        assert_eq!(
            p.decode_text(r#"{"type":"clear"}"#),
            vec![DownlinkEvent::BargeIn]
        );
        assert_eq!(
            p.decode_text(r#"{"type":"hangup"}"#),
            vec![DownlinkEvent::Hangup]
        );
        assert!(p.decode_text(r#"{"type":"dtmf","digit":"5"}"#).is_empty());
        assert!(p.decode_text("not json").is_empty());
    }

    #[test]
    fn headers_rate_and_auth() {
        let p = PcmWs::new(
            &agent("sample_rate=16000\nheaders={X-Custom='v'}\n"),
            Some("k1".into()),
        );
        assert_eq!(p.input_rate(), 16000);
        let h = p.headers();
        assert!(h.contains(&("X-Custom".into(), "v".into())));
        assert!(h.contains(&("Authorization".into(), "Bearer k1".into())));
    }
}
