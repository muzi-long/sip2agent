//! Tool-call webhook and call-end webhook.
//!
//! Tool webhook: `POST <tool_webhook>` with
//! `{"call_id","agent","tool","tool_call_id","arguments"}`; the response body
//! (as text) is returned to the model as the tool output.
//!
//! Call-end webhook: `POST <call_end_webhook>` with a [`CallEndReport`].

use crate::realtime::{ToolCall, ToolOutput};
use serde::Serialize;
use std::time::Duration;
use tracing::{info, warn};

/// One transcript line.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TranscriptEntry {
    /// `user` or `assistant`.
    pub role: String,
    pub text: String,
    /// RFC3339.
    pub time: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallEndReport {
    pub call_id: String,
    pub agent: String,
    pub from: String,
    pub to: String,
    /// Answer time, RFC3339.
    pub start_time: String,
    /// RFC3339.
    pub end_time: String,
    pub duration_secs: f64,
    pub hangup_reason: String,
    pub transcripts: Vec<TranscriptEntry>,
}

pub fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}

fn http_client(timeout: Duration) -> reqwest::Client {
    crate::install_crypto_provider();
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Executes model tool calls through the agent's webhook.
#[derive(Clone)]
pub struct ToolDispatcher {
    webhook: Option<String>,
    client: reqwest::Client,
    call_id: String,
    agent: String,
}

impl ToolDispatcher {
    pub fn new(webhook: Option<String>, call_id: String, agent: String, timeout: Duration) -> Self {
        Self {
            webhook,
            client: http_client(timeout),
            call_id,
            agent,
        }
    }

    /// Execute one call. Never fails: errors become a JSON error string the
    /// model can read.
    pub async fn execute(&self, call: &ToolCall) -> ToolOutput {
        let output = match &self.webhook {
            None => error_json("no tool_webhook configured"),
            Some(url) => {
                let arguments: serde_json::Value = serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone()));
                let body = serde_json::json!({
                    "call_id": self.call_id,
                    "agent": self.agent,
                    "tool": call.name,
                    "tool_call_id": call.call_id,
                    "arguments": arguments,
                });
                info!(call_id = %self.call_id, tool = %call.name, "tool webhook request");
                match self.client.post(url).json(&body).send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        let text = resp.text().await.unwrap_or_default();
                        if status.is_success() {
                            text
                        } else {
                            warn!(call_id = %self.call_id, tool = %call.name, %status, "tool webhook non-2xx");
                            error_json(&format!("tool webhook returned HTTP {}", status.as_u16()))
                        }
                    }
                    Err(e) => {
                        warn!(call_id = %self.call_id, tool = %call.name, error = %e, "tool webhook failed");
                        error_json("tool webhook request failed")
                    }
                }
            }
        };
        ToolOutput {
            call_id: call.call_id.clone(),
            output,
        }
    }

    /// Execute a batch concurrently, preserving order.
    pub async fn execute_all(&self, calls: &[ToolCall]) -> Vec<ToolOutput> {
        futures::future::join_all(calls.iter().map(|c| self.execute(c))).await
    }
}

fn error_json(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

/// Fire the call-end webhook (best effort, logged).
pub async fn send_call_end(url: &str, report: &CallEndReport, timeout: Duration) {
    match http_client(timeout).post(url).json(report).send().await {
        Ok(resp) if resp.status().is_success() => {
            info!(call_id = %report.call_id, "call-end webhook delivered");
        }
        Ok(resp) => {
            warn!(call_id = %report.call_id, status = %resp.status(), "call-end webhook non-2xx");
        }
        Err(e) => {
            warn!(call_id = %report.call_id, error = %e, "call-end webhook failed");
        }
    }
}
