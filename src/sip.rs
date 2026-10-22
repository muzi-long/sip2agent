//! SIP UAS on rsipstack: INVITE / ACK / BYE / CANCEL (+ in-dialog
//! re-INVITE/UPDATE/OPTIONS answered with the current SDP / 200).
//!
//! Call flow: INVITE → route by request-URI user (404 if no agent) → SDP
//! offer must contain PCMA or PCMU (488 otherwise) → 100 Trying → bind RTP
//! port and open the vendor WebSocket (503 on failure) → 200 OK with SDP →
//! media until either side ends → BYE (when we end) → call-end webhook.

use crate::call::{self, MediaEnd, MediaParams};
use crate::config::Config;
use crate::rtp::{self, PortAllocator};
use crate::sdp;
use crate::webhook::{self, CallEndReport};
use anyhow::{Context, Result, bail};
use rsipstack::EndpointBuilder;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog::{Dialog, DialogState, DialogStateReceiver, TerminatedReason};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::sip::{Header, Method, StatusCode};
use rsipstack::transaction::transaction::Transaction;
use rsipstack::transaction::{Endpoint, TransactionReceiver};
use rsipstack::transport::udp::UdpConnection;
use rsipstack::transport::{TcpListenerConnection, TransportLayer};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// How long shutdown waits for active calls to send BYE.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

struct CallHandle {
    cancel: CancellationToken,
    /// Hangup reason decided by the SIP side (caller BYE, CANCEL, shutdown...).
    remote_reason: Arc<Mutex<Option<&'static str>>>,
    /// Our current SDP answer (for re-INVITE / UPDATE refreshes).
    answer_sdp: Arc<Mutex<Option<String>>>,
}

type Calls = Arc<Mutex<HashMap<String, CallHandle>>>;

pub struct SipServer {
    cfg: Arc<Config>,
    endpoint: Endpoint,
    dialog_layer: Arc<DialogLayer>,
    ports: Arc<PortAllocator>,
    calls: Calls,
    local_addr: SocketAddr,
    cancel: CancellationToken,
}

impl SipServer {
    /// Bind SIP transports (UDP, optional TCP).
    pub async fn bind(cfg: Arc<Config>) -> Result<Self> {
        let cancel = CancellationToken::new();
        let transport_layer = TransportLayer::new(cancel.clone());
        let listen = cfg.sip.listen;
        let external = match cfg.sip.external_ip {
            Some(ip) => {
                if listen.port() == 0 {
                    bail!("sip.listen port must be fixed when sip.external_ip is set");
                }
                Some(SocketAddr::new(ip, listen.port()))
            }
            None => None,
        };
        let udp = UdpConnection::create_connection(listen, external, Some(cancel.child_token()))
            .await
            .with_context(|| format!("bind SIP UDP {listen}"))?;
        let bound_port = udp
            .get_addr()
            .get_socketaddr()
            .map(|a| a.port())
            .unwrap_or(listen.port());
        let local_addr = SocketAddr::new(listen.ip(), bound_port);
        transport_layer.add_transport(udp.into());
        if cfg.sip.tcp {
            let tcp_addr = SocketAddr::new(listen.ip(), bound_port);
            let tcp = TcpListenerConnection::new(tcp_addr, external)
                .await
                .with_context(|| format!("bind SIP TCP {tcp_addr}"))?;
            transport_layer.add_transport(tcp.into());
        }
        let endpoint = EndpointBuilder::new()
            .with_cancel_token(cancel.clone())
            .with_transport_layer(transport_layer)
            .with_user_agent(&cfg.sip.user_agent)
            .build();
        let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));
        let ports = Arc::new(PortAllocator::new(
            cfg.rtp.bind_ip,
            cfg.rtp.port_min,
            cfg.rtp.port_max,
        ));
        info!(%local_addr, tcp = cfg.sip.tcp, external_ip = ?cfg.sip.external_ip, "SIP listening");
        if cfg.sip.allow.is_empty() {
            warn!(
                "sip.allow is empty: ANY source can place calls (and spend vendor quota); \
                 set sip.allow or firewall the SIP port"
            );
        } else {
            let allow: Vec<String> = cfg.sip.allow.iter().map(|n| n.to_string()).collect();
            info!(allow = %allow.join(","), "SIP source allowlist enabled");
        }
        Ok(Self {
            cfg,
            endpoint,
            dialog_layer,
            ports,
            calls: Arc::new(Mutex::new(HashMap::new())),
            local_addr,
            cancel,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Run until `shutdown` fires; active calls get BYE before returning.
    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        let incoming = self.endpoint.incoming_transactions()?;
        let (state_tx, state_rx) = self.dialog_layer.new_dialog_state_channel();

        let ep = self.endpoint.inner.clone();
        let serve = tokio::spawn(async move {
            if let Err(e) = ep.serve().await {
                error!(error = %e, "SIP endpoint stopped");
            }
        });
        let state_task = tokio::spawn(state_loop(
            state_rx,
            self.dialog_layer.clone(),
            self.calls.clone(),
        ));
        let ctx = Arc::new(InviteCtx {
            cfg: self.cfg.clone(),
            dialog_layer: self.dialog_layer.clone(),
            ports: self.ports.clone(),
            calls: self.calls.clone(),
            state_tx,
            sip_ip: self.local_addr.ip(),
        });
        let incoming_task = tokio::spawn(incoming_loop(incoming, ctx));

        shutdown.cancelled().await;
        info!("shutting down: hanging up active calls");
        {
            let calls = self.calls.lock().expect("calls lock");
            for h in calls.values() {
                h.remote_reason
                    .lock()
                    .expect("reason lock")
                    .get_or_insert("shutdown");
                h.cancel.cancel();
            }
        }
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while Instant::now() < deadline && !self.calls.lock().expect("calls lock").is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        incoming_task.abort();
        state_task.abort();
        self.cancel.cancel();
        self.endpoint.shutdown();
        serve.abort();
        Ok(())
    }
}

