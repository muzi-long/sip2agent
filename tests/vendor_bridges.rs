//! Bridge-level tests against mock vendor servers speaking the documented
//! Qwen-Omni realtime and Doubao Seeduplex event shapes. No real keys are
//! used: these verify our encoding/decoding/sequencing, not the vendors.

mod common;

use base64::Engine as _;
use common::*;
use serde_json::{Value, json};
use sip2agent::config::Config;
use sip2agent::realtime::bridge::{BridgeEnd, BridgeResult, connect, run_bridge};
use sip2agent::realtime::protocol_for;
use sip2agent::rtp::MediaPort;
use sip2agent::webhook::ToolDispatcher;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

fn b64(pcm: &[i16]) -> String {
    base64::engine::general_purpose::STANDARD.encode(sip2agent::audio::pcm_to_bytes(pcm))
}

fn unb64(s: &str) -> Vec<i16> {
    sip2agent::audio::bytes_to_pcm(&base64::engine::general_purpose::STANDARD.decode(s).unwrap())
}

struct Harness {
    up_tx: mpsc::Sender<Vec<i16>>,
    down_rx: mpsc::Receiver<Vec<i16>>,
    cancel: CancellationToken,
    task: JoinHandle<BridgeResult>,
}

async fn start_bridge(config: &str, call_id: &str) -> Harness {
    sip2agent::install_crypto_provider();
    let cfg = Config::parse(config).unwrap();
    let agent = cfg.agents[0].clone();
    let protocol = protocol_for(&agent).unwrap();
    let ws = connect(protocol.as_ref(), Duration::from_secs(3))
        .await
        .unwrap();
    let (up_tx, up_rx) = mpsc::channel(50);
    let (down_tx, down_rx) = mpsc::channel(500);
    let cancel = CancellationToken::new();
    let tools = ToolDispatcher::new(
        agent.tool_webhook.clone(),
        call_id.into(),
        agent.name.clone(),
        Duration::from_secs(3),
    );
    let c = cancel.clone();
    let task = tokio::spawn(async move {
        run_bridge(
            protocol,
            ws,
            MediaPort {
                uplink: up_rx,
                downlink: down_tx,
            },
            tools,
            Duration::from_millis(agent.timeout_ms),
            c,
        )
        .await
    });
    Harness {
        up_tx,
        down_rx,
        cancel,
        task,
    }
}

/// Read downlink frames until one is non-silent (or timeout).
async fn wait_nonsilent(rx: &mut mpsc::Receiver<Vec<i16>>) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Ok(Some(f)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            assert_eq!(f.len(), 160);
            if !is_silent(&f) {
                return true;
            }
        }
    }
    false
}

/// After barge-in, at most a couple of in-flight frames may still be audible.
async fn assert_goes_silent(rx: &mut mpsc::Receiver<Vec<i16>>) {
    while rx.try_recv().is_ok() {}
    let mut loud_after_grace = 0;
    for i in 0..15 {
        let f = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if i >= 3 && !is_silent(&f) {
            loud_after_grace += 1;
        }
    }
    assert_eq!(loud_after_grace, 0, "playout must be cleared on barge-in");
}

