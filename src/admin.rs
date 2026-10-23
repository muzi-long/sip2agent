//! Local admin API (`[admin] listen`, default `127.0.0.1:9060`) and the CLI
//! client behind `sip2agent reload | stop | status`.
//!
//! Endpoints (JSON, `Connection: close`):
//! - `POST /reload` — re-read the server's OWN config file (no config content
//!   is accepted) through the same path as SIGHUP; serialized.
//! - `POST /stop` `{drain, timeout_secs}` — immediate graceful stop, or drain
//!   (new INVITE / OPTIONS → 503, exit when active calls reach 0, BYE the
//!   rest after the timeout). A non-drain stop while draining stops now.
//! - `GET /status`, `GET /health`.
//!
//! Requests carrying an `Origin` header or a non-IP / non-localhost `Host`
//! are refused (403), so a web page cannot drive the API from a browser
//! (CSRF / DNS rebinding).

use crate::config::{Config, DEFAULT_ADMIN_LISTEN, LogConfig, SharedConfig};
use crate::sip::ServerControl;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};

/// Default drain timeout for `POST /stop {"drain": true}`.
pub const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 600;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Applies a changed `log.level` (the binary owns the tracing filter).
pub type LogApplier = Arc<dyn Fn(&LogConfig) + Send + Sync>;

/// Reloads the config file; shared by SIGHUP and `POST /reload`.
pub struct Reloader {
    shared: Arc<SharedConfig>,
    path: PathBuf,
    on_log: Option<LogApplier>,
    lock: tokio::sync::Mutex<()>,
}

impl Reloader {
    pub fn new(shared: Arc<SharedConfig>, path: PathBuf, on_log: Option<LogApplier>) -> Self {
        Self {
            shared,
            path,
            on_log,
            lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-read and apply the config file. Invalid files leave the running
    /// config untouched. Concurrent reloads are serialized.
    pub async fn reload(&self, source: &str) -> ReloadResponse {
        let _guard = self.lock.lock().await;
        info!(source, path = %self.path.display(), "reloading config");
        match self.shared.reload_file(&self.path) {
            Ok(report) => {
                if let (Some(log), Some(apply)) = (&report.log_level, &self.on_log) {
                    apply(log);
                }
                let cfg = self.shared.load();
                ReloadResponse {
                    ok: true,
                    changed: report.changed,
                    agents: agent_summaries(&cfg),
                    allow_count: cfg.sip.allow.len(),
                    restart_required: report
                        .restart_required
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                    error: None,
                }
            }
            Err(e) => {
                let msg = format!("{e:#}");
                error!(error = %msg, "config reload failed; keeping the running config");
                ReloadResponse {
                    ok: false,
                    error: Some(msg),
                    ..Default::default()
                }
            }
        }
    }
}

fn agent_summaries(cfg: &Config) -> Vec<AgentSummary> {
    cfg.agents
        .iter()
        .map(|a| AgentSummary {
            name: a.name.clone(),
            match_user: a.match_user.clone(),
        })
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentSummary {
    pub name: String,
    #[serde(rename = "match")]
    pub match_user: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReloadResponse {
    pub ok: bool,
    #[serde(default)]
    pub changed: Vec<String>,
    #[serde(default)]
    pub agents: Vec<AgentSummary>,
    #[serde(default)]
    pub allow_count: usize,
    #[serde(default)]
    pub restart_required: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub version: String,
    pub uptime_secs: u64,
    pub draining: bool,
    pub active_calls: usize,
    #[serde(default)]
    pub per_agent: BTreeMap<String, usize>,
    pub config_path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StopRequest {
    #[serde(default)]
    pub drain: bool,
    #[serde(default = "default_drain_timeout")]
    pub timeout_secs: u64,
}

fn default_drain_timeout() -> u64 {
    DEFAULT_DRAIN_TIMEOUT_SECS
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StopResponse {
    pub ok: bool,
    pub active_calls: usize,
    pub draining: bool,
    /// The drain was already running (its original timeout stays).
    #[serde(default)]
    pub already_draining: bool,
    #[serde(default)]
    pub timeout_secs: u64,
}

// ───────────────────────────── server ─────────────────────────────

pub struct AdminState {
    pub reloader: Arc<Reloader>,
    pub control: Arc<ServerControl>,
}

/// Serve the admin API on an already-bound listener until the task is dropped.
pub async fn serve(listener: TcpListener, state: Arc<AdminState>) {
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "admin accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(sock, &state).await {
                debug!(%peer, error = %e, "admin request failed");
            }
        });
    }
}

/// Runs after the response is written (so the client gets an answer even
/// when the process exits right away).
enum After {
    Nothing,
    StopNow,
    Drain(Duration),
}

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

async fn handle_conn(mut sock: TcpStream, state: &AdminState) -> Result<()> {
    let req = tokio::time::timeout(REQUEST_READ_TIMEOUT, read_request(&mut sock)).await??;
    let (status, body, after) = route(&req, state).await;
    let resp = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        reason_phrase(status),
        body.len()
    );
    sock.write_all(resp.as_bytes()).await?;
    sock.shutdown().await.ok();
    match after {
        After::Nothing => {}
        After::StopNow => state.control.stop_now(),
        After::Drain(t) => {
            state.control.start_drain(t);
        }
    }
    Ok(())
}

async fn read_request(sock: &mut TcpStream) -> Result<HttpRequest> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        if buf.len() > MAX_REQUEST_BYTES {
            bail!("request too large");
        }
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            bail!("connection closed before request head");
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end])?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let method = first.next().unwrap_or_default().to_string();
    let path = first.next().unwrap_or_default().to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_REQUEST_BYTES {
        bail!("request body too large");
    }
    while buf.len() < head_end + len {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = buf[head_end..(head_end + len).min(buf.len())].to_vec();
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

/// `Host` must be an IP literal or `localhost` (optionally with a port), so
/// a DNS-rebound hostname from a browser is refused.
fn host_is_local_literal(host: &str) -> bool {
    let h = host.trim();
    if h.parse::<SocketAddr>().is_ok() || h.parse::<IpAddr>().is_ok() {
        return true;
    }
    let name = h.rsplit_once(':').map_or(
        h,
        |(n, port)| {
            if port.parse::<u16>().is_ok() { n } else { h }
        },
    );
    name.eq_ignore_ascii_case("localhost")
}

fn json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "{}".into())
}