struct InviteCtx {
    cfg: Arc<Config>,
    dialog_layer: Arc<DialogLayer>,
    ports: Arc<PortAllocator>,
    calls: Calls,
    state_tx: rsipstack::dialog::dialog::DialogStateSender,
    sip_ip: IpAddr,
}

async fn incoming_loop(mut incoming: TransactionReceiver, ctx: Arc<InviteCtx>) {
    while let Some(mut tx) = incoming.recv().await {
        let method = tx.original.method;
        let in_dialog = tx
            .original
            .to_header()
            .ok()
            .and_then(|t| t.tag().ok().flatten())
            .is_some();
        if !in_dialog && method != Method::Ack && !ctx.cfg.sip.allow.is_empty() {
            let src = source_ip(&tx.original);
            if !src.is_some_and(|ip| crate::acl::allowed(&ctx.cfg.sip.allow, ip)) {
                warn!(source = ?src, %method, "request from source not in sip.allow: 403");
                tokio::spawn(async move {
                    tx.reply(StatusCode::Forbidden).await.ok();
                });
                continue;
            }
        }
        if method == Method::Invite && !in_dialog {
            on_invite(&ctx, tx).await;
            continue;
        }
        match ctx.dialog_layer.match_dialog(&tx) {
            Some(Dialog::Invite(mut d)) => {
                tokio::spawn(async move {
                    if let Err(e) = d.handle(&mut tx).await {
                        debug!(error = %e, "in-dialog request handling failed");
                    }
                });
            }
            _ => {
                let status = match method {
                    Method::Ack => continue,
                    Method::Options => StatusCode::OK,
                    Method::Bye
                    | Method::Cancel
                    | Method::Invite
                    | Method::Update
                    | Method::Info => StatusCode::CallTransactionDoesNotExist,
                    _ => StatusCode::MethodNotAllowed,
                };
                tokio::spawn(async move {
                    tx.reply(status).await.ok();
                });
            }
        }
    }
}

/// Packet source IP of a received request. The transport layer stamps the real
/// source into the top Via (`received=` when it differs from sent-by, which
/// it then overwrites), so this is not spoofable via headers.
fn source_ip(req: &rsipstack::sip::Request) -> Option<IpAddr> {
    let via = req.via_header().ok()?;
    let (_, hp) = rsipstack::transport::SipConnection::parse_target_from_via(via).ok()?;
    match hp.host {
        rsipstack::sip::Host::IpAddr(ip) => Some(ip),
        _ => None,
    }
}

fn user_of(uri: &rsipstack::sip::Uri) -> String {
    uri.user().unwrap_or_default().to_string()
}

