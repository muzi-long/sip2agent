//! Live calls against the real Qwen-Omni realtime endpoint. Skipped unless
//! `SIP2AGENT_QWEN_LIVE=1`:
//!
//! ```sh
//! say -v Tingting -o $DIR/ask.wav --data-format=LEI16@8000 "你好，我想查一下订单，订单号是一二三四五。"
//! say -v Tingting -o $DIR/q.wav   --data-format=LEI16@8000 "请问你们周末营业吗？"
//! SIP2AGENT_QWEN_LIVE=1 SIP2AGENT_LIVE_DIR=$DIR QWEN_URL=wss://{WorkspaceId}.cn-beijing.maas.aliyuncs.com/api-ws/v1/realtime \
//!   DASHSCOPE_API_KEY=sk-... cargo test --test qwen_live -- --nocapture --test-threads=1
//! ```
//!
//! Full SIP path: test UAC → sip2agent → Qwen. The downlink is recorded to
//! `$DIR/live_*.wav` for listening.

mod common;

use common::*;
use serde_json::{Value, json};
use sip2agent::uac::{UacOptions, read_wav, run, write_wav};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const GREETING: &str =
    "您好，这里是星光商城客服中心，我是智能客服小星，很高兴为您服务，请问今天有什么可以帮您的吗？";

fn live_dir() -> Option<PathBuf> {
    if std::env::var("SIP2AGENT_QWEN_LIVE").as_deref() != Ok("1") {
        eprintln!("skip: SIP2AGENT_QWEN_LIVE != 1");
        return None;
    }
    Some(PathBuf::from(
        std::env::var("SIP2AGENT_LIVE_DIR").expect("SIP2AGENT_LIVE_DIR"),
    ))
}

fn config(port_min: u16, webhook: &str) -> String {
    let url = std::env::var("QWEN_URL").expect("QWEN_URL");
    format!(
        r#"
[sip]
listen = "127.0.0.1:0"
[rtp]
bind_ip = "127.0.0.1"
port_min = {port_min}
port_max = {}
timeout_secs = 30

[[agent]]
name = "qwen-live"
match = "*"
provider = "qwen_omni"
url = "{url}"
api_key_env = "DASHSCOPE_API_KEY"
instructions = "你是星光商城的电话客服，每次回复不超过两句话。客户问订单时调用 query_order 查询，按查询结果回答，不要编造。"
greeting = "{GREETING}"
tool_webhook = "{webhook}/tool"
call_end_webhook = "{webhook}/end"
timeout_ms = 10000

[[agent.tools]]
name = "query_order"
description = "按订单号查询订单状态"
parameters = {{ type = "object", properties = {{ order_id = {{ type = "string", description = "订单号" }} }}, required = ["order_id"] }}
"#,
        port_min + 20
    )
}

fn clip(dir: &std::path::Path, name: &str) -> Vec<i16> {
    let (pcm, rate) = read_wav(&dir.join(name)).unwrap();
    assert_eq!(rate, 8000, "{name} must be 8 kHz");
    pcm
}

/// Audible 20 ms frames as (start_sec, end_sec) segments, merging gaps < 400 ms.
fn segments(pcm: &[i16]) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::new();
    for (i, f) in pcm.chunks(160).enumerate() {
        let rms = (f.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / f.len() as f64).sqrt();
        if rms < 300.0 {
            continue;
        }
        let (s, e) = (i as f64 * 0.02, (i + 1) as f64 * 0.02);
        match out.last_mut() {
            Some(last) if s - last.1 < 0.4 => last.1 = e,
            _ => out.push((s, e)),
        }
    }
    out
}

fn rms(pcm: &[i16]) -> f64 {
    (pcm.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / pcm.len().max(1) as f64).sqrt()
}

async fn call_end(http: &mut MockHttp) -> Value {
    loop {
        let hit = tokio::time::timeout(Duration::from_secs(10), http.hits.recv())
            .await
            .expect("call-end webhook")
            .unwrap();
        if hit.path == "/end" {
            return hit.body;
        }
    }
}

fn print_transcripts(end: &Value) {
    for t in end["transcripts"].as_array().unwrap() {
        println!(
            "    {:9} {}",
            t["role"].as_str().unwrap(),
            t["text"].as_str().unwrap()
        );
    }
}