fn error_body(msg: &str) -> String {
    json(&serde_json::json!({ "ok": false, "error": msg }))
}

async fn route(req: &HttpRequest, state: &AdminState) -> (u16, String, After) {
    if req.header("origin").is_some() {
        return (
            403,
            error_body("browser requests are not allowed"),
            After::Nothing,
        );
    }
    if let Some(host) = req.header("host")
        && !host_is_local_literal(host)
    {
        return (
            403,
            error_body("Host must be an IP address or localhost"),
            After::Nothing,
        );
    }
    let path = req.path.split('?').next().unwrap_or_default();
    let allowed = match path {
        "/reload" | "/stop" => "POST",
        "/status" | "/health" => "GET",
        _ => return (404, error_body("not found"), After::Nothing),
    };
    if req.method != allowed {
        return (405, error_body("method not allowed"), After::Nothing);
    }
    let control = &state.control;
    match path {
        "/health" => (
            200,
            json(&serde_json::json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") })),
            After::Nothing,
        ),
        "/status" => (
            200,
            json(&StatusResponse {
                version: env!("CARGO_PKG_VERSION").to_string(),
                uptime_secs: control.uptime().as_secs(),
                draining: control.is_draining(),
                active_calls: control.active_calls(),
                per_agent: control.per_agent(),
                config_path: state.reloader.path().display().to_string(),
            }),
            After::Nothing,
        ),
        "/reload" => {
            let r = state.reloader.reload("admin").await;
            (if r.ok { 200 } else { 422 }, json(&r), After::Nothing)
        }
        "/stop" => {
            let stop: StopRequest = if req.body.iter().all(u8::is_ascii_whitespace) {
                StopRequest {
                    drain: false,
                    timeout_secs: DEFAULT_DRAIN_TIMEOUT_SECS,
                }
            } else {
                match serde_json::from_slice(&req.body) {
                    Ok(s) => s,
                    Err(e) => {
                        return (400, error_body(&format!("bad body: {e}")), After::Nothing);
                    }
                }
            };
            let already = control.is_draining();
            let resp = StopResponse {
                ok: true,
                active_calls: control.active_calls(),
                draining: stop.drain || already,
                already_draining: already && stop.drain,
                timeout_secs: stop.timeout_secs,
            };
            info!(
                drain = stop.drain,
                timeout_secs = stop.timeout_secs,
                active_calls = resp.active_calls,
                "admin: stop requested"
            );
            let after = if stop.drain {
                After::Drain(Duration::from_secs(stop.timeout_secs))
            } else {
                After::StopNow
            };
            (200, json(&resp), after)
        }
        _ => unreachable!("routes checked above"),
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        422 => "Unprocessable Entity",
        _ => "Error",
    }
}

// ───────────────────────────── client ─────────────────────────────

#[derive(Debug)]
pub enum AdminError {
    /// Nothing listening (server not running / wrong address).
    Unreachable(String),
    Protocol(String),
}

impl std::fmt::Display for AdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdminError::Unreachable(m) | AdminError::Protocol(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for AdminError {}

/// One HTTP/1.1 request to the admin API; returns (status, body).
pub async fn http_call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> std::result::Result<(u16, String), AdminError> {
    let fut = async {
        let mut sock = TcpStream::connect(addr)
            .await
            .map_err(|e| AdminError::Unreachable(format!("connect {addr}: {e}")))?;
        let body = body.unwrap_or("");
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let io = |e: std::io::Error| AdminError::Protocol(format!("admin API I/O: {e}"));
        sock.write_all(req.as_bytes()).await.map_err(io)?;
        let mut buf = Vec::new();
        sock.read_to_end(&mut buf).await.map_err(io)?;
        let text = String::from_utf8_lossy(&buf).to_string();
        let (head, body) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| AdminError::Protocol("malformed admin response".into()))?;
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| AdminError::Protocol("malformed admin status line".into()))?;
        Ok((status, body.to_string()))
    };
    tokio::time::timeout(CLIENT_TIMEOUT, fut)
        .await
        .map_err(|_| AdminError::Protocol(format!("admin API {addr} timed out")))?
}

