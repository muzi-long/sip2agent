//! `config.toml` schema, defaults, validation and agent routing.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub sip: SipConfig,
    #[serde(default)]
    pub rtp: RtpConfig,
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default, rename = "agent")]
    pub agents: Vec<AgentConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SipConfig {
    /// UDP (and optional TCP) listen address.
    #[serde(default = "default_sip_listen")]
    pub listen: SocketAddr,
    /// Also listen on TCP at the same address.
    #[serde(default)]
    pub tcp: bool,
    /// Public IP put in SDP `c=`/`o=` and SIP Via/Contact (NAT deployments).
    #[serde(default)]
    pub external_ip: Option<IpAddr>,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    /// Source-IP allowlist (IPs / CIDRs) for out-of-dialog requests, checked
    /// against the packet source address. Empty = anyone may call.
    #[serde(default)]
    pub allow: Vec<crate::acl::IpNet>,
}

impl Default for SipConfig {
    fn default() -> Self {
        Self {
            listen: default_sip_listen(),
            tcp: false,
            external_ip: None,
            user_agent: default_user_agent(),
            allow: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtpConfig {
    /// Local IP to bind RTP sockets on.
    #[serde(default = "default_rtp_bind")]
    pub bind_ip: IpAddr,
    #[serde(default = "default_port_min")]
    pub port_min: u16,
    #[serde(default = "default_port_max")]
    pub port_max: u16,
    /// Hang up when no RTP is received for this long (0 = disabled).
    #[serde(default = "default_rtp_timeout")]
    pub timeout_secs: u64,
}

impl Default for RtpConfig {
    fn default() -> Self {
        Self {
            bind_ip: default_rtp_bind(),
            port_min: default_port_min(),
            port_max: default_port_max(),
            timeout_secs: default_rtp_timeout(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    /// tracing EnvFilter directive, e.g. `info` or `sip2agent=debug,info`.
    /// `RUST_LOG` overrides it when set.
    #[serde(default = "default_log_level")]
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Built-in loopback, for smoke tests only.
    Echo,
    /// Doubao realtime 3.0 (Seeduplex).
    Doubao,
    /// Aliyun Bailian Qwen-Omni realtime.
    QwenOmni,
    /// Generic raw PCM16 WebSocket.
    PcmWs,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Echo => "echo",
            ProviderKind::Doubao => "doubao",
            ProviderKind::QwenOmni => "qwen_omni",
            ProviderKind::PcmWs => "pcm_ws",
        }
    }
}

/// A function tool exposed to the model (JSON Schema parameters).
#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema object; defaults to an empty object schema.
    #[serde(default = "default_tool_params")]
    pub parameters: serde_json::Value,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    /// Called number (request-URI user) to match exactly, or `*` as default.
    #[serde(rename = "match")]
    pub match_user: String,
    pub provider: ProviderKind,

    /// WebSocket URL override. Defaults per provider (pcm_ws requires it).
    #[serde(default)]
    pub url: Option<String>,
    /// Model id (qwen_omni: query `model=`; doubao: `session.model`).
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
    /// Opening line spoken when the call is answered.
    #[serde(default)]
    pub greeting: Option<String>,
    /// PCM sample rate for pcm_ws (default 8000).
    #[serde(default)]
    pub sample_rate: Option<u32>,
    /// Extra static WebSocket upgrade headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// qwen_omni turn detection: `server_vad` (default) or `semantic_vad`.
    #[serde(default)]
    pub turn_detection: Option<String>,
    /// doubao: top-level `extension` passthrough object (asr/tts/dialog).
    #[serde(default)]
    pub extension: Option<serde_json::Value>,

    #[serde(default)]
    pub tool_webhook: Option<String>,
    #[serde(default)]
    pub tools: Vec<ToolDef>,
    #[serde(default)]
    pub call_end_webhook: Option<String>,
    /// WebSocket connect / session-ready / webhook HTTP timeout.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the API key.
        f.debug_struct("AgentConfig")
            .field("name", &self.name)
            .field("match", &self.match_user)
            .field("provider", &self.provider)
            .field("url", &self.url)
            .field("model", &self.model)
            .field("voice", &self.voice)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("api_key_env", &self.api_key_env)
            .field("tools", &self.tools.len())
            .finish_non_exhaustive()
    }
}

impl AgentConfig {
    /// Resolve the API key: inline `api_key` wins, else the `api_key_env` variable.
    pub fn resolve_api_key(&self) -> Option<String> {
        if let Some(k) = self.api_key.as_deref().filter(|k| !k.is_empty()) {
            return Some(k.to_string());
        }
        self.api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|k| !k.is_empty())
    }
}

fn default_sip_listen() -> SocketAddr {
    "0.0.0.0:5060".parse().expect("static addr")
}
fn default_user_agent() -> String {
    format!("sip2agent/{}", env!("CARGO_PKG_VERSION"))
}
fn default_rtp_bind() -> IpAddr {
    "0.0.0.0".parse().expect("static ip")
}
fn default_port_min() -> u16 {
    20000
}
fn default_port_max() -> u16 {
    20999
}
fn default_rtp_timeout() -> u64 {
    30
}
fn default_log_level() -> String {
    "info".into()
}
fn default_timeout_ms() -> u64 {
    10_000
}
fn default_tool_params() -> serde_json::Value {
    serde_json::json!({"type": "object", "properties": {}})
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text).context("parse config")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.rtp.port_min > self.rtp.port_max {
            bail!("rtp.port_min must be <= rtp.port_max");
        }
        let mut names = std::collections::HashSet::new();
        let mut matches = std::collections::HashSet::new();
        for a in &self.agents {
            if a.name.is_empty() {
                bail!("agent name must not be empty");
            }
            if !names.insert(a.name.as_str()) {
                bail!("duplicate agent name '{}'", a.name);
            }
            if !matches.insert(a.match_user.as_str()) {
                bail!("duplicate agent match '{}'", a.match_user);
            }
            match a.provider {
                ProviderKind::Echo => {}
                ProviderKind::PcmWs => {
                    if a.url.as_deref().unwrap_or("").is_empty() {
                        bail!("agent '{}': provider pcm_ws requires `url`", a.name);
                    }
                    if let Some(r) = a.sample_rate
                        && !(8000..=48000).contains(&r)
                    {
                        bail!("agent '{}': sample_rate {r} out of range", a.name);
                    }
                }
                ProviderKind::QwenOmni => {
                    if a.url.as_deref().unwrap_or("").is_empty() {
                        bail!("agent '{}': provider qwen_omni requires `url`", a.name);
                    }
                    if a.api_key.is_none() && a.api_key_env.is_none() {
                        bail!("agent '{}': `api_key` or `api_key_env` is required", a.name);
                    }
                }
                ProviderKind::Doubao => {
                    if a.voice.as_deref().unwrap_or("").is_empty() {
                        bail!("agent '{}': provider doubao requires `voice`", a.name);
                    }
                    if a.api_key.is_none() && a.api_key_env.is_none() {
                        bail!("agent '{}': `api_key` or `api_key_env` is required", a.name);
                    }
                }
            }
            if !a.tools.is_empty() && a.tool_webhook.is_none() {
                tracing::warn!(
                    agent = %a.name,
                    "tools configured without tool_webhook: tool calls will return an error"
                );
            }
        }
        Ok(())
    }

    /// Route a called number: exact match first, then `*`.
    pub fn match_agent(&self, called: &str) -> Option<&AgentConfig> {
        self.agents
            .iter()
            .find(|a| a.match_user == called)
            .or_else(|| self.agents.iter().find(|a| a.match_user == "*"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[sip]
listen = "127.0.0.1:5070"
external_ip = "203.0.113.5"

[rtp]
port_min = 30000
port_max = 30010

[[agent]]
name = "echo"
match = "9000"
provider = "echo"

[[agent]]
name = "qwen"
match = "*"
provider = "qwen_omni"
url = "wss://ws-123.cn-beijing.maas.aliyuncs.com/api-ws/v1/realtime"
api_key_env = "DASHSCOPE_API_KEY"
greeting = "您好"

[[agent.tools]]
name = "query_order"
description = "查询订单"
parameters = { type = "object", properties = { order_id = { type = "string" } }, required = ["order_id"] }
"#;

    #[test]
    fn parses_and_routes() {
        let cfg = Config::parse(SAMPLE).unwrap();
        assert_eq!(cfg.sip.listen.port(), 5070);
        assert_eq!(cfg.rtp.port_max, 30010);
        assert_eq!(cfg.match_agent("9000").unwrap().name, "echo");
        assert_eq!(cfg.match_agent("12345").unwrap().name, "qwen");
        let q = cfg.match_agent("x").unwrap();
        assert_eq!(q.tools.len(), 1);
        assert_eq!(q.tools[0].parameters["required"][0], "order_id");
        assert_eq!(q.timeout_ms, 10_000);
    }

    #[test]
    fn example_config_is_valid() {
        let cfg = Config::parse(include_str!("../config.example.toml")).unwrap();
        assert_eq!(cfg.agents.len(), 4);
        assert_eq!(
            cfg.match_agent("9000").unwrap().provider,
            ProviderKind::Echo
        );
        assert_eq!(
            cfg.match_agent("1000").unwrap().provider,
            ProviderKind::Doubao
        );
        assert_eq!(
            cfg.match_agent("2000").unwrap().provider,
            ProviderKind::QwenOmni
        );
        assert_eq!(
            cfg.match_agent("777").unwrap().provider,
            ProviderKind::PcmWs
        );
    }

    #[test]
    fn no_default_means_none() {
        let cfg = Config::parse(
            r#"
[[agent]]
name = "echo"
match = "1"
provider = "echo"
"#,
        )
        .unwrap();
        assert!(cfg.match_agent("2").is_none());
    }

    #[test]
    fn rejects_invalid() {
        assert!(Config::parse("[[agent]]\nname='p'\nmatch='1'\nprovider='pcm_ws'\n").is_err());
        assert!(
            Config::parse("[[agent]]\nname='d'\nmatch='1'\nprovider='doubao'\napi_key='k'\n")
                .is_err(),
            "doubao needs voice"
        );
        assert!(Config::parse("[[agent]]\nname='x'\nmatch='1'\nprovider='openai'\n").is_err());
        assert!(Config::parse("[sip]\nbogus = 1\n").is_err());
        assert!(Config::parse("[sip]\nallow = [\"10.0.0.0/33\"]\n").is_err());
        assert!(Config::parse("[sip]\nallow = [\"not-an-ip\"]\n").is_err());
    }

    #[test]
    fn sip_allow_parses() {
        assert!(Config::parse("").unwrap().sip.allow.is_empty());
        let cfg =
            Config::parse("[sip]\nallow = [\"10.0.0.0/8\", \"203.0.113.7\", \"::1\"]\n").unwrap();
        assert_eq!(cfg.sip.allow.len(), 3);
        assert!(crate::acl::allowed(
            &cfg.sip.allow,
            "203.0.113.7".parse().unwrap()
        ));
        assert!(!crate::acl::allowed(
            &cfg.sip.allow,
            "203.0.113.8".parse().unwrap()
        ));
    }

    #[test]
    fn api_key_resolution_and_redaction() {
        let mut cfg = Config::parse(SAMPLE).unwrap();
        let a = &mut cfg.agents[1];
        a.api_key = Some("sk-secret".into());
        assert_eq!(a.resolve_api_key().as_deref(), Some("sk-secret"));
        let dbg = format!("{a:?}");
        assert!(!dbg.contains("sk-secret"));
        a.api_key = None;
        a.api_key_env = Some("SIP2AGENT_TEST_NONEXISTENT_ENV".into());
        assert_eq!(a.resolve_api_key(), None);
    }
}
