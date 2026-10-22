//! Doubao realtime 3.0 (Seeduplex) full-duplex protocol.
//!
//! Verified against Volcengine docs 6561/2549778 + 6561/2549732 and the
//! official python3.7 duplex demo (2026-10):
//! - URL `wss://openspeech.bytedance.com/api/v3/duplex/realtime/dialogue`,
//!   header `X-Api-Key`. Pure JSON text frames routed by `type`.
//! - `session.create` with `session.model = "1.2.6.1"`; `extension` sits at
//!   the top level of the event (sibling of `session`), as in the demo.
//! - Input `pcm` 16 kHz int16; output `pcm_s16le` 24 kHz (`pcm` would be float32).
//! - Audio must be sent at real-time pace in 20 ms packets (640 bytes), also
//!   during silence (keepalive) — the bridge loop guarantees that.
//! - Tools are flat `{type:"function",name,description,parameters}`.
//! - All calls of one `response.function_call_arguments.done` are answered in
//!   a single `conversation.item.create` (role `tool`).
//! - Close: `session.close`, wait for `session.closed`, then close the socket.

use super::{
    DownlinkEvent, RealtimeProtocol, ToolCall, ToolOutput, UplinkMessage, b64_pcm, pcm_b64,
    str_field,
};
use crate::config::AgentConfig;
use serde_json::{Value, json};

pub const DEFAULT_URL: &str = "wss://openspeech.bytedance.com/api/v3/duplex/realtime/dialogue";
pub const DEFAULT_MODEL: &str = "1.2.6.1";
pub const INPUT_RATE: u32 = 16000;
pub const OUTPUT_RATE: u32 = 24000;

pub struct Doubao {
    url: String,
    api_key: String,
    extra_headers: Vec<(String, String)>,
    model: String,
    voice: String,
    instructions: Option<String>,
    greeting: Option<String>,
    tools: Vec<crate::config::ToolDef>,
    extension: Option<Value>,
}

impl Doubao {
    pub fn new(agent: &AgentConfig, api_key: String) -> Self {
        Self {
            url: agent.url.clone().unwrap_or_else(|| DEFAULT_URL.to_string()),
            api_key,
            extra_headers: agent
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            model: agent
                .model
                .clone()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            voice: agent.voice.clone().unwrap_or_default(),
            instructions: agent.instructions.clone().filter(|s| !s.is_empty()),
            greeting: agent.greeting.clone().filter(|s| !s.is_empty()),
            tools: agent.tools.clone(),
            extension: agent.extension.clone(),
        }
    }
}