/// `[admin] listen` read leniently from a config file (only that key).
/// `None` when the file is missing / unparsable or has no `[admin]` listen.
pub fn admin_listen_from_file(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: toml::Value = toml::from_str(&text).ok()?;
    v.get("admin")?.get("listen")?.as_str().map(str::to_string)
}

/// Admin address for the CLI: `--address` > `[admin] listen` from the
/// config file > `127.0.0.1:9060`. A wildcard listen address is reached via
/// loopback.
pub fn resolve_address(cli: Option<&str>, file_listen: Option<&str>) -> Result<SocketAddr> {
    let raw = match (cli, file_listen) {
        (Some(a), _) => a.trim().to_string(),
        (None, Some(l)) if l.trim().is_empty() => {
            bail!("admin.listen 为空(管理接口已关闭);请在配置里开启,或用 --address 指定")
        }
        (None, Some(l)) => l.trim().to_string(),
        (None, None) => DEFAULT_ADMIN_LISTEN.to_string(),
    };
    let mut addr: SocketAddr = raw
        .parse()
        .map_err(|_| anyhow::anyhow!("无效的管理接口地址 '{raw}'(应为 ip:port)"))?;
    if addr.ip().is_unspecified() {
        addr.set_ip(match addr.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        });
    }
    Ok(addr)
}

// ───────────────────────────── CLI output ─────────────────────────────

pub fn format_reload(r: &ReloadResponse) -> String {
    if !r.ok {
        return format!(
            "✗ 重新加载失败,服务继续使用当前配置:\n  {}",
            r.error.as_deref().unwrap_or("未知错误")
        );
    }
    let mut out = String::from("✓ 配置已重新加载(只影响新呼入,进行中的通话不受影响)\n");
    if r.changed.is_empty() {
        out.push_str("  改动: 无\n");
    } else {
        out.push_str(&format!("  改动: {}\n", r.changed.join("  ")));
        if r.changed.iter().any(|c| c.starts_with("agents(")) {
            out.push_str("        (+ 新增  - 删除  ~ 修改)\n");
        }
    }
    let agents: Vec<String> = r
        .agents
        .iter()
        .map(|a| format!("{}({})", a.name, a.match_user))
        .collect();
    out.push_str(&format!(
        "  agent: {}\n",
        if agents.is_empty() {
            "无(所有呼入都会 404)".to_string()
        } else {
            agents.join(", ")
        }
    ));
    if r.allow_count == 0 {
        out.push_str("  sip.allow: 未配置(任何来源都能拨入)\n");
    } else {
        out.push_str(&format!("  sip.allow: {} 条\n", r.allow_count));
    }
    if !r.restart_required.is_empty() {
        out.push_str(&format!(
            "  ⚠ 以下字段需要重启才能生效,当前仍使用旧值: {}\n",
            r.restart_required.join(", ")
        ));
    }
    out.trim_end().to_string()
}

