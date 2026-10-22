//! Reference `pcm_ws` server: echoes caller PCM back; optional hangup.
//!
//! cargo run --example pcm_ws_echo -- --listen 127.0.0.1:9100 [--hangup-after-secs 5]
//!
//! Wire: binary PCM16-LE mono both ways (rate = agent `sample_rate`, default
//! 8000); text `{"type":"clear"}` = barge-in, `{"type":"hangup"}` = hang up.

use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut listen = "127.0.0.1:9100".to_string();
    let mut hangup_after: Option<f64> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--listen" => listen = args.next().unwrap_or(listen),
            "--hangup-after-secs" => hangup_after = args.next().and_then(|v| v.parse().ok()),
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    let listener = TcpListener::bind(&listen).await?;
    println!("pcm_ws echo listening on ws://{listen}");
    while let Ok((sock, peer)) = listener.accept().await {
        tokio::spawn(async move {
            let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await else {
                return;
            };
            println!("{peer}: connected");
            let start = std::time::Instant::now();
            let mut frames = 0u64;
            while let Some(Ok(msg)) = ws.next().await {
                match msg {
                    Message::Binary(b) => {
                        frames += 1;
                        if ws.send(Message::Binary(b)).await.is_err() {
                            break;
                        }
                        if let Some(s) = hangup_after
                            && start.elapsed().as_secs_f64() >= s
                        {
                            let _ = ws.send(Message::Text(r#"{"type":"hangup"}"#.into())).await;
                            hangup_after = None;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            println!("{peer}: closed after {frames} frames");
        });
    }
    Ok(())
}
