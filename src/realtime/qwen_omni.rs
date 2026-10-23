//! Aliyun Bailian Qwen-Omni realtime (default model `qwen3.8-omni-flash-realtime`).
//!
//! Verified against https://help.aliyun.com/zh/model-studio/client-events and
//! https://help.aliyun.com/zh/model-studio/server-events (2026-10), plus live
//! calls against `qwen3.8-omni-flash-realtime` (实测 2026-10-22):
//! - URL `wss://{WorkspaceId}.{region}.maas.aliyuncs.com/api-ws/v1/realtime?model=<model>`
//!   (configured as one `url`; `?model=` appended when absent),
//!   header `Authorization: Bearer <DashScope key>`.
//! - `session.update` first; input must not be sent before `session.updated`.
//! - PCM16 mono at **8 kHz both ways**: the docs only list 16 kHz in / 24 kHz
//!   out for 3.8, but `sample_rate: 8000` is accepted and echoed in
//!   `session.updated`, and both directions work — no resampling needed.
//! - A bare `response.create` on a fresh session is rejected ("Cannot create
//!   response without input, history, or instructions"), even with
//!   instructions set. The greeting therefore runs as: session with
//!   `turn_detection: null` + "say exactly" directive → append 300 ms of
//!   silence + `input_audio_buffer.commit` → on `committed`,
//!   `response.create` → on that response's `response.done`, `session.update`
//!   back to VAD with the plain instructions → on `session.updated`, release
//!   the caller audio held meanwhile. Sending caller audio while in manual
//!   mode makes the server drop the connection, hence the hold.
//! - Tools: the documented `{type:"function", function:{name,...}}` shape works.
//! - Tool output: `conversation.item.create {item:{type:"function_call_output",...}}`
//!   for every call, then one `response.create`.

use super::{
    DownlinkEvent, RealtimeProtocol, ToolCall, ToolOutput, UplinkMessage, b64_pcm, pcm_b64,
    str_field,
};
use crate::config::AgentConfig;
use serde_json::{Value, json};

pub const DEFAULT_MODEL: &str = "qwen3.8-omni-flash-realtime";
pub const DEFAULT_VOICE: &str = "Tina";
pub const SAMPLE_RATE: u32 = 8000;
pub const ASR_MODEL: &str = "qwen3-asr-flash-realtime";
/// Turn detection outside the greeting phase (fixed, not configurable).
pub const TURN_DETECTION: &str = "server_vad";
/// Silence committed as the (empty) caller turn that the greeting answers.
const GREETING_SILENCE_SAMPLES: usize = SAMPLE_RATE as usize * 300 / 1000;

/// Session lifecycle; see the module docs for the greeting sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Initial `session.update` in flight.
    Init,
    /// Session configured for the greeting; waiting for `greeting()`.
    GreetingReady,
    /// Silence committed; waiting for `input_audio_buffer.committed`.
    Committing,
    /// `response.create` sent; waiting for `response.created`.
    Creating,
    /// Greeting response in progress.
    Speaking(String),
    /// VAD-restoring `session.update` in flight.
    Restoring,
    /// Normal conversation.
    Live,
}

pub struct QwenOmni {
    url: String,
    api_key: String,
    extra_headers: Vec<(String, String)>,
    voice: String,
    instructions: Option<String>,
    greeting: Option<String>,
    tools: Vec<crate::config::ToolDef>,
    phase: Phase,
    pending: Vec<UplinkMessage>,
}