pub fn format_duration(secs: u64) -> String {
    let (d, h, m, s) = (
        secs / 86400,
        secs % 86400 / 3600,
        secs % 3600 / 60,
        secs % 60,
    );
    match (d, h, m) {
        (0, 0, 0) => format!("{s}秒"),
        (0, 0, _) => format!("{m}分{s}秒"),
        (0, _, _) => format!("{h}小时{m}分"),
        _ => format!("{d}天{h}小时"),
    }
}

pub fn format_status(s: &StatusResponse) -> String {
    let mut out = format!(
        "sip2agent {} 运行中(已运行 {})\n  配置文件: {}\n  状态: {}\n  进行中的通话: {}",
        s.version,
        format_duration(s.uptime_secs),
        s.config_path,
        if s.draining {
            "排空中(新呼入回 503,进行中的通话结束后退出)"
        } else {
            "正常接听"
        },
        s.active_calls
    );
    for (agent, n) in &s.per_agent {
        out.push_str(&format!("\n    {agent}: {n}"));
    }
    out
}

pub fn format_stop(r: &StopResponse, drain: bool) -> String {
    if drain && r.already_draining {
        return format!(
            "✓ 服务已在排空中(沿用第一次的超时),当前进行中的通话: {}",
            r.active_calls
        );
    }
    if drain {
        format!(
            "✓ 已进入排空模式:新呼入回 503,{} 通进行中的通话结束后自动退出(最长等待 {} 秒,超时挂断剩余通话)",
            r.active_calls, r.timeout_secs
        )
    } else {
        format!(
            "✓ 已通知服务停止,正在挂断 {} 通进行中的通话",
            r.active_calls
        )
    }
}

// ───────────────────────────── CLI commands ─────────────────────────────

pub const CLI_USAGE: &str = "usage:
  sip2agent [run] [-c|--config <config.toml>] [--check]   启动服务(--check 只校验配置)
  sip2agent reload [-c <config.toml>] [--address <ip:port>]
  sip2agent status [-c <config.toml>] [--address <ip:port>]
  sip2agent stop   [-c <config.toml>] [--address <ip:port>] [--drain] [--timeout <秒>]";

struct CliArgs {
    config: PathBuf,
    address: Option<String>,
    drain: bool,
    timeout_secs: u64,
}

fn parse_cli_args(cmd: &str, args: &[String]) -> Result<CliArgs> {
    let mut out = CliArgs {
        config: PathBuf::from("config.toml"),
        address: None,
        drain: false,
        timeout_secs: DEFAULT_DRAIN_TIMEOUT_SECS,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{a} 需要一个参数\n{CLI_USAGE}"))
        };
        match a.as_str() {
            "-c" | "--config" => out.config = PathBuf::from(value()?),
            "--address" => out.address = Some(value()?),
            "--drain" if cmd == "stop" => out.drain = true,
            "--timeout" if cmd == "stop" => {
                out.timeout_secs = value()?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--timeout 需要秒数"))?
            }
            other => bail!("未知参数 '{other}'\n{CLI_USAGE}"),
        }
    }
    Ok(out)
}

fn unreachable_hint(addr: SocketAddr, e: &AdminError) -> String {
    format!("✗ 连不上管理接口 {addr}:服务未运行,或 admin.listen 与 -c / --address 不一致\n  ({e})")
}

/// Run `reload` / `stop` / `status`; returns the process exit code.
pub async fn cli_main(cmd: &str, args: &[String]) -> i32 {
    match cli_run(cmd, args).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e:#}");
            2
        }
    }
}