impl RealtimeProtocol for Doubao {
    fn name(&self) -> &'static str {
        "doubao"
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
        let mut h = vec![("X-Api-Key".to_string(), self.api_key.clone())];
        h.extend(self.extra_headers.iter().cloned());
        h
    }
    fn ready_on_open(&self) -> bool {
        false
    }

    fn session_open(&self) -> Vec<UplinkMessage> {
        let mut session = json!({
            "model": self.model,
            "audio": {
                "input": {"format": {"type": "pcm", "rate": INPUT_RATE}},
                "output": {
                    "format": {"type": "pcm_s16le", "rate": OUTPUT_RATE},
                    "voice": self.voice,
                },
            },
        });
        if let Some(i) = &self.instructions {
            session["instructions"] = json!(i);
        }
        if !self.tools.is_empty() {
            session["tools"] = Value::Array(
                self.tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        })
                    })
                    .collect(),
            );
        }
        let mut ev = json!({"type": "session.create", "session": session});
        if let Some(ext) = &self.extension {
            ev["extension"] = ext.clone();
        }
        vec![UplinkMessage::json(ev)]
    }

    fn greeting(&self) -> Vec<UplinkMessage> {
        match &self.greeting {
            Some(text) => vec![UplinkMessage::json(json!({
                "type": "speech_text_buffer.commit",
                "text": text,
            }))],
            None => Vec::new(),
        }
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
        let items: Vec<Value> = outputs
            .iter()
            .map(|o| {
                json!({
                    "call_id": o.call_id,
                    "role": "tool",
                    "content": [{"type": "input_text", "text": o.output}],
                })
            })
            .collect();
        vec![UplinkMessage::json(json!({
            "type": "conversation.item.create",
            "items": items,
        }))]
    }

    fn close(&self) -> Vec<UplinkMessage> {
        vec![UplinkMessage::json(json!({"type": "session.close"}))]
    }

    fn decode_text(&self, text: &str) -> Vec<DownlinkEvent> {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return Vec::new();
        };
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "session.created" => vec![DownlinkEvent::SessionReady],
            "session.closed" => vec![DownlinkEvent::SessionClosed],
            "response.output_audio.started" => vec![DownlinkEvent::ResponseStarted],
            "response.output_audio.delta" => str_field(&v, "delta")
                .and_then(|d| b64_pcm(&d))
                .filter(|p| !p.is_empty())
                .map(DownlinkEvent::Audio)
                .into_iter()
                .collect(),
            "response.output_text.delta" => str_field(&v, "delta")
                .map(DownlinkEvent::AssistantTextDelta)
                .into_iter()
                .collect(),
            "response.output_text.done" => {
                vec![DownlinkEvent::AssistantTextDone(str_field(&v, "text"))]
            }
            // First recognized word of the caller: barge-in.
            "conversation.item.input_audio_transcription.started" => vec![DownlinkEvent::BargeIn],
            "conversation.item.input_audio_transcription.completed" => str_field(&v, "transcript")
                .or_else(|| str_field(&v, "text"))
                .filter(|t| !t.is_empty())
                .map(DownlinkEvent::UserTranscript)
                .into_iter()
                .collect(),
            "response.function_call_arguments.done" => {
                let calls: Vec<ToolCall> = v
                    .get("items")
                    .and_then(|i| i.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .map(|it| ToolCall {
                                call_id: str_field(it, "call_id").unwrap_or_default(),
                                name: str_field(it, "name").unwrap_or_default(),
                                arguments: str_field(it, "arguments").unwrap_or_default(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if calls.is_empty() {
                    Vec::new()
                } else {
                    vec![DownlinkEvent::ToolCalls {
                        calls,
                        complete: true,
                    }]
                }
            }
            "response.done" | "response.canceled" => vec![DownlinkEvent::ResponseDone],
            "error" => {
                let message = v
                    .get("error")
                    .map(|e| match e {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_else(|| v.to_string());
                // The official demo terminates on any `error` event.
                vec![DownlinkEvent::Error {
                    message,
                    fatal: true,
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

    fn proto(extra: &str) -> Doubao {
        let cfg = Config::parse(&format!(
            "[[agent]]\nname='d'\nmatch='*'\nprovider='doubao'\nvoice='zh_female_vv_jupiter_bigtts'\napi_key='ak'\n{extra}"
        ))
        .unwrap();
        let a = &cfg.agents[0];
        Doubao::new(a, a.resolve_api_key().unwrap())
    }

    fn json_of(m: &UplinkMessage) -> Value {
        match m {
            UplinkMessage::Text(t) => serde_json::from_str(t).unwrap(),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn url_headers_rates() {
        let p = proto("");
        assert_eq!(p.connect_url(), DEFAULT_URL);
        assert_eq!(p.headers()[0], ("X-Api-Key".into(), "ak".into()));
        assert_eq!(p.input_rate(), 16000);
        assert_eq!(p.output_rate(), 24000);
        assert!(!p.ready_on_open());
    }

    #[test]
    fn session_create_shape() {
        let p = proto(
            "instructions='你是客服'\nextension={dialog={extra={enable_music=false}}}\n[[agent.tools]]\nname='get_weather'\ndescription='查天气'\nparameters={type='object',properties={city={type='string'}},required=['city']}\n",
        );
        let v = json_of(&p.session_open()[0]);
        assert_eq!(v["type"], "session.create");
        let s = &v["session"];
        assert_eq!(s["model"], "1.2.6.1");
        assert_eq!(s["instructions"], "你是客服");
        assert_eq!(
            s["audio"]["input"]["format"],
            json!({"type": "pcm", "rate": 16000})
        );
        assert_eq!(
            s["audio"]["output"]["format"],
            json!({"type": "pcm_s16le", "rate": 24000})
        );
        assert_eq!(s["audio"]["output"]["voice"], "zh_female_vv_jupiter_bigtts");
        assert_eq!(
            s["tools"][0],
            json!({
                "type": "function",
                "name": "get_weather",
                "description": "查天气",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
            })
        );
        assert_eq!(v["extension"]["dialog"]["extra"]["enable_music"], false);
        assert!(s.get("id").is_none());
    }

    #[test]
    fn greeting_audio_cancel_close() {
        let p = proto("greeting='您好，请问有什么可以帮您？'");
        assert_eq!(
            json_of(&p.greeting()[0]),
            json!({"type": "speech_text_buffer.commit", "text": "您好，请问有什么可以帮您？"})
        );
        let pcm = vec![0i16; 320];
        let v = json_of(&p.encode_audio(&pcm)[0]);
        assert_eq!(v["type"], "input_audio_buffer.append");
        // 20 ms @ 16 kHz int16 = 640 bytes.
        assert_eq!(
            b64_pcm(v["audio"].as_str().unwrap()).unwrap().len() * 2,
            640
        );
        assert_eq!(
            json_of(&p.cancel_response()[0]),
            json!({"type": "response.cancel"})
        );
        assert_eq!(json_of(&p.close()[0]), json!({"type": "session.close"}));
        assert!(proto("").greeting().is_empty());
    }

    #[test]
    fn tool_outputs_batched_in_one_event() {
        let p = proto("");
        let out = p.encode_tool_outputs(&[
            ToolOutput {
                call_id: "call_weather_001".into(),
                output: "今天北京天气晴".into(),
            },
            ToolOutput {
                call_id: "call_stock_002".into(),
                output: "33.25".into(),
            },
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(
            json_of(&out[0]),
            json!({
                "type": "conversation.item.create",
                "items": [
                    {"call_id": "call_weather_001", "role": "tool", "content": [{"type": "input_text", "text": "今天北京天气晴"}]},
                    {"call_id": "call_stock_002", "role": "tool", "content": [{"type": "input_text", "text": "33.25"}]},
                ]
            })
        );
    }

    #[test]
    fn decodes_doc_examples() {
        let p = proto("");
        let delta = pcm_b64(&[100, -100, 7]);
        let cases: Vec<(String, Vec<DownlinkEvent>)> = vec![
            (
                r#"{"type":"session.created","event_id":"event_C9G5RJeJ2gF77mV7f2B1j","session":{"id":"d1"}}"#.into(),
                vec![DownlinkEvent::SessionReady],
            ),
            (
                r#"{"type":"conversation.item.input_audio_transcription.started","event_id":"event_0","item_id":""}"#.into(),
                vec![DownlinkEvent::BargeIn],
            ),
            (
                r#"{"type":"conversation.item.input_audio_transcription.delta","event_id":"e","item_id":"i","content_index":0,"delta":"Hey"}"#.into(),
                vec![],
            ),
            (
                r#"{"type":"conversation.item.input_audio_transcription.completed","event_id":"e","item_id":"i","content_index":0,"transcript":"Hey, can you hear me?"}"#.into(),
                vec![DownlinkEvent::UserTranscript("Hey, can you hear me?".into())],
            ),
            (
                r#"{"event_id":"event_4142","type":"response.output_text.delta","question_id":"q","response_id":"r","delta":"Sure, I can h"}"#.into(),
                vec![DownlinkEvent::AssistantTextDelta("Sure, I can h".into())],
            ),
            (
                r#"{"event_id":"event_4344","type":"response.output_text.done","question_id":"q","response_id":"r","text":"Sure, I can help with that."}"#.into(),
                vec![DownlinkEvent::AssistantTextDone(Some("Sure, I can help with that.".into()))],
            ),
            (
                r#"{"event_id":"event_4","type":"response.output_audio.started","question_id":"q","response_id":"","tts_type":""}"#.into(),
                vec![DownlinkEvent::ResponseStarted],
            ),
            (
                format!(r#"{{"event_id":"event_4950","type":"response.output_audio.delta","question_id":"q","response_id":"r","delta":"{delta}"}}"#),
                vec![DownlinkEvent::Audio(vec![100, -100, 7])],
            ),
            (
                r#"{"event_id":"event_5152","type":"response.output_audio.done","question_id":"q","response_id":"r","status_code":"20000002"}"#.into(),
                vec![],
            ),
            (
                r#"{"event_id":"event_8899","type":"response.function_call_arguments.done","items":[{"id":"item_001","type":"function_call","call_id":"call_weather_001","name":"get_weather","arguments":"{\"city\":\"Beijing\"}"},{"id":"item_002","type":"function_call","call_id":"call_stock_002","name":"get_stock","arguments":"{\"stock_code\":\"600036\",\"market\":\"sh\"}"}]}"#.into(),
                vec![DownlinkEvent::ToolCalls {
                    calls: vec![
                        ToolCall { call_id: "call_weather_001".into(), name: "get_weather".into(), arguments: "{\"city\":\"Beijing\"}".into() },
                        ToolCall { call_id: "call_stock_002".into(), name: "get_stock".into(), arguments: "{\"stock_code\":\"600036\",\"market\":\"sh\"}".into() },
                    ],
                    complete: true,
                }],
            ),
            (r#"{"type":"response.canceled","event_id":"event_17"}"#.into(), vec![DownlinkEvent::ResponseDone]),
            (r#"{"type":"response.done","usage":{}}"#.into(), vec![DownlinkEvent::ResponseDone]),
            (r#"{"type":"session.closed"}"#.into(), vec![DownlinkEvent::SessionClosed]),
            (
                r#"{"type":"error","error":{"code":"55000001","message":"ContextCanceled"}}"#.into(),
                vec![DownlinkEvent::Error {
                    message: r#"{"code":"55000001","message":"ContextCanceled"}"#.into(),
                    fatal: true,
                }],
            ),
        ];
        for (input, want) in cases {
            assert_eq!(p.decode_text(&input), want, "input: {input}");
        }
    }
}