async fn on_invite(ctx: &Arc<InviteCtx>, mut tx: Transaction) {
    let called = user_of(&tx.original.uri);
    let from = tx
        .original
        .from_header()
        .ok()
        .and_then(|h| h.uri().ok())
        .map(|u| user_of(&u))
        .unwrap_or_default();
    let call_id = tx
        .original
        .call_id_header()
        .map(|c| c.value().to_string())
        .unwrap_or_default();

    let Some(agent) = ctx.cfg.match_agent(&called).cloned() else {
        info!(%call_id, %from, to = %called, "no agent for called number → 404");
        tokio::spawn(async move {
            tx.reply(StatusCode::NotFound).await.ok();
        });
        return;
    };
    let body = String::from_utf8_lossy(&tx.original.body).to_string();
    let offer = match sdp::parse_offer(&body) {
        Ok(o) => o,
        Err(e) => {
            info!(%call_id, error = %e, "unacceptable SDP offer → 488");
            tokio::spawn(async move {
                tx.reply(StatusCode::NotAcceptableHere).await.ok();
            });
            return;
        }
    };

    let dialog =
        match ctx
            .dialog_layer
            .get_or_create_server_invite(&tx, ctx.state_tx.clone(), None, None)
        {
            Ok(d) => d,
            Err(e) => {
                warn!(%call_id, error = %e, "create dialog failed");
                tokio::spawn(async move {
                    tx.reply(StatusCode::ServerInternalError).await.ok();
                });
                return;
            }
        };
    let dialog_key = dialog.id().to_string();
    let handle = CallHandle {
        cancel: CancellationToken::new(),
        remote_reason: Arc::new(Mutex::new(None)),
        answer_sdp: Arc::new(Mutex::new(None)),
    };
    let cancel = handle.cancel.clone();
    let remote_reason = handle.remote_reason.clone();
    let answer_slot = handle.answer_sdp.clone();
    ctx.calls
        .lock()
        .expect("calls lock")
        .insert(dialog_key.clone(), handle);

    // Drive the INVITE server transaction (100 Trying, CANCEL, ACK).
    {
        let mut d = dialog.clone();
        tokio::spawn(async move {
            if let Err(e) = d.handle(&mut tx).await {
                debug!(error = %e, "invite transaction ended with error");
            }
        });
    }

    info!(%call_id, %from, to = %called, agent = %agent.name, provider = agent.provider.as_str(), codec = offer.codec.name(), "incoming call");
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let info = CallInfo {
            call_id,
            from,
            to: called,
            agent: Arc::new(agent),
        };
        run_call(
            &ctx,
            dialog.clone(),
            info,
            offer,
            cancel,
            remote_reason,
            answer_slot,
        )
        .await;
        ctx.calls.lock().expect("calls lock").remove(&dialog_key);
        ctx.dialog_layer.remove_dialog(&dialog.id());
    });
}

struct CallInfo {
    call_id: String,
    from: String,
    to: String,
    agent: Arc<crate::config::AgentConfig>,
}

fn reject(dialog: &InviteDialog, code: StatusCode) {
    if let Err(e) = dialog.reject(Some(code), None) {
        debug!(error = %e, "reject failed");
    }
}

/// Pick the IP we advertise in SDP.
fn advertised_ip(ctx: &InviteCtx, remote: Option<SocketAddr>) -> IpAddr {
    if let Some(ip) = ctx.cfg.sip.external_ip {
        return ip;
    }
    if !ctx.cfg.rtp.bind_ip.is_unspecified() {
        return ctx.cfg.rtp.bind_ip;
    }
    if let Some(r) = remote
        && let Ok(s) = std::net::UdpSocket::bind(SocketAddr::new(
            if r.is_ipv6() {
                IpAddr::from([0u16; 8])
            } else {
                IpAddr::from([0u8; 4])
            },
            0,
        ))
        && s.connect(r).is_ok()
        && let Ok(local) = s.local_addr()
    {
        return local.ip();
    }
    if !ctx.sip_ip.is_unspecified() {
        return ctx.sip_ip;
    }
    IpAddr::from([127, 0, 0, 1])
}