async fn cli_run(cmd: &str, args: &[String]) -> Result<i32> {
    let a = parse_cli_args(cmd, args)?;
    if cmd == "reload" {
        // Validate locally first: a bad file never reaches the server.
        if let Err(e) = Config::load(&a.config) {
            println!("✗ 配置文件校验失败,未通知服务(服务继续使用当前配置):\n  {e:#}");
            return Ok(1);
        }
    }
    let addr = resolve_address(
        a.address.as_deref(),
        admin_listen_from_file(&a.config).as_deref(),
    )?;
    match cmd {
        "reload" => match http_call(addr, "POST", "/reload", None).await {
            Ok((_, body)) => {
                let r: ReloadResponse = serde_json::from_str(&body)
                    .map_err(|e| anyhow::anyhow!("unexpected response: {e}: {body}"))?;
                println!("{}", format_reload(&r));
                Ok(if r.ok { 0 } else { 1 })
            }
            Err(e) => {
                println!("{}", unreachable_hint(addr, &e));
                Ok(1)
            }
        },
        "status" => match http_call(addr, "GET", "/status", None).await {
            Ok((200, body)) => {
                let s: StatusResponse = serde_json::from_str(&body)
                    .map_err(|e| anyhow::anyhow!("unexpected response: {e}: {body}"))?;
                println!("{}", format_status(&s));
                Ok(0)
            }
            Ok((code, body)) => {
                println!("✗ 管理接口返回 {code}: {body}");
                Ok(1)
            }
            Err(e) => {
                println!("{}", unreachable_hint(addr, &e));
                Ok(1)
            }
        },
        "stop" => {
            let req = json(&StopRequest {
                drain: a.drain,
                timeout_secs: a.timeout_secs,
            });
            let r: StopResponse = match http_call(addr, "POST", "/stop", Some(&req)).await {
                Ok((200, body)) => serde_json::from_str(&body)
                    .map_err(|e| anyhow::anyhow!("unexpected response: {e}: {body}"))?,
                Ok((code, body)) => {
                    println!("✗ 管理接口返回 {code}: {body}");
                    return Ok(1);
                }
                Err(e) => {
                    println!("{}", unreachable_hint(addr, &e));
                    return Ok(1);
                }
            };
            println!("{}", format_stop(&r, a.drain));
            // Wait for the process to go away (connection refused = exited).
            let limit = if a.drain {
                Duration::from_secs(a.timeout_secs) + Duration::from_secs(15)
            } else {
                Duration::from_secs(15)
            };
            wait_until_exited(addr, limit, a.drain).await
        }
        other => bail!("未知命令 '{other}'\n{CLI_USAGE}"),
    }
}