impl QwenOmni {
    pub fn new(agent: &AgentConfig, api_key: String) -> Self {
        let model = agent
            .model
            .clone()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        // Config validation guarantees `url` is set for qwen_omni.
        let base = agent.url.clone().unwrap_or_default();
        let url = if base.contains("model=") {
            base
        } else {
            let sep = if base.contains('?') { '&' } else { '?' };
            format!("{base}{sep}model={model}")
        };
        Self {
            url,
            api_key,
            extra_headers: agent
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            voice: agent
                .voice
                .clone()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_VOICE.to_string()),
            instructions: agent.instructions.clone().filter(|s| !s.is_empty()),
            greeting: agent.greeting.clone().filter(|s| !s.is_empty()),
            tools: agent.tools.clone(),
            phase: Phase::Init,
            pending: Vec::new(),
        }
    }

    /// Full session configuration. `greeting` = manual turn detection plus
    /// the verbatim-greeting directive; otherwise VAD and plain instructions.
    fn session_update(&self, greeting: Option<&str>) -> UplinkMessage {
        let format = json!({"type": "pcm", "sample_rate": SAMPLE_RATE});
        let mut session = json!({
            "modalities": ["text", "audio"],
            "voice": self.voice,
            "audio": {
                "input": {"format": format},
                "output": {"format": format, "voice": self.voice},
            },
            "turn_detection": match greeting {
                Some(_) => Value::Null,
                None => json!({"type": TURN_DETECTION}),
            },
            "input_audio_transcription": {"model": ASR_MODEL},
        });
        let instructions = match (&self.instructions, greeting) {
            (Some(i), Some(g)) => Some(format!("{i}\n\n{}", say_directive(g))),
            (None, Some(g)) => Some(say_directive(g)),
            (Some(i), None) => Some(i.clone()),
            (None, None) => None,
        };
        if let Some(i) = instructions {
            session["instructions"] = json!(i);
        }
        if !self.tools.is_empty() {
            session["tools"] = Value::Array(
                self.tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.parameters,
                            }
                        })
                    })
                    .collect(),
            );
        }
        UplinkMessage::json(json!({"type": "session.update", "session": session}))
    }

    /// Leave the greeting: back to VAD; caller audio is released once the
    /// server confirms.
    fn restore_vad(&mut self) {
        self.phase = Phase::Restoring;
        let msg = self.session_update(None);
        self.pending.push(msg);
    }
}

/// Instruct the model to speak `text` verbatim as its next turn. Describing
/// the situation alone is not enough: the exact words must be given, and the
/// committed silence must be named as such, else the model answers it.
fn say_directive(text: &str) -> String {
    format!(
        "The caller has not said anything yet (what you received is silence; do not treat it as their answer). \
         Your next utterance must be exactly: \u{300c}{text}\u{300d} without adding, removing or changing any word."
    )
}

