//! End-to-end SIP tests: in-process sip2agent + rsipstack test UAC over
//! loopback UDP, with mock WebSocket / HTTP servers.

mod common;

use common::*;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use sip2agent::audio::g711::G711;
use sip2agent::uac::{UacOptions, run};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

fn base_config(port_min: u16, timeout_secs: u64, agents: &str) -> String {
    format!(
        r#"
[sip]
listen = "127.0.0.1:0"
[rtp]
bind_ip = "127.0.0.1"
port_min = {port_min}
port_max = {}
timeout_secs = {timeout_secs}
{agents}
"#,
        port_min + 40
    )
}

const ECHO: &str = r#"
[[agent]]
name = "echo"
match = "9000"
provider = "echo"
"#;

#[tokio::test]
async fn echo_call_roundtrip_both_codecs() {
    let (addr, shutdown) = start_server(&base_config(32000, 10, ECHO)).await;
    for codecs in [vec![G711::Alaw, G711::Ulaw], vec![G711::Ulaw]] {
        let want = codecs[0];
        let r = run(UacOptions {
            server: addr,
            to: "9000".into(),
            codecs,
            duration: Duration::from_millis(1500),
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.codec, Some(want));
        assert!(r.rtp_sent >= 60, "{r:?}");
        assert!(r.rtp_received >= r.rtp_sent - 5, "{r:?}");
        assert!(r.rtp_received_nonsilent >= r.rtp_sent - 5, "{r:?}");
        assert!(r.local_hangup);
    }
    shutdown.cancel();
}

#[tokio::test]
async fn unknown_number_404_and_bad_codec_488() {
    let (addr, shutdown) = start_server(&base_config(32100, 10, ECHO)).await;
    let r = run(UacOptions {
        server: addr,
        to: "1234".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(r.status, 404);

    let offer = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
m=audio 40000 RTP/AVP 9 101\r\na=rtpmap:9 G722/8000\r\na=rtpmap:101 telephone-event/8000\r\n";
    let r = run(UacOptions {
        server: addr,
        to: "9000".into(),
        raw_offer: Some(offer.into()),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(r.status, 488);
    shutdown.cancel();
}

#[tokio::test]
async fn rtp_timeout_hangs_up_with_bye() {
    let (addr, shutdown) = start_server(&base_config(32200, 1, ECHO)).await;
    let t = Instant::now();
    let r = run(UacOptions {
        server: addr,
        to: "9000".into(),
        send_rtp: false,
        duration: Duration::from_secs(6),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(r.status, 200);
    assert!(r.remote_hangup, "server should BYE on RTP timeout: {r:?}");
    assert!(t.elapsed() < Duration::from_secs(4));
    shutdown.cancel();
}

/// pcm_ws: mock echoes caller audio, then sends `clear` and `hangup`.
/// Verifies headers, 20 ms framing (320 bytes @ 8 kHz), RTP both ways,
/// server-initiated BYE and the call-end webhook.
#[tokio::test]
async fn pcm_ws_echo_then_agent_hangup_with_call_end_webhook() {
    let (ws_url, mut conns) = mock_ws().await;
    let mut http = mock_http(Arc::new(|_| (200, "{}".into()))).await;
    let agents = format!(
        r#"
[[agent]]
name = "bot"
match = "*"
provider = "pcm_ws"
url = "{ws_url}/pcm?samplerate=8000"
headers = {{ X-Test = "yes" }}
call_end_webhook = "{}/call-end"
"#,
        http.base
    );
    let (addr, shutdown) = start_server(&base_config(32300, 10, &agents)).await;

    let mock = tokio::spawn(async move {
        let mut c = conns.recv().await.unwrap();
        assert_eq!(c.header("x-test").as_deref(), Some("yes"));
        assert!(c.uri.contains("/pcm"));
        let mut frames = 0usize;
        let mut first: Option<Instant> = None;
        let mut sizes_ok = true;
        while let Some(Ok(msg)) = c.ws.next().await {
            if let Message::Binary(b) = msg {
                sizes_ok &= b.len() == 320;
                first.get_or_insert_with(Instant::now);
                frames += 1;
                c.ws.send(Message::Binary(b)).await.unwrap();
                if frames == 25 {
                    c.send_json(json!({"type": "clear"})).await;
                }
                if frames == 100 {
                    c.send_json(json!({"type": "hangup"})).await;
                    break;
                }
            }
        }
        let paced = first.unwrap().elapsed();
        // Drain until the bridge closes the socket.
        while let Some(Ok(m)) = c.ws.next().await {
            if matches!(m, Message::Close(_)) {
                break;
            }
        }
        (frames, sizes_ok, paced)
    });

    let r = run(UacOptions {
        server: addr,
        to: "5555".into(),
        from: "13800000000".into(),
        duration: Duration::from_secs(8),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(r.status, 200);
    assert!(r.remote_hangup, "agent hangup must BYE the caller: {r:?}");
    assert!(r.rtp_received_nonsilent > 50, "{r:?}");

    let (frames, sizes_ok, paced) = mock.await.unwrap();
    assert_eq!(frames, 100);
    assert!(sizes_ok, "every uplink frame must be 20 ms of 8 kHz PCM16");
    // 99 intervals of 20 ms ≈ 1.98 s: real-time pacing, not a burst.
    assert!(
        paced >= Duration::from_millis(1700) && paced <= Duration::from_millis(2600),
        "{paced:?}"
    );

    let hit = tokio::time::timeout(Duration::from_secs(5), http.hits.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hit.path, "/call-end");
    let b = hit.body;
    assert_eq!(b["agent"], "bot");
    assert_eq!(b["from"], "13800000000");
    assert_eq!(b["to"], "5555");
    assert_eq!(b["hangup_reason"], "agent_hangup");
    assert!(b["duration_secs"].as_f64().unwrap() > 1.5);
    assert!(
        chrono::DateTime::parse_from_rfc3339(b["start_time"].as_str().unwrap()).is_ok()
            && chrono::DateTime::parse_from_rfc3339(b["end_time"].as_str().unwrap()).is_ok()
    );
    assert!(b["call_id"].as_str().unwrap().len() > 5);
    assert!(b["transcripts"].is_array());
    shutdown.cancel();
}

/// Vendor socket closing must hang up the call; silence is still streamed
/// at real-time pace when the caller sends no RTP.
#[tokio::test]
async fn ws_close_hangs_up_and_silence_keepalive() {
    let (ws_url, mut conns) = mock_ws().await;
    let mut http = mock_http(Arc::new(|_| (204, String::new()))).await;
    let agents = format!(
        r#"
[[agent]]
name = "bot"
match = "*"
provider = "pcm_ws"
url = "{ws_url}/x"
call_end_webhook = "{}/end"
"#,
        http.base
    );
    let (addr, shutdown) = start_server(&base_config(32400, 30, &agents)).await;
    let mock = tokio::spawn(async move {
        let mut c = conns.recv().await.unwrap();
        let mut zeros = 0;
        let t = Instant::now();
        while let Some(Ok(msg)) = c.ws.next().await {
            if let Message::Binary(b) = msg
                && b.len() == 320
                && b.iter().all(|&x| x == 0)
            {
                zeros += 1;
                if zeros == 25 {
                    break;
                }
            }
        }
        let elapsed = t.elapsed();
        c.ws.close(None).await.ok();
        (zeros, elapsed)
    });
    let r = run(UacOptions {
        server: addr,
        to: "1".into(),
        send_rtp: false,
        duration: Duration::from_secs(5),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(r.status, 200);
    assert!(r.remote_hangup, "{r:?}");
    let (zeros, elapsed) = mock.await.unwrap();
    assert_eq!(zeros, 25);
    assert!(elapsed >= Duration::from_millis(400), "{elapsed:?}");
    let hit = tokio::time::timeout(Duration::from_secs(5), http.hits.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hit.body["hangup_reason"], "ws_closed");
    shutdown.cancel();
}

/// Vendor unreachable → 503 before answering.
#[tokio::test]
async fn vendor_unreachable_is_503() {
    let agents = r#"
[[agent]]
name = "bot"
match = "*"
provider = "pcm_ws"
url = "ws://127.0.0.1:1/nothing"
timeout_ms = 1000
"#;
    let (addr, shutdown) = start_server(&base_config(32500, 10, agents)).await;
    let r = run(UacOptions {
        server: addr,
        to: "1".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(r.status, 503);
    shutdown.cancel();
}