#[tokio::test]
async fn qwen_omni_full_session() {
    let (ws_url, mut conns) = mock_ws().await;
    let mut http = mock_http(Arc::new(|hit: &HttpHit| {
        (
            200,
            json!({"order": hit.body["arguments"]["order_id"], "status": "shipped"}).to_string(),
        )
    }))
    .await;
    let config = format!(
        r#"
[[agent]]
name = "qwen"
match = "*"
provider = "qwen_omni"
url = "{ws_url}/api-ws/v1/realtime"
api_key = "sk-test"
instructions = "你是客服"
greeting = "您好，这里是测试客服"
tool_webhook = "{}/tool"
timeout_ms = 3000

[[agent.tools]]
name = "query_order"
description = "查询订单"
parameters = {{ type = "object", properties = {{ order_id = {{ type = "string" }} }}, required = ["order_id"] }}
"#,
        http.base
    );
    let (barge_tx, barge_rx) = oneshot::channel::<()>();
    let mock = tokio::spawn(async move {
        let mut c = conns.recv().await.unwrap();
        assert!(
            c.uri
                .ends_with("/api-ws/v1/realtime?model=qwen3.8-omni-flash-realtime"),
            "{}",
            c.uri
        );
        assert_eq!(c.header("authorization").as_deref(), Some("Bearer sk-test"));

        let upd = c.next_json().await;
        assert_eq!(upd["type"], "session.update");
        assert_eq!(
            upd["session"]["audio"]["input"]["format"]["sample_rate"],
            16000
        );
        assert_eq!(
            upd["session"]["tools"][0]["function"]["name"],
            "query_order"
        );
        c.send_json(json!({"type": "session.created", "session": {"id": "sess_1"}}))
            .await;
        // Nothing (no audio) may arrive before session.updated.
        tokio::time::sleep(Duration::from_millis(200)).await;
        c.send_json(json!({"type": "session.updated", "session": {"id": "sess_1"}}))
            .await;
        // Greeting comes first, before any audio.
        assert_eq!(c.next_json().await, json!({"type": "response.create"}));
        // Uplink audio: 20 ms at 16 kHz.
        let a = c.next_json().await;
        assert_eq!(a["type"], "input_audio_buffer.append");
        assert_eq!(unb64(a["audio"].as_str().unwrap()).len(), 320);

        // Greeting audio: 1 s of 24 kHz tone in 50 deltas.
        c.send_json(json!({"type": "response.created", "response": {"id": "r1"}}))
            .await;
        let t = tone(480, 24000.0, 300.0, 10000.0);
        for _ in 0..50 {
            c.send_json(json!({"type": "response.audio.delta", "response_id": "r1", "item_id": "i1", "output_index": 0, "content_index": 0, "delta": b64(&t)}))
                .await;
        }
        c.send_json(json!({"type": "response.audio_transcript.done", "response_id": "r1", "transcript": "您好，这里是测试客服"}))
            .await;
        c.send_json(
            json!({"type": "response.done", "response": {"id": "r1", "status": "completed"}}),
        )
        .await;

        // Caller asks; model calls a tool.
        c.send_json(json!({"type": "conversation.item.input_audio_transcription.completed", "item_id": "u1", "content_index": 0, "transcript": "查一下订单A1"}))
            .await;
        c.send_json(json!({"type": "response.created", "response": {"id": "r2"}}))
            .await;
        c.send_json(json!({"type": "response.function_call_arguments.done", "response_id": "r2", "item_id": "fc1", "output_index": 0, "name": "query_order", "call_id": "call_1", "arguments": "{\"order_id\":\"A1\"}"}))
            .await;
        c.send_json(
            json!({"type": "response.done", "response": {"id": "r2", "status": "completed"}}),
        )
        .await;
        let item = c.next_control().await;
        assert_eq!(item["type"], "conversation.item.create");
        assert_eq!(item["item"]["type"], "function_call_output");
        assert_eq!(item["item"]["call_id"], "call_1");
        let out: Value = serde_json::from_str(item["item"]["output"].as_str().unwrap()).unwrap();
        assert_eq!(out, json!({"order": "A1", "status": "shipped"}));
        assert_eq!(c.next_control().await, json!({"type": "response.create"}));

        // Long answer (3 s), then the caller barges in.
        c.send_json(json!({"type": "response.created", "response": {"id": "r3"}}))
            .await;
        for _ in 0..150 {
            c.send_json(
                json!({"type": "response.audio.delta", "response_id": "r3", "delta": b64(&t)}),
            )
            .await;
        }
        c.send_json(json!({"type": "response.audio_transcript.delta", "response_id": "r3", "delta": "订单A1已"}))
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        c.send_json(json!({"type": "input_audio_buffer.speech_started", "audio_start_ms": 5000, "item_id": "u2"}))
            .await;
        // Stale audio of the cancelled response must be dropped.
        for _ in 0..20 {
            c.send_json(
                json!({"type": "response.audio.delta", "response_id": "r3", "delta": b64(&t)}),
            )
            .await;
        }
        let _ = barge_tx.send(());
        assert_eq!(c.next_control().await, json!({"type": "response.cancel"}));
        c.send_json(
            json!({"type": "response.done", "response": {"id": "r3", "status": "cancelled"}}),
        )
        .await;
        // A benign error must not end the session.
        c.send_json(json!({"type": "error", "error": {"type": "invalid_request_error", "code": "x", "message": "no active response"}}))
            .await;
        // Wait for the client to close.
        use futures::StreamExt;
        while let Some(Ok(m)) = c.ws.next().await {
            if matches!(m, tokio_tungstenite::tungstenite::Message::Close(_)) {
                return true;
            }
        }
        false
    });

    let mut h = start_bridge(&config, "call-qwen-1").await;
    // Feed caller audio continuously.
    let up = h.up_tx.clone();
    let feeder = tokio::spawn(async move {
        let f = tone(160, 8000.0, 440.0, 8000.0);
        loop {
            if up.send(f.clone()).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    assert!(
        wait_nonsilent(&mut h.down_rx).await,
        "greeting audio must play"
    );
    barge_rx.await.unwrap();
    assert_goes_silent(&mut h.down_rx).await;

    // Tool webhook payload.
    let hit = tokio::time::timeout(Duration::from_secs(3), http.hits.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hit.path, "/tool");
    assert_eq!(
        hit.body,
        json!({"call_id": "call-qwen-1", "agent": "qwen", "tool": "query_order", "tool_call_id": "call_1", "arguments": {"order_id": "A1"}})
    );

    tokio::time::sleep(Duration::from_millis(200)).await;
    h.cancel.cancel();
    let res = h.task.await.unwrap();
    feeder.abort();
    assert_eq!(res.end, BridgeEnd::Cancelled);
    assert!(mock.await.unwrap(), "client must close the websocket");
    let roles: Vec<(&str, &str)> = res
        .transcripts
        .iter()
        .map(|t| (t.role.as_str(), t.text.as_str()))
        .collect();
    assert_eq!(
        roles,
        vec![
            ("assistant", "您好，这里是测试客服"),
            ("user", "查一下订单A1"),
            ("assistant", "订单A1已"),
        ]
    );
}

#[tokio::test]
async fn doubao_full_session() {
    let (ws_url, mut conns) = mock_ws().await;
    let mut http = mock_http(Arc::new(|hit: &HttpHit| {
        (
            200,
            format!("result-of-{}", hit.body["tool"].as_str().unwrap_or("")),
        )
    }))
    .await;
    let config = format!(
        r#"
[[agent]]
name = "doubao"
match = "*"
provider = "doubao"
url = "{ws_url}/api/v3/duplex/realtime/dialogue"
api_key = "ak-test"
voice = "zh_female_vv_jupiter_bigtts"
instructions = "你是客服"
greeting = "您好！"
tool_webhook = "{}/tool"
timeout_ms = 3000

[[agent.tools]]
name = "get_weather"
description = "查天气"
parameters = {{ type = "object", properties = {{ city = {{ type = "string" }} }} }}

[[agent.tools]]
name = "get_stock"
description = "查股价"
"#,
        http.base
    );
    let (barge_tx, barge_rx) = oneshot::channel::<()>();
    let mock = tokio::spawn(async move {
        let mut c = conns.recv().await.unwrap();
        assert_eq!(c.header("x-api-key").as_deref(), Some("ak-test"));
        let create = c.next_json().await;
        assert_eq!(create["type"], "session.create");
        assert_eq!(create["session"]["model"], "1.2.6.1");
        assert_eq!(
            create["session"]["audio"]["output"]["format"],
            json!({"type": "pcm_s16le", "rate": 24000})
        );
        assert_eq!(create["session"]["tools"][1]["name"], "get_stock");
        c.send_json(
            json!({"type": "session.created", "event_id": "e0", "session": {"id": "dlg_1"}}),
        )
        .await;
        assert_eq!(
            c.next_json().await,
            json!({"type": "speech_text_buffer.commit", "text": "您好！"})
        );

        // Real-time pacing with silence keepalive: ~50 packets/s, 640 bytes each.
        let t0 = Instant::now();
        let mut n = 0;
        while t0.elapsed() < Duration::from_millis(1000) {
            let a = c.next_json().await;
            if a["type"] == "input_audio_buffer.append" {
                assert_eq!(unb64(a["audio"].as_str().unwrap()).len() * 2, 640);
                n += 1;
            }
        }
        assert!((40..=60).contains(&n), "paced packets in 1 s: {n}");

        let t = tone(480, 24000.0, 300.0, 10000.0);
        c.send_json(json!({"type": "response.output_audio.started", "question_id": "q", "response_id": "", "tts_type": "chat_tts_text"}))
            .await;
        for _ in 0..50 {
            c.send_json(json!({"type": "response.output_audio.delta", "question_id": "q", "response_id": "r1", "delta": b64(&t)}))
                .await;
        }
        c.send_json(
            json!({"type": "response.output_text.delta", "response_id": "r1", "delta": "您"}),
        )
        .await;
        c.send_json(
            json!({"type": "response.output_text.delta", "response_id": "r1", "delta": "好！"}),
        )
        .await;
        c.send_json(json!({"type": "response.output_text.done", "response_id": "r1"}))
            .await;
        c.send_json(json!({"type": "conversation.item.input_audio_transcription.completed", "item_id": "u1", "content_index": 0, "transcript": "北京天气和招行股价"}))
            .await;
        c.send_json(json!({"type": "response.function_call_arguments.done", "event_id": "e9", "items": [
            {"id": "item_001", "type": "function_call", "call_id": "call_weather_001", "name": "get_weather", "arguments": "{\"city\":\"Beijing\"}"},
            {"id": "item_002", "type": "function_call", "call_id": "call_stock_002", "name": "get_stock", "arguments": "{\"stock_code\":\"600036\"}"}
        ]}))
        .await;
        let item = c.next_control().await;
        assert_eq!(
            item,
            json!({"type": "conversation.item.create", "items": [
                {"call_id": "call_weather_001", "role": "tool", "content": [{"type": "input_text", "text": "result-of-get_weather"}]},
                {"call_id": "call_stock_002", "role": "tool", "content": [{"type": "input_text", "text": "result-of-get_stock"}]}
            ]})
        );

        // Long reply then barge-in (ASR started).
        c.send_json(json!({"type": "response.output_audio.started", "question_id": "q2", "response_id": "", "tts_type": ""}))
            .await;
        for _ in 0..150 {
            c.send_json(json!({"type": "response.output_audio.delta", "response_id": "r2", "delta": b64(&t)}))
                .await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        c.send_json(json!({"type": "conversation.item.input_audio_transcription.started", "event_id": "e", "item_id": ""}))
            .await;
        let _ = barge_tx.send(());
        assert_eq!(c.next_control().await, json!({"type": "response.cancel"}));
        c.send_json(json!({"type": "response.canceled", "event_id": "e17"}))
            .await;

        // Graceful close handshake.
        assert_eq!(c.next_control().await, json!({"type": "session.close"}));
        c.send_json(json!({"type": "session.closed"})).await;
        use futures::StreamExt;
        while let Some(Ok(m)) = c.ws.next().await {
            if matches!(m, tokio_tungstenite::tungstenite::Message::Close(_)) {
                return true;
            }
        }
        false
    });

    let mut h = start_bridge(&config, "call-db-1").await;
    // No caller audio at all: the bridge must still stream silence.
    assert!(
        wait_nonsilent(&mut h.down_rx).await,
        "reply audio must play"
    );
    barge_rx.await.unwrap();
    assert_goes_silent(&mut h.down_rx).await;

    let mut tools = Vec::new();
    for _ in 0..2 {
        let hit = tokio::time::timeout(Duration::from_secs(3), http.hits.recv())
            .await
            .unwrap()
            .unwrap();
        tools.push(hit.body["tool"].as_str().unwrap().to_string());
        assert_eq!(hit.body["call_id"], "call-db-1");
        assert_eq!(hit.body["agent"], "doubao");
    }
    tools.sort();
    assert_eq!(tools, vec!["get_stock", "get_weather"]);

    h.cancel.cancel();
    let t = Instant::now();
    let res = h.task.await.unwrap();
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "closed ack should end the wait early"
    );
    assert_eq!(res.end, BridgeEnd::Cancelled);
    assert!(mock.await.unwrap());
    let roles: Vec<(&str, &str)> = res
        .transcripts
        .iter()
        .map(|t| (t.role.as_str(), t.text.as_str()))
        .collect();
    assert_eq!(
        roles,
        vec![("assistant", "您好！"), ("user", "北京天气和招行股价")]
    );
    drop(h.up_tx);
}

#[tokio::test]
async fn doubao_fatal_error_and_ready_timeout() {
    // Fatal vendor error ends the bridge.
    let (ws_url, mut conns) = mock_ws().await;
    let cfg = format!(
        "[[agent]]\nname='d'\nmatch='*'\nprovider='doubao'\nurl='{ws_url}/x'\napi_key='k'\nvoice='v'\ntimeout_ms=3000\n"
    );
    let mock = tokio::spawn(async move {
        let mut c = conns.recv().await.unwrap();
        let _ = c.next_json().await;
        c.send_json(json!({"type": "session.created", "session": {"id": "d"}}))
            .await;
        c.send_json(json!({"type": "error", "error": {"code": "45000003", "message": "Abnormal silence audio"}}))
            .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    });
    let h = start_bridge(&cfg, "c1").await;
    let res = tokio::time::timeout(Duration::from_secs(3), h.task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(res.end, BridgeEnd::VendorError(ref m) if m.contains("45000003")));
    mock.await.unwrap();

    // Session never created → ready timeout (no audio sent meanwhile).
    let (ws_url, mut conns) = mock_ws().await;
    let cfg = format!(
        "[[agent]]\nname='d'\nmatch='*'\nprovider='doubao'\nurl='{ws_url}/x'\napi_key='k'\nvoice='v'\ntimeout_ms=400\n"
    );
    let mock = tokio::spawn(async move {
        let mut c = conns.recv().await.unwrap();
        let first = c.next_json().await;
        assert_eq!(first["type"], "session.create");
        // Next thing must be our close, not audio.
        let next = c.next_json().await;
        assert_eq!(next["type"], "session.close");
    });
    let h = start_bridge(&cfg, "c2").await;
    let res = tokio::time::timeout(Duration::from_secs(5), h.task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res.end, BridgeEnd::ReadyTimeout);
    mock.await.unwrap();
}

#[tokio::test]
async fn tool_dispatcher_errors_are_reported_to_model() {
    sip2agent::install_crypto_provider();
    let call = sip2agent::realtime::ToolCall {
        call_id: "c".into(),
        name: "t".into(),
        arguments: "not-json".into(),
    };
    // No webhook configured.
    let d = ToolDispatcher::new(None, "x".into(), "a".into(), Duration::from_secs(1));
    let out = d.execute(&call).await;
    assert_eq!(out.call_id, "c");
    assert!(out.output.contains("no tool_webhook"));
    // Non-2xx.
    let mut http = mock_http(Arc::new(|_| (500, "boom".into()))).await;
    let d = ToolDispatcher::new(
        Some(format!("{}/t", http.base)),
        "x".into(),
        "a".into(),
        Duration::from_secs(2),
    );
    let out = d.execute(&call).await;
    assert!(out.output.contains("HTTP 500"), "{}", out.output);
    let hit = http.hits.recv().await.unwrap();
    // Non-JSON arguments are passed through as a string.
    assert_eq!(hit.body["arguments"], "not-json");
    // Unreachable.
    let d = ToolDispatcher::new(
        Some("http://127.0.0.1:1/t".into()),
        "x".into(),
        "a".into(),
        Duration::from_secs(1),
    );
    assert!(d.execute(&call).await.output.contains("failed"));
}