async fn run_call(
    ctx: &InviteCtx,
    dialog: InviteDialog,
    info: CallInfo,
    offer: sdp::Offer,
    cancel: CancellationToken,
    remote_reason: Arc<Mutex<Option<&'static str>>>,
    answer_slot: Arc<Mutex<Option<String>>>,
) {
    let call_id = info.call_id.clone();
    let socket = match ctx.ports.bind().await {
        Ok(s) => s,
        Err(e) => {
            warn!(%call_id, error = %e, "no RTP port → 503");
            reject(&dialog, StatusCode::ServiceUnavailable);
            return;
        }
    };
    let session = tokio::select! {
        _ = cancel.cancelled() => {
            info!(%call_id, "caller cancelled before answer");
            return;
        }
        r = call::prepare_agent(&info.agent) => match r {
            Ok(s) => s,
            Err(e) => {
                warn!(%call_id, agent = %info.agent.name, error = %e, "agent unavailable → 503");
                reject(&dialog, StatusCode::ServiceUnavailable);
                return;
            }
        }
    };
    if cancel.is_cancelled() {
        info!(%call_id, "caller cancelled before answer");
        return;
    }

    let local_port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
    let ip = advertised_ip(ctx, offer.remote_rtp);
    let answer = sdp::build_answer(ip, local_port, offer.codec, rand::random::<u32>() as u64);
    *answer_slot.lock().expect("answer lock") = Some(answer.clone());
    if let Err(e) = dialog.accept(
        Some(vec![Header::ContentType("application/sdp".into())]),
        Some(answer.into_bytes()),
    ) {
        warn!(%call_id, error = %e, "send 200 OK failed");
        return;
    }
    let started = chrono::Local::now();
    let t0 = Instant::now();
    info!(%call_id, rtp_port = local_port, %ip, remote_rtp = ?offer.remote_rtp, "call answered");

    let media_cancel = CancellationToken::new();
    let (rtp_session, media) =
        match rtp::start(socket, offer.codec, offer.remote_rtp, media_cancel.clone()) {
            Ok(v) => v,
            Err(e) => {
                warn!(%call_id, error = %e, "rtp start failed");
                dialog.bye().await.ok();
                return;
            }
        };

    let rtp_timeout =
        (ctx.cfg.rtp.timeout_secs > 0).then(|| Duration::from_secs(ctx.cfg.rtp.timeout_secs));
    let outcome = call::run_media(
        MediaParams {
            call_id: call_id.clone(),
            agent: info.agent.clone(),
            rtp_timeout,
        },
        session,
        media,
        &rtp_session,
        cancel.clone(),
    )
    .await;

    let remote = *remote_reason.lock().expect("reason lock");
    let (reason, we_hang_up) = match &outcome.end {
        MediaEnd::Remote => (remote.unwrap_or("caller_bye"), remote == Some("shutdown")),
        MediaEnd::RtpTimeout => ("rtp_timeout", true),
        MediaEnd::Agent(b) => (b.reason(), true),
    };
    if we_hang_up && let Err(e) = dialog.bye().await {
        debug!(%call_id, error = %e, "BYE failed");
    }
    media_cancel.cancel();
    let duration = t0.elapsed();
    info!(
        %call_id,
        reason,
        duration_secs = duration.as_secs_f64(),
        rtp_rx = rtp_session.rx_packets.load(std::sync::atomic::Ordering::Relaxed),
        rtp_tx = rtp_session.tx_packets.load(std::sync::atomic::Ordering::Relaxed),
        "call ended"
    );

    if let Some(url) = info.agent.call_end_webhook.clone() {
        let report = CallEndReport {
            call_id,
            agent: info.agent.name.clone(),
            from: info.from,
            to: info.to,
            start_time: started.to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
            end_time: webhook::now_rfc3339(),
            duration_secs: (duration.as_millis() as f64) / 1000.0,
            hangup_reason: reason.to_string(),
            transcripts: outcome.transcripts,
        };
        let timeout = Duration::from_millis(info.agent.timeout_ms);
        webhook::send_call_end(&url, &report, timeout).await;
    }
}

async fn state_loop(mut rx: DialogStateReceiver, layer: Arc<DialogLayer>, calls: Calls) {
    while let Some(state) = rx.recv().await {
        match state {
            DialogState::Terminated(id, reason) => {
                let why: &'static str = match reason {
                    TerminatedReason::UacBye => "caller_bye",
                    TerminatedReason::UacCancel => "caller_cancel",
                    TerminatedReason::Timeout => "sip_timeout",
                    TerminatedReason::UasBye => "agent_bye",
                    _ => "dialog_terminated",
                };
                debug!(%id, ?reason, "dialog terminated");
                if let Some(h) = calls.lock().expect("calls lock").get(&id.to_string()) {
                    h.remote_reason
                        .lock()
                        .expect("reason lock")
                        .get_or_insert(why);
                    h.cancel.cancel();
                }
                layer.remove_dialog(&id);
            }
            DialogState::Updated(id, req, handle) => {
                // re-INVITE / UPDATE (session refresh, hold): answer with our SDP.
                let sdp = answer_for(&calls, &id);
                tokio::spawn(async move {
                    let with_sdp = !req.body.is_empty() || req.method == Method::Invite;
                    match (with_sdp, sdp) {
                        (true, Some(sdp)) => handle
                            .respond(
                                StatusCode::OK,
                                Some(vec![Header::ContentType("application/sdp".into())]),
                                Some(sdp.into_bytes()),
                            )
                            .await
                            .ok(),
                        _ => handle.reply(StatusCode::OK).await.ok(),
                    };
                });
            }
            DialogState::Options(_, _, h)
            | DialogState::Info(_, _, h)
            | DialogState::Message(_, _, h)
            | DialogState::Notify(_, _, h) => {
                tokio::spawn(async move {
                    h.reply(StatusCode::OK).await.ok();
                });
            }
            DialogState::Refer(_, _, h) | DialogState::Publish(_, _, h) => {
                tokio::spawn(async move {
                    h.reply(StatusCode::NotImplemented).await.ok();
                });
            }
            other => debug!(state = %other, "dialog state"),
        }
    }
}

fn answer_for(calls: &Calls, id: &DialogId) -> Option<String> {
    calls
        .lock()
        .expect("calls lock")
        .get(&id.to_string())
        .and_then(|h| h.answer_sdp.lock().expect("answer lock").clone())
}
