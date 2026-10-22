//! Minimal test UAC (rsipstack) used by `examples/uac.rs` and the
//! integration tests: INVITE with a G.711 offer, stream a test tone over RTP,
//! count what comes back, then BYE (or observe the remote BYE).

use crate::audio::g711::G711;
use crate::audio::{FRAME_MS, SIP_FRAME_SAMPLES};
use crate::rtp::RtpPacket;
use crate::sdp;
use anyhow::{Context, Result, anyhow};
use rsipstack::EndpointBuilder;
use rsipstack::dialog::dialog::{Dialog, DialogState};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::transport::TransportLayer;
use rsipstack::transport::udp::UdpConnection;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct UacOptions {
    /// sip2agent SIP address.
    pub server: SocketAddr,
    /// Called number (request-URI user).
    pub to: String,
    pub from: String,
    /// Codecs to offer, in preference order.
    pub codecs: Vec<G711>,
    /// How long to stream audio before hanging up.
    pub duration: Duration,
    /// Tone frequency to send (0 = silence).
    pub tone_hz: f32,
    /// Local IP to bind SIP/RTP on.
    pub local_ip: IpAddr,
    /// Send RTP at all (false = test the RTP inactivity timeout).
    pub send_rtp: bool,
    /// Override the generated SDP offer (e.g. codec negotiation tests).
    pub raw_offer: Option<String>,
}

impl Default for UacOptions {
    fn default() -> Self {
        Self {
            server: "127.0.0.1:5060".parse().expect("static"),
            to: "9000".into(),
            from: "1001".into(),
            codecs: vec![G711::Alaw, G711::Ulaw],
            duration: Duration::from_secs(3),
            tone_hz: 440.0,
            local_ip: "127.0.0.1".parse().expect("static"),
            send_rtp: true,
            raw_offer: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct UacReport {
    /// Final INVITE status code.
    pub status: u16,
    pub codec: Option<G711>,
    pub rtp_sent: u64,
    pub rtp_received: u64,
    /// Received packets whose decoded audio is not (near) silence.
    pub rtp_received_nonsilent: u64,
    /// The far end hung up (BYE received) before we did.
    pub remote_hangup: bool,
    /// Whether our BYE got sent (we hung up).
    pub local_hangup: bool,
}

pub async fn run(opts: UacOptions) -> Result<UacReport> {
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();
    let tl = TransportLayer::new(cancel.clone());
    let conn = UdpConnection::create_connection(
        SocketAddr::new(opts.local_ip, 0),
        None,
        Some(cancel.child_token()),
    )
    .await?;
    let sip_local = conn.get_addr().get_socketaddr()?;
    tl.add_transport(conn.into());
    let endpoint = EndpointBuilder::new()
        .with_cancel_token(cancel.clone())
        .with_transport_layer(tl)
        .with_user_agent("sip2agent-test-uac")
        .build();
    let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));
    let (state_tx, mut state_rx) = dialog_layer.new_dialog_state_channel();
    let mut incoming = endpoint.incoming_transactions()?;
    {
        let ep = endpoint.inner.clone();
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = ep.serve() => {}
                _ = c.cancelled() => {}
            }
        });
    }
    {
        let dl = dialog_layer.clone();
        tokio::spawn(async move {
            while let Some(mut tx) = incoming.recv().await {
                if let Some(Dialog::Invite(mut d)) = dl.match_dialog(&tx) {
                    tokio::spawn(async move {
                        d.handle(&mut tx).await.ok();
                    });
                } else {
                    tx.reply(rsipstack::sip::StatusCode::CallTransactionDoesNotExist)
                        .await
                        .ok();
                }
            }
        });
    }

    let rtp = UdpSocket::bind(SocketAddr::new(opts.local_ip, 0)).await?;
    let rtp_port = rtp.local_addr()?.port();
    let offer = opts
        .raw_offer
        .clone()
        .unwrap_or_else(|| sdp::build_offer(opts.local_ip, rtp_port, &opts.codecs));

    let uri = |user: &str, addr: SocketAddr| -> Result<rsipstack::sip::Uri> {
        rsipstack::sip::Uri::try_from(format!("sip:{user}@{addr}").as_str())
            .map_err(|e| anyhow!("uri: {e}"))
    };
    let invite = InviteOption {
        caller: uri(&opts.from, sip_local)?,
        callee: uri(&opts.to, opts.server)?,
        contact: uri(&opts.from, sip_local)?,
        content_type: Some("application/sdp".into()),
        offer: Some(offer.into_bytes()),
        ..Default::default()
    };
    let (dialog, resp) = dialog_layer
        .do_invite(invite, state_tx)
        .await
        .context("INVITE failed")?;
    let mut report = UacReport::default();
    let Some(resp) = resp else {
        return Err(anyhow!("no final response"));
    };
    report.status = u16::from(resp.status_code.clone());
    if report.status != 200 {
        return Ok(report);
    }
    let answer = sdp::parse_offer(&String::from_utf8_lossy(&resp.body))
        .map_err(|e| anyhow!("bad answer SDP: {e}"))?;
    report.codec = Some(answer.codec);
    let remote = answer
        .remote_rtp
        .ok_or_else(|| anyhow!("answer without RTP address"))?;
    let codec = answer.codec;

    let mut ticker = tokio::time::interval(Duration::from_millis(FRAME_MS));
    let deadline = tokio::time::Instant::now() + opts.duration;
    let mut seq: u16 = 1;
    let mut ts: u32 = 0;
    let mut phase: f32 = 0.0;
    let step = 2.0 * std::f32::consts::PI * opts.tone_hz / 8000.0;
    let mut buf = [0u8; 2048];
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            st = state_rx.recv() => {
                if let Some(DialogState::Terminated(_, _)) = st {
                    report.remote_hangup = true;
                    break;
                }
            }
            _ = ticker.tick(), if opts.send_rtp => {
                let frame: Vec<i16> = (0..SIP_FRAME_SAMPLES)
                    .map(|_| {
                        let s = if opts.tone_hz > 0.0 { (phase.sin() * 8000.0) as i16 } else { 0 };
                        phase += step;
                        s
                    })
                    .collect();
                let pkt = RtpPacket {
                    marker: seq == 1,
                    payload_type: codec.payload_type(),
                    seq,
                    timestamp: ts,
                    ssrc: 0x5151_5151,
                    payload: codec.encode(&frame),
                };
                rtp.send_to(&pkt.build(), remote).await?;
                report.rtp_sent += 1;
                seq = seq.wrapping_add(1);
                ts = ts.wrapping_add(SIP_FRAME_SAMPLES as u32);
            }
            r = rtp.recv_from(&mut buf) => {
                if let Ok((n, _)) = r
                    && let Some(p) = RtpPacket::parse(&buf[..n])
                {
                    report.rtp_received += 1;
                    let pcm = codec.decode(&p.payload);
                    if pcm.iter().any(|s| s.unsigned_abs() > 500) {
                        report.rtp_received_nonsilent += 1;
                    }
                }
            }
        }
    }
    if !report.remote_hangup {
        dialog.bye().await.ok();
        report.local_hangup = true;
        // Give the BYE transaction a moment to complete.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(report)
}
