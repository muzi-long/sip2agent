//! Aliyun Bailian Qwen-Omni realtime (default model `qwen3.8-omni-flash-realtime`).
//!
//! Verified against https://help.aliyun.com/zh/model-studio/client-events and
//! https://help.aliyun.com/zh/model-studio/server-events (2026-10):
//! - URL `wss://{WorkspaceId}.{region}.maas.aliyuncs.com/api-ws/v1/realtime?model=<model>`
//!   (configured as one `url`; `?model=` appended when absent),
//!   header `Authorization: Bearer <DashScope key>`.
//! - `session.update` first; input must not be sent before `session.updated`.
//! - Input PCM16 16 kHz mono, output PCM16 24 kHz.
//! - `response.create` takes no parameters, so the greeting is expressed in
//!   the session instructions and triggered with a bare `response.create`.
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
pub const INPUT_RATE: u32 = 16000;
pub const OUTPUT_RATE: u32 = 24000;
pub const ASR_MODEL: &str = "qwen3-asr-flash-realtime";

pub struct QwenOmni {
    url: String,
    api_key: String,
    extra_headers: Vec<(String, String)>,
    voice: String,
    instructions: Option<String>,
    greeting: Option<String>,
    turn_detection: String,
    tools: Vec<crate::config::ToolDef>,
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
            turn_detection: agent
                .turn_detection
                .clone()
                .unwrap_or_else(|| "server_vad".to_string()),
            tools: agent.tools.clone(),
        }
    }

    fn session_instructions(&self) -> Option<String> {
        match (&self.instructions, &self.greeting) {
            (Some(i), Some(g)) => Some(format!(
                "{i}\n\nWhen the call connects, open by saying exactly: {g}"
            )),
            (None, Some(g)) => Some(format!(
                "When the call connects, open by saying exactly: {g}"
            )),
            (Some(i), None) => Some(i.clone()),
            (None, None) => None,
        }
    }
}