/// Greeting verbatim → caller asks → tool round trip → answer → caller BYE.
#[tokio::test]
async fn live_greeting_tool_answer() {
    let Some(dir) = live_dir() else { return };
    let tool_hits = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let th = tool_hits.clone();
    let mut http = mock_http(Arc::new(move |hit: &HttpHit| {
        if hit.path == "/tool" {
            th.lock().unwrap().push(hit.body.clone());
            (
                200,
                json!({"status": "已发货", "eta": "明天下午送达"}).to_string(),
            )
        } else {
            (200, "{}".into())
        }
    }))
    .await;
    let (addr, shutdown) = start_server(&config(33000, &http.base)).await;
    let r = run(UacOptions {
        server: addr,
        to: "1".into(),
        duration: Duration::from_secs(26),
        script: vec![(Duration::from_secs(10), clip(&dir, "ask.wav"))],
        record: true,
        ..Default::default()
    })
    .await
    .unwrap();
    write_wav(&dir.join("live_greeting_tool.wav"), &r.received, 8000).unwrap();
    let end = call_end(&mut http).await;
    shutdown.cancel();

    let segs = segments(&r.received);
    println!(
        "  status={} codec={:?} rtp sent/recv={}/{} rms={:.0}",
        r.status,
        r.codec,
        r.rtp_sent,
        r.rtp_received,
        rms(&r.received)
    );
    println!("  audible segments (s): {segs:?}");
    println!("  tool calls: {:?}", tool_hits.lock().unwrap());
    println!(
        "  hangup_reason={} duration={}",
        end["hangup_reason"], end["duration_secs"]
    );
    print_transcripts(&end);

    assert_eq!(r.status, 200);
    assert!(r.local_hangup && !r.remote_hangup);
    assert_eq!(end["hangup_reason"], "caller_bye");
    let greeting_secs: f64 = segs.iter().filter(|s| s.0 < 10.0).map(|s| s.1 - s.0).sum();
    assert!(greeting_secs > 3.0, "greeting audible: {greeting_secs}s");
    let answer_secs: f64 = segs.iter().filter(|s| s.0 > 14.0).map(|s| s.1 - s.0).sum();
    assert!(answer_secs > 1.0, "answer audible: {answer_secs}s");

    let ts = end["transcripts"].as_array().unwrap();
    let first = &ts[0];
    assert_eq!(first["role"], "assistant");
    assert!(
        first["text"].as_str().unwrap().contains("星光商城"),
        "greeting transcript: {first}"
    );
    assert!(
        ts.iter()
            .any(|t| t["role"] == "user" && t["text"].as_str().unwrap().contains("12345"))
    );
    let calls = tool_hits.lock().unwrap().clone();
    assert!(!calls.is_empty(), "query_order must be called");
    assert_eq!(calls[0]["tool"], "query_order");
    assert!(
        calls[0]["arguments"]["order_id"]
            .as_str()
            .unwrap()
            .contains("12345")
    );
    let last = ts.iter().rev().find(|t| t["role"] == "assistant").unwrap();
    assert!(
        last["text"].as_str().unwrap().contains("发货")
            || last["text"].as_str().unwrap().contains("送达"),
        "answer uses tool result: {last}"
    );
}

/// Caller talks over the greeting: the greeting is cut and the question answered.
#[tokio::test]
async fn live_barge_in_during_greeting() {
    let Some(dir) = live_dir() else { return };
    let mut http = mock_http(Arc::new(|_: &HttpHit| (200, "{}".into()))).await;
    let (addr, shutdown) = start_server(&config(33100, &http.base)).await;
    let r = run(UacOptions {
        server: addr,
        to: "1".into(),
        duration: Duration::from_secs(16),
        script: vec![(Duration::from_millis(2500), clip(&dir, "q.wav"))],
        record: true,
        ..Default::default()
    })
    .await
    .unwrap();
    write_wav(&dir.join("live_barge_in.wav"), &r.received, 8000).unwrap();
    let end = call_end(&mut http).await;
    shutdown.cancel();

    let segs = segments(&r.received);
    println!(
        "  status={} rtp sent/recv={}/{} rms={:.0}",
        r.status,
        r.rtp_sent,
        r.rtp_received,
        rms(&r.received)
    );
    println!("  audible segments (s): {segs:?}");
    print_transcripts(&end);

    assert_eq!(r.status, 200);
    // Full greeting is ~8 s of speech; it must stop soon after the caller
    // starts at 2.5 s (held audio is replayed once the greeting response is
    // generated, then server VAD fires).
    let first = segs.first().expect("greeting audible");
    assert!(first.0 < 2.5, "greeting starts before the caller: {segs:?}");
    assert!(first.1 < 6.0, "greeting cut by barge-in: {segs:?}");
    let ts = end["transcripts"].as_array().unwrap();
    assert!(
        ts.iter()
            .any(|t| t["role"] == "user" && t["text"].as_str().unwrap().contains("营业")),
        "caller question transcribed"
    );
    let answered = segs.iter().any(|s| s.0 > first.1 + 0.3);
    assert!(answered, "question answered after the cut: {segs:?}");
}
