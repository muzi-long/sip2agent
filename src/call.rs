//! Per-call media orchestration: agent session (echo or realtime bridge),
//! RTP inactivity watchdog, outcome + transcripts.

use crate::config::{AgentConfig, ProviderKind};
use crate::realtime::bridge::{BridgeEnd, WsStream, connect, run_bridge};
use crate::realtime::{RealtimeProtocol, protocol_for};
use crate::rtp::{MediaPort, RtpSession};
use crate::webhook::{ToolDispatcher, TranscriptEntry};
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Agent session prepared before answering (the vendor WebSocket is opened
/// first so a vendor outage yields 503 instead of a dead answered call).
pub enum AgentSession {
    Echo,
    Realtime {
        protocol: Box<dyn RealtimeProtocol>,
        ws: Box<WsStream>,
    },
}

pub async fn prepare_agent(agent: &AgentConfig) -> Result<AgentSession> {
    if agent.provider == ProviderKind::Echo {
        return Ok(AgentSession::Echo);
    }
    let protocol = protocol_for(agent)?;
    let ws = connect(protocol.as_ref(), Duration::from_millis(agent.timeout_ms)).await?;
    Ok(AgentSession::Realtime {
        protocol,
        ws: Box::new(ws),
    })
}

/// How the media phase ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaEnd {
    /// The SIP side ended the call (caller BYE, ACK timeout, shutdown...).
    Remote,
    /// No RTP received for `rtp.timeout_secs`.
    RtpTimeout,
    /// The agent ended (realtime only).
    Agent(BridgeEnd),
}

pub struct MediaOutcome {
    pub end: MediaEnd,
    pub transcripts: Vec<TranscriptEntry>,
}

pub struct MediaParams {
    pub call_id: String,
    pub agent: Arc<AgentConfig>,
    pub rtp_timeout: Option<Duration>,
}

/// Run the agent against the call's media until either side ends.
pub async fn run_media(
    params: MediaParams,
    session: AgentSession,
    media: MediaPort,
    rtp: &RtpSession,
    remote_cancel: CancellationToken,
) -> MediaOutcome {
    let agent_cancel = CancellationToken::new();
    let agent = params.agent.clone();
    let call_id = params.call_id.clone();
    let agent_task = {
        let agent_cancel = agent_cancel.clone();
        tokio::spawn(async move {
            match session {
                AgentSession::Echo => {
                    run_echo(media, agent_cancel).await;
                    None
                }
                AgentSession::Realtime { protocol, ws } => {
                    let tools = ToolDispatcher::new(
                        agent.tool_webhook.clone(),
                        call_id,
                        agent.name.clone(),
                        Duration::from_millis(agent.timeout_ms),
                    );
                    Some(
                        run_bridge(
                            protocol,
                            *ws,
                            media,
                            tools,
                            Duration::from_millis(agent.timeout_ms),
                            agent_cancel,
                        )
                        .await,
                    )
                }
            }
        })
    };
    tokio::pin!(agent_task);

    let mut watchdog = tokio::time::interval(Duration::from_millis(500));
    let mut end: Option<MediaEnd> = None;
    let result = loop {
        tokio::select! {
            r = &mut agent_task => break r,
            _ = remote_cancel.cancelled(), if end.is_none() => {
                end = Some(MediaEnd::Remote);
                agent_cancel.cancel();
            }
            _ = watchdog.tick(), if end.is_none() => {
                if let Some(t) = params.rtp_timeout
                    && rtp.idle_for() >= t
                {
                    info!(call_id = %params.call_id, "rtp timeout");
                    end = Some(MediaEnd::RtpTimeout);
                    agent_cancel.cancel();
                }
            }
        }
    };

    let (bridge_end, transcripts) = match result {
        Ok(Some(r)) => (Some(r.end), r.transcripts),
        Ok(None) => (None, Vec::new()),
        Err(e) => (
            Some(BridgeEnd::WsError(format!("agent task: {e}"))),
            Vec::new(),
        ),
    };
    let end = match (end, bridge_end) {
        (Some(e), _) => e,
        (None, Some(b)) => MediaEnd::Agent(b),
        (None, None) => MediaEnd::Remote,
    };
    MediaOutcome { end, transcripts }
}

/// Built-in loopback agent: plays the caller's audio straight back.
pub async fn run_echo(mut media: MediaPort, cancel: CancellationToken) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            f = media.uplink.recv() => match f {
                Some(frame) => {
                    if media.downlink.send(frame).await.is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
    }
}