impl RealtimeProtocol for QwenOmni {
    fn name(&self) -> &'static str {
        "qwen_omni"
    }
    fn input_rate(&self) -> u32 {
        INPUT_RATE
    }
    fn output_rate(&self) -> u32 {
        OUTPUT_RATE
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
        let mut session = json!({
            "modalities": ["text", "audio"],
            "voice": self.voice,
            "audio": {
                "input": {"format": {"type": "pcm", "sample_rate": INPUT_RATE}},
                "output": {
                    "format": {"type": "pcm", "sample_rate": OUTPUT_RATE},
                    "voice": self.voice,
                },
            },
            "turn_detection": {"type": self.turn_detection},
            "input_audio_transcription": {"model": ASR_MODEL},
        });
        if let Some(i) = self.session_instructions() {
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
        vec![UplinkMessage::json(
            json!({"type": "session.update", "session": session}),
        )]
    }

    fn greeting(&self) -> Vec<UplinkMessage> {
        if self.greeting.is_none() {
            return Vec::new();
        }
        vec![UplinkMessage::json(json!({"type": "response.create"}))]
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

    fn decode_text(&self, text: &str) -> Vec<DownlinkEvent> {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return Vec::new();
        };
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "session.updated" => vec![DownlinkEvent::SessionReady],
            "response.created" => vec![DownlinkEvent::ResponseStarted],
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
                vec![DownlinkEvent::AssistantTextDone(str_field(
                    &v,
                    "transcript",
                ))]
            }
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
            "response.done" => vec![DownlinkEvent::ResponseDone],
            "error" => {
                let e = v.get("error").cloned().unwrap_or(Value::Null);
                let message = format!(
                    "{}: {}",
                    e.get("code").and_then(|c| c.as_str()).unwrap_or("error"),
                    e.get("message").and_then(|c| c.as_str()).unwrap_or("")
                );
                // Qwen also reports benign client mistakes (e.g. cancel with no
                // active response) as `error`; fatal problems close the socket.
                vec![DownlinkEvent::Error {
                    message,
                    fatal: false,
                }]
            }
            _ => Vec::new(),
        }
    }

    fn decode_binary(&self, _bytes: &[u8]) -> Vec<DownlinkEvent> {
        Vec::new()
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
    }

    #[test]
    fn session_update_shape() {
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
        assert_eq!(s["audio"]["input"]["format"]["type"], "pcm");
        assert_eq!(s["audio"]["input"]["format"]["sample_rate"], 16000);
        assert_eq!(s["audio"]["output"]["format"]["sample_rate"], 24000);
        assert_eq!(s["turn_detection"]["type"], "server_vad");
        assert_eq!(
            s["input_audio_transcription"]["model"],
            "qwen3-asr-flash-realtime"
        );
        assert!(s["instructions"].as_str().unwrap().starts_with("be nice"));
        assert!(s["instructions"].as_str().unwrap().contains("您好"));
        assert_eq!(s["tools"][0]["type"], "function");
        assert_eq!(s["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(
            s["tools"][0]["function"]["parameters"]["properties"]["city"]["type"],
            "string"
        );
        let g = p.greeting();
        assert_eq!(json_of(&g[0]), json!({"type": "response.create"}));
    }

    #[test]
    fn no_greeting_no_tools() {
        let p = proto("turn_detection='semantic_vad'");
        let v = json_of(&p.session_open()[0]);
        assert!(v["session"].get("tools").is_none());
        assert!(v["session"].get("instructions").is_none());
        assert_eq!(v["session"]["turn_detection"]["type"], "semantic_vad");
        assert!(p.greeting().is_empty());
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

    #[test]
    fn decodes_server_events() {
        let p = proto("");
        let delta = pcm_b64(&[5, -5]);
        let cases: Vec<(String, Vec<DownlinkEvent>)> = vec![
            (
                r#"{"event_id":"e1","type":"session.created","session":{"id":"s"}}"#.into(),
                vec![],
            ),
            (
                r#"{"event_id":"e2","type":"session.updated","session":{"id":"s"}}"#.into(),
                vec![DownlinkEvent::SessionReady],
            ),
            (
                r#"{"type":"response.created","response":{"id":"r1","status":"in_progress"}}"#
                    .into(),
                vec![DownlinkEvent::ResponseStarted],
            ),
            (
                format!(
                    r#"{{"event_id":"e3","type":"response.audio.delta","response_id":"r1","item_id":"i","output_index":0,"content_index":0,"delta":"{delta}"}}"#
                ),
                vec![DownlinkEvent::Audio(vec![5, -5])],
            ),
            (
                r#"{"type":"response.audio_transcript.delta","response_id":"r1","delta":"你好"}"#
                    .into(),
                vec![DownlinkEvent::AssistantTextDelta("你好".into())],
            ),
            (
                r#"{"type":"response.audio_transcript.done","response_id":"r1","transcript":"你好呀"}"#
                    .into(),
                vec![DownlinkEvent::AssistantTextDone(Some("你好呀".into()))],
            ),
            (
                r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"i","content_index":0,"transcript":"查订单"}"#
                    .into(),
                vec![DownlinkEvent::UserTranscript("查订单".into())],
            ),
            (
                r#"{"type":"input_audio_buffer.speech_started","audio_start_ms":100,"item_id":"i"}"#
                    .into(),
                vec![DownlinkEvent::BargeIn],
            ),
            (
                r#"{"type":"input_audio_buffer.speech_stopped","audio_end_ms":900,"item_id":"i"}"#
                    .into(),
                vec![],
            ),
            (
                r#"{"type":"response.function_call_arguments.done","response_id":"r1","item_id":"i","output_index":0,"name":"get_weather","call_id":"call_1","arguments":"{\"city\":\"北京\"}"}"#
                    .into(),
                vec![DownlinkEvent::ToolCalls {
                    calls: vec![ToolCall {
                        call_id: "call_1".into(),
                        name: "get_weather".into(),
                        arguments: "{\"city\":\"北京\"}".into(),
                    }],
                    complete: false,
                }],
            ),
            (
                r#"{"type":"response.done","response":{"id":"r1","status":"completed","usage":{"total_tokens":10}}}"#
                    .into(),
                vec![DownlinkEvent::ResponseDone],
            ),
            (
                r#"{"event_id":"e9","type":"error","error":{"type":"invalid_request_error","code":"invalid_value","message":"bad","param":"session.modalities"}}"#
                    .into(),
                vec![DownlinkEvent::Error {
                    message: "invalid_value: bad".into(),
                    fatal: false,
                }],
            ),
            ("garbage".into(), vec![]),
        ];
        for (input, want) in cases {
            assert_eq!(p.decode_text(&input), want, "input: {input}");
        }
    }
}