async fn wait_until_exited(addr: SocketAddr, limit: Duration, show_progress: bool) -> Result<i32> {
    let deadline = Instant::now() + limit;
    let mut last: Option<usize> = None;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(1)).await;
        match http_call(addr, "GET", "/status", None).await {
            Err(AdminError::Unreachable(_)) => {
                println!("✓ 服务已退出");
                return Ok(0);
            }
            Ok((200, body)) => {
                if show_progress
                    && let Ok(s) = serde_json::from_str::<StatusResponse>(&body)
                    && last != Some(s.active_calls)
                {
                    println!("  剩余 {} 通通话…", s.active_calls);
                    last = Some(s.active_calls);
                }
            }
            // Mid-shutdown hiccups: keep waiting.
            Ok(_) | Err(AdminError::Protocol(_)) => {}
        }
    }
    println!("✗ 等待服务退出超时({} 秒),请检查服务日志", limit.as_secs());
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_resolution_order() {
        let d: SocketAddr = "127.0.0.1:9060".parse().unwrap();
        assert_eq!(resolve_address(None, None).unwrap(), d);
        assert_eq!(
            resolve_address(None, Some("127.0.0.1:9100")).unwrap(),
            "127.0.0.1:9100".parse().unwrap()
        );
        assert_eq!(
            resolve_address(Some("127.0.0.1:9200"), Some("127.0.0.1:9100")).unwrap(),
            "127.0.0.1:9200".parse().unwrap()
        );
        // Wildcard listen → loopback, same family.
        assert_eq!(resolve_address(None, Some("0.0.0.0:9060")).unwrap(), d);
        assert_eq!(
            resolve_address(None, Some("[::]:9060")).unwrap(),
            "[::1]:9060".parse().unwrap()
        );
        // Disabled in the file and no --address → error; --address still wins.
        assert!(resolve_address(None, Some("")).is_err());
        assert!(resolve_address(Some("127.0.0.1:1"), Some("")).is_ok());
        assert!(resolve_address(Some("nope"), None).is_err());
    }

    #[test]
    fn admin_listen_read_leniently() {
        let dir = std::env::temp_dir().join(format!("sip2agent-admin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.toml");
        assert_eq!(admin_listen_from_file(&p), None, "missing file");
        std::fs::write(
            &p,
            "[admin]\nlisten = \"127.0.0.1:9100\"\n[[agent]]\nbogus=1\n",
        )
        .unwrap();
        assert_eq!(
            admin_listen_from_file(&p).as_deref(),
            Some("127.0.0.1:9100"),
            "only [admin] matters"
        );
        std::fs::write(&p, "[sip]\n").unwrap();
        assert_eq!(admin_listen_from_file(&p), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn host_header_check() {
        for ok in [
            "127.0.0.1:9060",
            "[::1]:9060",
            "::1",
            "localhost",
            "localhost:9060",
            "10.0.0.5",
        ] {
            assert!(host_is_local_literal(ok), "{ok}");
        }
        for bad in [
            "evil.example.com",
            "evil.example.com:9060",
            "localhost.evil.com",
        ] {
            assert!(!host_is_local_literal(bad), "{bad}");
        }
    }

    #[test]
    fn reload_formatting() {
        let r = ReloadResponse {
            ok: true,
            changed: vec!["agents(+b,~a)".into(), "sip.allow".into()],
            agents: vec![
                AgentSummary {
                    name: "a".into(),
                    match_user: "9000".into(),
                },
                AgentSummary {
                    name: "b".into(),
                    match_user: "*".into(),
                },
            ],
            allow_count: 2,
            restart_required: vec!["sip.listen".into()],
            error: None,
        };
        let s = format_reload(&r);
        assert!(s.starts_with("✓ 配置已重新加载"), "{s}");
        assert!(s.contains("改动: agents(+b,~a)  sip.allow"), "{s}");
        assert!(s.contains("(+ 新增  - 删除  ~ 修改)"), "{s}");
        assert!(s.contains("agent: a(9000), b(*)"), "{s}");
        assert!(s.contains("sip.allow: 2 条"), "{s}");
        assert!(
            s.contains("⚠ 以下字段需要重启才能生效,当前仍使用旧值: sip.listen"),
            "{s}"
        );

        let none = format_reload(&ReloadResponse {
            ok: true,
            ..Default::default()
        });
        assert!(none.contains("改动: 无") && none.contains("所有呼入都会 404"));
        assert!(none.contains("未配置(任何来源都能拨入)"));
        assert!(!none.contains('⚠'));

        let err = format_reload(&ReloadResponse {
            ok: false,
            error: Some("parse config: bad".into()),
            ..Default::default()
        });
        assert!(err.starts_with("✗ 重新加载失败") && err.contains("parse config: bad"));
    }

    #[test]
    fn status_and_stop_formatting() {
        let s = StatusResponse {
            version: "0.1.3".into(),
            uptime_secs: 3723,
            draining: true,
            active_calls: 3,
            per_agent: BTreeMap::from([("echo".into(), 1), ("qwen".into(), 2)]),
            config_path: "/app/conf/config.toml".into(),
        };
        let out = format_status(&s);
        assert!(
            out.contains("sip2agent 0.1.3 运行中(已运行 1小时2分)"),
            "{out}"
        );
        assert!(out.contains("排空中"), "{out}");
        assert!(
            out.contains("进行中的通话: 3\n    echo: 1\n    qwen: 2"),
            "{out}"
        );
        assert_eq!(format_duration(5), "5秒");
        assert_eq!(format_duration(65), "1分5秒");
        assert_eq!(format_duration(90000), "1天1小时");

        let r = StopResponse {
            ok: true,
            active_calls: 2,
            draining: true,
            already_draining: false,
            timeout_secs: 600,
        };
        assert!(format_stop(&r, true).contains("2 通进行中的通话结束后自动退出(最长等待 600 秒"));
        assert!(
            format_stop(
                &StopResponse {
                    already_draining: true,
                    ..r.clone()
                },
                true
            )
            .contains("已在排空中")
        );
        assert!(format_stop(&r, false).contains("正在挂断 2 通"));
    }

    #[test]
    fn cli_args() {
        let a = parse_cli_args(
            "stop",
            &[
                "-c".into(),
                "x.toml".into(),
                "--drain".into(),
                "--timeout".into(),
                "30".into(),
            ],
        )
        .unwrap();
        assert!(a.drain && a.timeout_secs == 30 && a.config == Path::new("x.toml"));
        assert!(
            parse_cli_args("reload", &["--drain".into()]).is_err(),
            "--drain only for stop"
        );
        assert!(parse_cli_args("status", &["--address".into()]).is_err());
    }
}
