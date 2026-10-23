//! Admin API end to end: in-process sip2agent + admin HTTP server on an
//! ephemeral port, driven through the same client the CLI uses.

use serde_json::Value;
use sip2agent::admin::{self, AdminState, ReloadResponse, Reloader, StatusResponse};
use sip2agent::config::{Config, SharedConfig};
use sip2agent::sip::SipServer;
use sip2agent::uac::{UacOptions, run};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

fn config_text(port_min: u16, called: &str) -> String {
    format!(
        r#"
[sip]
listen = "127.0.0.1:0"
[rtp]
bind_ip = "127.0.0.1"
port_min = {port_min}
port_max = {}
[[agent]]
name = "echo"
match = "{called}"
provider = "echo"
"#,
        port_min + 40
    )
}

struct Harness {
    sip: SocketAddr,
    admin: SocketAddr,
    path: PathBuf,
    dir: PathBuf,
    run: JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

async fn start(config: &str) -> Harness {
    sip2agent::install_crypto_provider();
    let dir = std::env::temp_dir().join(format!("sip2agent-admin-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, config).unwrap();
    let shared = Arc::new(SharedConfig::new(Config::load(&path).unwrap()));
    let server = SipServer::bind_shared(shared.clone()).await.unwrap();
    let sip = server.local_addr();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin = listener.local_addr().unwrap();
    tokio::spawn(admin::serve(
        listener,
        Arc::new(AdminState {
            reloader: Arc::new(Reloader::new(shared, path.clone(), None)),
            control: server.control(),
        }),
    ));
    let run = tokio::spawn(async move {
        server.run(CancellationToken::new()).await.ok();
    });
    Harness {
        sip,
        admin,
        path,
        dir,
        run,
    }
}

async fn get_status(admin: SocketAddr) -> StatusResponse {
    let (code, body) = admin::http_call(admin, "GET", "/status", None)
        .await
        .unwrap();
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// Raw HTTP request with custom headers (for the Origin / Host checks).
async fn raw_http(addr: SocketAddr, head: &str) -> u16 {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(head.as_bytes()).await.unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).await.unwrap();
    buf.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// Out-of-dialog OPTIONS over UDP; returns (status, has Retry-After).
async fn options_probe(sip: SocketAddr) -> (u16, bool) {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local = sock.local_addr().unwrap();
    let msg = format!(
        "OPTIONS sip:probe@{sip} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {local};branch=z9hG4bK{}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:probe@{local}>;tag=t1\r\n\
         To: <sip:probe@{sip}>\r\n\
         Call-ID: {}\r\n\
         CSeq: 1 OPTIONS\r\n\
         Content-Length: 0\r\n\r\n",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4()
    );
    sock.send_to(msg.as_bytes(), sip).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let (n, _) = tokio::time::timeout(Duration::from_secs(3), sock.recv_from(&mut buf))
        .await
        .expect("OPTIONS answered")
        .unwrap();
    let text = String::from_utf8_lossy(&buf[..n]).to_string();
    let status = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, text.to_ascii_lowercase().contains("retry-after:"))
}

async fn call(sip: SocketAddr, to: &str, secs: f32) -> sip2agent::uac::UacReport {
    run(UacOptions {
        server: sip,
        to: to.into(),
        duration: Duration::from_secs_f32(secs),
        ..Default::default()
    })
    .await
    .unwrap()
}

async fn wait_run_end(run: &mut JoinHandle<()>, within: Duration) {
    tokio::time::timeout(within, run)
        .await
        .expect("server stopped in time")
        .unwrap();
}

#[tokio::test]
async fn reload_health_and_request_guards() {
    let h = start(&config_text(34000, "9000")).await;

    let (code, body) = admin::http_call(h.admin, "GET", "/health", None)
        .await
        .unwrap();
    assert_eq!(code, 200);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["ok"], true);

    // Edit the file → POST /reload → new route for new calls.
    std::fs::write(&h.path, config_text(34000, "9100")).unwrap();
    let (code, body) = admin::http_call(h.admin, "POST", "/reload", None)
        .await
        .unwrap();
    assert_eq!(code, 200, "{body}");
    let r: ReloadResponse = serde_json::from_str(&body).unwrap();
    assert!(r.ok);
    assert_eq!(r.changed, vec!["agents(~echo)"]);
    assert_eq!(r.agents.len(), 1);
    assert_eq!(r.agents[0].match_user, "9100");
    assert!(r.restart_required.is_empty());
    assert_eq!(call(h.sip, "9000", 0.3).await.status, 404);
    assert_eq!(call(h.sip, "9100", 0.3).await.status, 200);

    // Restart-required change is reported, rest applied.
    std::fs::write(
        &h.path,
        config_text(34000, "9100").replace("port_max = 34040", "port_max = 34050"),
    )
    .unwrap();
    let (_, body) = admin::http_call(h.admin, "POST", "/reload", None)
        .await
        .unwrap();
    let r: ReloadResponse = serde_json::from_str(&body).unwrap();
    assert!(r.ok);
    assert_eq!(r.restart_required, vec!["rtp.port_max"]);

    // Invalid file → 422 with the error; the running config is kept.
    std::fs::write(
        &h.path,
        "[[agent]]\nname='p'\nmatch='1'\nprovider='pcm_ws'\n",
    )
    .unwrap();
    let (code, body) = admin::http_call(h.admin, "POST", "/reload", None)
        .await
        .unwrap();
    assert_eq!(code, 422);
    let r: ReloadResponse = serde_json::from_str(&body).unwrap();
    assert!(!r.ok);
    assert!(r.error.unwrap().contains("pcm_ws requires `url`"));
    assert_eq!(call(h.sip, "9100", 0.3).await.status, 200);

    // Routing / method / browser guards.
    assert_eq!(
        admin::http_call(h.admin, "GET", "/reload", None)
            .await
            .unwrap()
            .0,
        405
    );
    assert_eq!(
        admin::http_call(h.admin, "GET", "/nope", None)
            .await
            .unwrap()
            .0,
        404
    );
    let a = h.admin;
    assert_eq!(
        raw_http(
            a,
            &format!("POST /reload HTTP/1.1\r\nHost: {a}\r\nOrigin: http://evil.example\r\nContent-Length: 0\r\n\r\n")
        )
        .await,
        403
    );
    assert_eq!(
        raw_http(
            a,
            "POST /stop HTTP/1.1\r\nHost: evil.example:9060\r\nContent-Length: 0\r\n\r\n"
        )
        .await,
        403
    );
    // The guarded /stop above must not have stopped anything.
    assert!(!get_status(a).await.draining);
    assert_eq!(call(h.sip, "9100", 0.3).await.status, 200);
}

/// Drain: active call keeps flowing, new INVITE / OPTIONS get 503, the
/// server stops once the call hangs up.
#[tokio::test]
async fn drain_waits_for_active_calls() {
    let mut h = start(&config_text(34100, "9000")).await;
    let sip = h.sip;
    let ongoing = tokio::spawn(async move { call(sip, "9000", 2.5).await });
    tokio::time::sleep(Duration::from_millis(600)).await;

    let s = get_status(h.admin).await;
    assert_eq!(s.active_calls, 1);
    assert_eq!(s.per_agent.get("echo"), Some(&1));
    assert!(!s.draining);
    assert_eq!(options_probe(sip).await.0, 200, "OPTIONS 200 before drain");

    let (code, body) = admin::http_call(
        h.admin,
        "POST",
        "/stop",
        Some(r#"{"drain":true,"timeout_secs":30}"#),
    )
    .await
    .unwrap();
    assert_eq!(code, 200, "{body}");
    let r: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(r["active_calls"], 1);
    assert_eq!(r["draining"], true);
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(get_status(h.admin).await.draining);
    assert_eq!(call(sip, "9000", 0.3).await.status, 503, "new call refused");
    assert_eq!(
        options_probe(sip).await,
        (503, true),
        "OPTIONS 503 + Retry-After"
    );

    let started = Instant::now();
    let r = ongoing.await.unwrap();
    assert_eq!(r.status, 200);
    assert!(!r.remote_hangup, "drained call must not be hung up: {r:?}");
    assert!(r.rtp_received > 50, "{r:?}");
    wait_run_end(&mut h.run, Duration::from_secs(5)).await;
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// Drain timeout → remaining calls get BYE; a second, immediate stop while
/// draining upgrades it.
#[tokio::test]
async fn drain_timeout_and_upgrade_to_immediate() {
    // Timeout path.
    let mut h = start(&config_text(34200, "9000")).await;
    let sip = h.sip;
    let ongoing = tokio::spawn(async move { call(sip, "9000", 8.0).await });
    tokio::time::sleep(Duration::from_millis(600)).await;
    let t0 = Instant::now();
    admin::http_call(
        h.admin,
        "POST",
        "/stop",
        Some(r#"{"drain":true,"timeout_secs":1}"#),
    )
    .await
    .unwrap();
    let r = ongoing.await.unwrap();
    assert!(r.remote_hangup, "timed-out drain BYEs the call: {r:?}");
    assert!(t0.elapsed() < Duration::from_secs(4), "{:?}", t0.elapsed());
    wait_run_end(&mut h.run, Duration::from_secs(5)).await;

    // Upgrade path: long drain, then a plain stop.
    let mut h = start(&config_text(34300, "9000")).await;
    let sip = h.sip;
    let ongoing = tokio::spawn(async move { call(sip, "9000", 8.0).await });
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (_, body) = admin::http_call(h.admin, "POST", "/stop", Some(r#"{"drain":true}"#))
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["timeout_secs"],
        600
    );
    let (_, body) = admin::http_call(h.admin, "POST", "/stop", Some(r#"{"drain":true}"#))
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["already_draining"],
        true
    );
    let t0 = Instant::now();
    admin::http_call(h.admin, "POST", "/stop", Some(r#"{"drain":false}"#))
        .await
        .unwrap();
    let r = ongoing.await.unwrap();
    assert!(r.remote_hangup, "{r:?}");
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
    wait_run_end(&mut h.run, Duration::from_secs(5)).await;
}