impl RealtimeProtocol for QwenOmni {
    fn name(&self) -> &'static str {
        "qwen_omni"
    }
    fn input_rate(&self) -> u32 {
        SAMPLE_RATE
    }
    fn output_rate(&self) -> u32 {
        SAMPLE_RATE
    }
    fn connect_url(&self) -> String {
        self.url.clone()
    }
    fn headers(&self) -> Vec<(String, String)> {
        let mut h = vec![(
            "Authorization".to_string(),
            format!("Bearer {}", self.api_key),
        )];
        h.extend(self.extra_headers.iter().cloned());
        h
    }
    fn ready_on_open(&self) -> bool {
        false
    }

    fn session_open(&self) -> Vec<UplinkMessage> {
        vec![self.session_update(self.greeting.as_deref())]
    }

    fn greeting(&mut self) -> Vec<UplinkMessage> {
        if self.phase != Phase::GreetingReady {
            return Vec::new();
        }
        self.phase = Phase::Committing;
        vec![
            UplinkMessage::json(json!({
                "type": "input_audio_buffer.append",
                "audio": pcm_b64(&[0i16; GREETING_SILENCE_SAMPLES]),
            })),
            UplinkMessage::json(json!({"type": "input_audio_buffer.commit"})),
        ]
    }

    fn encode_audio(&self, pcm: &[i16]) -> Vec<UplinkMessage> {
        vec![UplinkMessage::json(json!({
            "type": "input_audio_buffer.append",
            "audio": pcm_b64(pcm),
        }))]
    }

    fn cancel_response(&self) -> Vec<UplinkMessage> {
        vec![UplinkMessage::json(json!({"type": "response.cancel"}))]
    }

    fn encode_tool_outputs(&self, outputs: &[ToolOutput]) -> Vec<UplinkMessage> {
        if outputs.is_empty() {
            return Vec::new();
        }
        let mut msgs: Vec<UplinkMessage> = outputs
            .iter()
            .map(|o| {
                UplinkMessage::json(json!({
                    "type": "conversation.item.create",
                    "item": {
                        "type": "function_call_output",
                        "call_id": o.call_id,
                        "output": o.output,
                    }
                }))
            })
            .collect();
        msgs.push(UplinkMessage::json(json!({"type": "response.create"})));
        msgs
    }

    fn close(&self) -> Vec<UplinkMessage> {
        // No session-close event is documented; a WebSocket close frame suffices.
        Vec::new()
    }

    fn decode_text(&mut self, text: &str) -> Vec<DownlinkEvent> {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return Vec::new();
        };
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "session.updated" => match self.phase {
                Phase::Init => {
                    self.phase = if self.greeting.is_some() {
                        Phase::GreetingReady
                    } else {
                        Phase::Live
                    };
                    vec![DownlinkEvent::SessionReady]
                }
                Phase::Restoring => {
                    self.phase = Phase::Live;
                    Vec::new()
                }
                _ => Vec::new(),
            },
            "input_audio_buffer.committed" => {
                if self.phase == Phase::Committing {
                    self.phase = Phase::Creating;
                    self.pending
                        .push(UplinkMessage::json(json!({"type": "response.create"})));
                }
                Vec::new()
            }
            "response.created" => {
                if self.phase == Phase::Creating {
                    let id = v
                        .pointer("/response/id")
                        .and_then(|x| x.as_str())
                        .unwrap_or_default();
                    self.phase = Phase::Speaking(id.to_string());
                }
                vec![DownlinkEvent::ResponseStarted]
            }
            "response.audio.delta" => str_field(&v, "delta")
                .and_then(|d| b64_pcm(&d))
                .filter(|p| !p.is_empty())
                .map(DownlinkEvent::Audio)
                .into_iter()
                .collect(),
            "response.audio_transcript.delta" => str_field(&v, "delta")
                .map(DownlinkEvent::AssistantTextDelta)
                .into_iter()
                .collect(),
            "response.audio_transcript.done" => {
                vec![DownlinkEvent::AssistantTextDone(
                    str_field(&v, "transcript").map(|t| t.trim().to_string()),
                )]
            }
            // The greeting's committed silence yields an empty transcript.
            "conversation.item.input_audio_transcription.completed" => str_field(&v, "transcript")
                .filter(|t| !t.is_empty())
                .map(DownlinkEvent::UserTranscript)
                .into_iter()
                .collect(),
            "input_audio_buffer.speech_started" => vec![DownlinkEvent::BargeIn],
            "response.function_call_arguments.done" => {
                let call = ToolCall {
                    call_id: str_field(&v, "call_id").unwrap_or_default(),
                    name: str_field(&v, "name").unwrap_or_default(),
                    arguments: str_field(&v, "arguments").unwrap_or_default(),
                };
                vec![DownlinkEvent::ToolCalls {
                    calls: vec![call],
                    complete: false,
                }]
            }
            "response.done" => {
                if matches!(self.phase, Phase::Speaking(_)) {
                    self.restore_vad();
                }
                vec![DownlinkEvent::ResponseDone]
            }
            "error" => {
                let e = v.get("error").cloned().unwrap_or(Value::Null);
                let message = format!(
                    "{}: {}",
                    e.get("code")
                        .or_else(|| e.get("type"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("error"),
                    e.get("message").and_then(|c| c.as_str()).unwrap_or("")
                );
                // A rejected session config is fatal. Other errors include
                // benign ones (e.g. cancel with no active response); a broken
                // session closes the socket anyway.
                let fatal = self.phase == Phase::Init;
                if matches!(self.phase, Phase::Committing | Phase::Creating) {
                    // Greeting failed to start: drop it rather than keep the
                    // caller's audio on hold forever.
                    self.restore_vad();
                }
                vec![DownlinkEvent::Error { message, fatal }]
            }
            _ => Vec::new(),
        }
    }

    fn decode_binary(&self, _bytes: &[u8]) -> Vec<DownlinkEvent> {
        Vec::new()
    }

    fn take_uplink(&mut self) -> Vec<UplinkMessage> {
        std::mem::take(&mut self.pending)
    }

    fn hold_uplink(&self) -> bool {
        self.phase != Phase::Live
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn proto(extra: &str) -> QwenOmni {
        let cfg = Config::parse(&format!(
            "[[agent]]\nname='q'\nmatch='*'\nprovider='qwen_omni'\nurl='wss://ws1.cn-beijing.maas.aliyuncs.com/api-ws/v1/realtime'\napi_key='sk-x'\n{extra}"
        ))
        .unwrap();
        let a = &cfg.agents[0];
        QwenOmni::new(a, a.resolve_api_key().unwrap())
    }

    fn json_of(m: &UplinkMessage) -> Value {
        match m {
            UplinkMessage::Text(t) => serde_json::from_str(t).unwrap(),
            _ => panic!("expected text"),
        }
    }

    fn types(msgs: &[UplinkMessage]) -> Vec<String> {
        msgs.iter()
            .map(|m| json_of(m)["type"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn url_and_auth() {
        let p = proto("");
        assert_eq!(
            p.connect_url(),
            "wss://ws1.cn-beijing.maas.aliyuncs.com/api-ws/v1/realtime?model=qwen3.8-omni-flash-realtime"
        );
        assert_eq!(
            p.headers()[0],
            ("Authorization".into(), "Bearer sk-x".into())
        );
        let p = proto("model='m2'");
        assert_eq!(
            p.connect_url(),
            "wss://ws1.cn-beijing.maas.aliyuncs.com/api-ws/v1/realtime?model=m2"
        );
        assert!(!p.ready_on_open());
        assert_eq!((p.input_rate(), p.output_rate()), (8000, 8000));
    }

    #[test]
    fn session_update_shape_with_greeting() {
        let p = proto(
            "instructions='be nice'\ngreeting='您好'\n[[agent.tools]]\nname='get_weather'\ndescription='w'\nparameters={type='object',properties={city={type='string'}}}\n",
        );
        let msgs = p.session_open();
        assert_eq!(msgs.len(), 1);
        let v = json_of(&msgs[0]);
        assert_eq!(v["type"], "session.update");
        let s = &v["session"];
        assert_eq!(s["modalities"], json!(["text", "audio"]));
        assert_eq!(s["voice"], "Tina");
        assert_eq!(s["audio"]["output"]["voice"], "Tina");
        assert_eq!(s["audio"]["input"]["format"]["type"], "pcm");
        assert_eq!(s["audio"]["input"]["format"]["sample_rate"], 8000);
        assert_eq!(s["audio"]["output"]["format"]["sample_rate"], 8000);
        // Greeting runs in manual mode.
        assert!(s["turn_detection"].is_null());
        assert_eq!(
            s["input_audio_transcription"]["model"],
            "qwen3-asr-flash-realtime"
        );
        let i = s["instructions"].as_str().unwrap();
        assert!(i.starts_with("be nice"));
        assert!(i.contains("「您好」"));
        assert_eq!(s["tools"][0]["type"], "function");
        assert_eq!(s["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(
            s["tools"][0]["function"]["parameters"]["properties"]["city"]["type"],
            "string"
        );
    }

    #[test]
    fn greeting_sequence() {
        let mut p = proto("instructions='be nice'\ngreeting='您好'");
        assert!(p.hold_uplink());
        assert!(p.greeting().is_empty(), "not before session.updated");
        assert_eq!(
            p.decode_text(r#"{"type":"session.updated","session":{}}"#),
            vec![DownlinkEvent::SessionReady]
        );
        let g = p.greeting();
        assert_eq!(
            types(&g),
            ["input_audio_buffer.append", "input_audio_buffer.commit"]
        );
        let silence = b64_pcm(json_of(&g[0])["audio"].as_str().unwrap()).unwrap();
        assert_eq!(silence.len(), 2400);
        assert!(silence.iter().all(|s| *s == 0));
        assert!(p.greeting().is_empty(), "only once");
        assert!(p.hold_uplink());

        assert!(
            p.decode_text(r#"{"type":"input_audio_buffer.committed","item_id":"i1"}"#)
                .is_empty()
        );
        assert_eq!(types(&p.take_uplink()), ["response.create"]);
        assert_eq!(
            p.decode_text(r#"{"type":"response.created","response":{"id":"r1"}}"#),
            vec![DownlinkEvent::ResponseStarted]
        );
        // The silence's empty transcript is not a caller utterance.
        assert!(p
            .decode_text(r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"i1","content_index":0,"transcript":""}"#)
            .is_empty());
        assert!(p.take_uplink().is_empty());
        assert_eq!(
            p.decode_text(
                r#"{"type":"response.done","response":{"id":"r1","status":"completed"}}"#
            ),
            vec![DownlinkEvent::ResponseDone]
        );
        let restore = p.take_uplink();
        assert_eq!(types(&restore), ["session.update"]);
        let s = &json_of(&restore[0])["session"];
        assert_eq!(s["turn_detection"]["type"], "server_vad");
        assert_eq!(s["instructions"], "be nice");
        assert!(p.hold_uplink(), "held until the server confirms");
        assert!(
            p.decode_text(r#"{"type":"session.updated","session":{}}"#)
                .is_empty()
        );
        assert!(!p.hold_uplink());
        // Later responses don't touch the session.
        p.decode_text(r#"{"type":"response.created","response":{"id":"r2"}}"#);
        p.decode_text(r#"{"type":"response.done","response":{"id":"r2"}}"#);
        assert!(p.take_uplink().is_empty());
    }

    #[test]
    fn greeting_error_releases_hold() {
        let mut p = proto("greeting='您好'");
        p.decode_text(r#"{"type":"session.updated"}"#);
        p.greeting();
        let ev = p.decode_text(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"nope"}}"#,
        );
        assert_eq!(
            ev,
            vec![DownlinkEvent::Error {
                message: "invalid_request_error: nope".into(),
                fatal: false,
            }]
        );
        let restore = p.take_uplink();
        assert_eq!(types(&restore), ["session.update"]);
        assert!(
            json_of(&restore[0])["session"]
                .get("instructions")
                .is_none()
        );
        p.decode_text(r#"{"type":"session.updated"}"#);
        assert!(!p.hold_uplink());
    }

    #[test]
    fn error_before_ready_is_fatal() {
        let mut p = proto("");
        let ev = p.decode_text(r#"{"type":"error","error":{"code":"bad","message":"x"}}"#);
        assert_eq!(
            ev,
            vec![DownlinkEvent::Error {
                message: "bad: x".into(),
                fatal: true,
            }]
        );
    }

    #[test]
    fn no_greeting_goes_live() {
        let mut p = proto("");
        let v = json_of(&p.session_open()[0]);
        assert!(v["session"].get("tools").is_none());
        assert!(v["session"].get("instructions").is_none());
        assert_eq!(v["session"]["turn_detection"]["type"], "server_vad");
        assert_eq!(
            p.decode_text(r#"{"type":"session.updated"}"#),
            vec![DownlinkEvent::SessionReady]
        );
        assert!(!p.hold_uplink());
        assert!(p.greeting().is_empty());
        p.decode_text(r#"{"type":"response.created","response":{"id":"r1"}}"#);
        p.decode_text(r#"{"type":"response.done","response":{"id":"r1"}}"#);
        assert!(p.take_uplink().is_empty());
    }

    #[test]
    fn audio_append_and_tool_outputs() {
        let p = proto("");
        let v = json_of(&p.encode_audio(&[1, 2])[0]);
        assert_eq!(v["type"], "input_audio_buffer.append");
        assert_eq!(b64_pcm(v["audio"].as_str().unwrap()).unwrap(), vec![1, 2]);
        let out = p.encode_tool_outputs(&[
            ToolOutput {
                call_id: "c1".into(),
                output: "{\"t\":1}".into(),
            },
            ToolOutput {
                call_id: "c2".into(),
                output: "ok".into(),
            },
        ]);
        assert_eq!(out.len(), 3);
        let first = json_of(&out[0]);
        assert_eq!(first["type"], "conversation.item.create");
        assert_eq!(first["item"]["type"], "function_call_output");
        assert_eq!(first["item"]["call_id"], "c1");
        assert_eq!(first["item"]["output"], "{\"t\":1}");
        assert_eq!(json_of(&out[2]), json!({"type": "response.create"}));
        assert_eq!(
            json_of(&p.cancel_response()[0]),
            json!({"type": "response.cancel"})
        );
        assert!(p.close().is_empty());
    }

    /// Event payloads below are trimmed copies of frames captured live
    /// (实测 2026-10-22).
    #[test]
    fn decodes_server_events() {
        let mut p = proto("");
        p.decode_text(r#"{"type":"session.updated"}"#);
        let delta = pcm_b64(&[5, -5]);
        let cases: Vec<(String, Vec<DownlinkEvent>)> = vec![
            (
                r#"{"event_id":"e1","type":"session.created","session":{"id":"s"}}"#.into(),
                vec![],
            ),
            (
                r#"{"type":"response.created","response":{"id":"resp_1","object":"realtime.response","status":"in_progress","modalities":["text","audio"],"voice":"Tina","output_audio_format":"pcm","output":[]}}"#
                    .into(),
                vec![DownlinkEvent::ResponseStarted],
            ),
            (
                format!(
                    r#"{{"type":"response.audio.delta","response_id":"resp_1","item_id":"i","output_index":0,"content_index":0,"delta":"{delta}"}}"#
                ),
                vec![DownlinkEvent::Audio(vec![5, -5])],
            ),
            (
                r#"{"type":"response.audio_transcript.delta","response_id":"resp_1","item_id":"i","output_index":0,"content_index":0,"delta":"订单"}"#
                    .into(),
                vec![DownlinkEvent::AssistantTextDelta("订单".into())],
            ),
            (
                r#"{"type":"response.audio_transcript.done","response_id":"resp_1","item_id":"i","output_index":0,"content_index":0,"transcript":"订单12345已发货，预计明天送达。\n"}"#
                    .into(),
                vec![DownlinkEvent::AssistantTextDone(Some(
                    "订单12345已发货，预计明天送达。".into(),
                ))],
            ),
            (
                r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"i","content_index":0,"text":"你好","stash":"，我想","language":"zh","emotion":"neutral"}"#
                    .into(),
                vec![],
            ),
            (
                r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"i","content_index":0,"transcript":"你好，我想查一下订单，订单号是12345。","language":"zh","emotion":"neutral"}"#
                    .into(),
                vec![DownlinkEvent::UserTranscript(
                    "你好，我想查一下订单，订单号是12345。".into(),
                )],
            ),
            (
                r#"{"type":"input_audio_buffer.speech_started","audio_start_ms":0,"item_id":"i"}"#
                    .into(),
                vec![DownlinkEvent::BargeIn],
            ),
            (
                r#"{"type":"input_audio_buffer.speech_stopped","audio_end_ms":5040,"item_id":"i"}"#
                    .into(),
                vec![],
            ),
            (
                r#"{"type":"response.function_call_arguments.done","response_id":"resp_2","item_id":"i","output_index":0,"name":"query_order","call_id":"call_1b2f","arguments":"{\"order_id\": \"12345\"}"}"#
                    .into(),
                vec![DownlinkEvent::ToolCalls {
                    calls: vec![ToolCall {
                        call_id: "call_1b2f".into(),
                        name: "query_order".into(),
                        arguments: "{\"order_id\": \"12345\"}".into(),
                    }],
                    complete: false,
                }],
            ),
            (
                r#"{"type":"response.done","response":{"id":"resp_2","status":"completed","usage":{"total_tokens":10}}}"#
                    .into(),
                vec![DownlinkEvent::ResponseDone],
            ),
            (
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"Cannot create response without input, history, or instructions","param":"response"}}"#
                    .into(),
                vec![DownlinkEvent::Error {
                    message: "invalid_request_error: Cannot create response without input, history, or instructions".into(),
                    fatal: false,
                }],
            ),
            ("garbage".into(), vec![]),
        ];
        for (input, want) in cases {
            assert_eq!(p.decode_text(&input), want, "input: {input}");
        }
        assert!(p.take_uplink().is_empty());
    }
}
