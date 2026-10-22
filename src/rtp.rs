//! Minimal RTP over tokio `UdpSocket` for one G.711 stream.
//!
//! - RFC 3550 fixed header parse/build (CSRC/extension/padding tolerated on receive)
//! - random SSRC / initial seq / timestamp, timestamp += 160 per 20 ms frame
//! - symmetric RTP with a guarded latch (see [`Latch`]): we send to the
//!   latched source, before that to the SDP `c=`/`m=` address; audio from
//!   any other source is dropped
//! - incoming audio is re-chunked into 20 ms / 160-sample frames
//! - no RTCP, no DTMF (telephone-event packets are ignored)

use crate::audio::SIP_FRAME_SAMPLES;
use crate::audio::g711::G711;
use anyhow::{Result, bail};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// The latched source must be silent at least this long before another
/// source may take over.
const LATCH_SILENCE: Duration = Duration::from_secs(1);
/// A new source needs this many consecutive valid packets to take over.
const LATCH_MIN_PACKETS: u32 = 5;
/// Max gap between a candidate's packets for them to count as consecutive.
const LATCH_MAX_GAP: Duration = Duration::from_millis(200);

/// Symmetric-RTP source latch.
///
/// Only packets with the negotiated payload type are fed in. Rules:
/// - a packet from the SDP address always (re)latches to it — the SDP source
///   wins over an earlier unknown source;
/// - otherwise the first valid source is latched;
/// - once latched, other sources are dropped, unless the latched source has
///   been silent for [`LATCH_SILENCE`] and the new source sent
///   [`LATCH_MIN_PACKETS`] consecutive packets (sporadic packets never move it).
#[derive(Debug)]
pub struct Latch {
    sdp: Option<SocketAddr>,
    current: Option<SocketAddr>,
    last_rx: Option<Instant>,
    /// (source, consecutive count, last seen)
    candidate: Option<(SocketAddr, u32, Instant)>,
}

impl Latch {
    pub fn new(sdp: Option<SocketAddr>) -> Self {
        Self {
            sdp,
            current: None,
            last_rx: None,
            candidate: None,
        }
    }

    /// Where to send downlink RTP.
    pub fn destination(&self) -> Option<SocketAddr> {
        self.current.or(self.sdp)
    }

    /// New SDP address from a re-INVITE / UPDATE: forget the latch.
    pub fn reset(&mut self, sdp: Option<SocketAddr>) {
        *self = Self::new(sdp);
    }

    /// Feed one valid packet from `from`; returns whether its audio is used.
    pub fn accept(&mut self, from: SocketAddr, now: Instant) -> bool {
        if self.current == Some(from) {
            self.last_rx = Some(now);
            self.candidate = None;
            return true;
        }
        let silent = self
            .last_rx
            .is_none_or(|t| now.saturating_duration_since(t) >= LATCH_SILENCE);
        let take = if self.current.is_none() || self.sdp == Some(from) {
            true
        } else {
            let count = match self.candidate {
                Some((addr, n, last))
                    if addr == from && now.saturating_duration_since(last) <= LATCH_MAX_GAP =>
                {
                    n + 1
                }
                _ => 1,
            };
            self.candidate = Some((from, count, now));
            silent && count >= LATCH_MIN_PACKETS
        };
        if !take {
            return false;
        }
        info!(%from, previous = ?self.current, "rtp: latched remote address (symmetric RTP)");
        self.current = Some(from);
        self.last_rx = Some(now);
        self.candidate = None;
        true
    }
}

/// Handle to adjust a running RTP session from the SIP side.
#[derive(Clone)]
pub struct RtpControl {
    latch: Arc<Mutex<Latch>>,
}

impl RtpControl {
    /// Apply the remote address from a re-INVITE / UPDATE offer. A changed
    /// address resets the latch; the same address is a no-op.
    pub fn update_sdp_remote(&self, sdp: SocketAddr) {
        let mut l = self.latch.lock().expect("latch lock");
        if l.sdp != Some(sdp) {
            info!(%sdp, "rtp: remote SDP address changed, latch reset");
            l.reset(Some(sdp));
        }
    }
}

pub const RTP_VERSION: u8 = 2;
const MAX_PACKET: usize = 1500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpPacket {
    pub marker: bool,
    pub payload_type: u8,
    pub seq: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload: Vec<u8>,
}

impl RtpPacket {
    pub fn parse(buf: &[u8]) -> Option<RtpPacket> {
        if buf.len() < 12 {
            return None;
        }
        let b0 = buf[0];
        if b0 >> 6 != RTP_VERSION {
            return None;
        }
        let padding = b0 & 0x20 != 0;
        let extension = b0 & 0x10 != 0;
        let cc = (b0 & 0x0F) as usize;
        let b1 = buf[1];
        let marker = b1 & 0x80 != 0;
        let payload_type = b1 & 0x7F;
        let seq = u16::from_be_bytes([buf[2], buf[3]]);
        let timestamp = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let mut off = 12 + cc * 4;
        if buf.len() < off {
            return None;
        }
        if extension {
            if buf.len() < off + 4 {
                return None;
            }
            let words = u16::from_be_bytes([buf[off + 2], buf[off + 3]]) as usize;
            off += 4 + words * 4;
            if buf.len() < off {
                return None;
            }
        }
        let mut end = buf.len();
        if padding {
            let pad = *buf.last()? as usize;
            if pad == 0 || end < off + pad {
                return None;
            }
            end -= pad;
        }
        Some(RtpPacket {
            marker,
            payload_type,
            seq,
            timestamp,
            ssrc,
            payload: buf[off..end].to_vec(),
        })
    }

    pub fn build(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + self.payload.len());
        out.push(RTP_VERSION << 6);
        out.push(((self.marker as u8) << 7) | (self.payload_type & 0x7F));
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }
}

/// Allocates RTP ports (even numbers) from the configured range.
pub struct PortAllocator {
    bind_ip: IpAddr,
    min: u16,
    max: u16,
    cursor: AtomicU16,
}

impl PortAllocator {
    pub fn new(bind_ip: IpAddr, min: u16, max: u16) -> Self {
        let min = if min % 2 == 1 { min + 1 } else { min };
        Self {
            bind_ip,
            min,
            max,
            cursor: AtomicU16::new(min),
        }
    }

    /// Bind a UDP socket on a free even port in range.
    pub async fn bind(&self) -> Result<UdpSocket> {
        if self.min > self.max {
            bail!("empty RTP port range");
        }
        let slots = ((self.max - self.min) / 2 + 1) as u32;
        for _ in 0..slots {
            let port = self
                .cursor
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |p| {
                    let next = p.saturating_add(2);
                    Some(if next > self.max { self.min } else { next })
                })
                .unwrap_or(self.min);
            if let Ok(sock) = UdpSocket::bind(SocketAddr::new(self.bind_ip, port)).await {
                return Ok(sock);
            }
        }
        bail!("no free RTP port in {}-{}", self.min, self.max)
    }
}

/// Audio interface between the RTP leg and an agent: 20 ms / 160-sample
/// frames of 8 kHz PCM16 in both directions.
pub struct MediaPort {
    /// Caller audio (decoded, re-chunked to 160 samples).
    pub uplink: mpsc::Receiver<Vec<i16>>,
    /// Audio to play to the caller; each frame is sent as one RTP packet
    /// immediately, so the producer is responsible for 20 ms pacing.
    pub downlink: mpsc::Sender<Vec<i16>>,
}

/// Live RTP session state shared with the call.
pub struct RtpSession {
    pub local_port: u16,
    last_rx_ms: Arc<AtomicU64>,
    started: Instant,
    pub rx_packets: Arc<AtomicU64>,
    pub tx_packets: Arc<AtomicU64>,
    latch: Arc<Mutex<Latch>>,
}

impl RtpSession {
    pub fn control(&self) -> RtpControl {
        RtpControl {
            latch: self.latch.clone(),
        }
    }

    /// Time since the last valid RTP packet (or since start if none yet).
    pub fn idle_for(&self) -> Duration {
        let last = self.last_rx_ms.load(Ordering::Relaxed);
        let now = self.started.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(last))
    }
}

/// Start RX/TX tasks on `socket`. Tasks stop when `cancel` fires or the
/// downlink sender is dropped.
pub fn start(
    socket: UdpSocket,
    codec: G711,
    sdp_remote: Option<SocketAddr>,
    cancel: CancellationToken,
) -> Result<(RtpSession, MediaPort)> {
    let local_port = socket.local_addr()?.port();
    let socket = Arc::new(socket);
    let latch = Arc::new(Mutex::new(Latch::new(sdp_remote)));
    let started = Instant::now();
    let last_rx_ms = Arc::new(AtomicU64::new(0));
    let rx_packets = Arc::new(AtomicU64::new(0));
    let tx_packets = Arc::new(AtomicU64::new(0));

    let (up_tx, up_rx) = mpsc::channel::<Vec<i16>>(50);
    let (down_tx, mut down_rx) = mpsc::channel::<Vec<i16>>(50);

    // RX
    {
        let socket = socket.clone();
        let latch = latch.clone();
        let cancel = cancel.clone();
        let last_rx_ms = last_rx_ms.clone();
        let rx_packets = rx_packets.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_PACKET];
            let mut pending: Vec<i16> = Vec::with_capacity(SIP_FRAME_SAMPLES * 2);
            let pt = codec.payload_type();
            loop {
                let (n, from) = tokio::select! {
                    _ = cancel.cancelled() => break,
                    r = socket.recv_from(&mut buf) => match r {
                        Ok(v) => v,
                        Err(e) => {
                            // ICMP port unreachable etc. on some platforms — keep going.
                            debug!(error = %e, "rtp recv error");
                            continue;
                        }
                    },
                };
                let Some(pkt) = RtpPacket::parse(&buf[..n]) else {
                    continue;
                };
                if pkt.payload_type != pt {
                    // telephone-event, comfort noise, anything else: ignored.
                    continue;
                }
                if !latch
                    .lock()
                    .expect("latch lock")
                    .accept(from, Instant::now())
                {
                    // Not the latched source: no audio, no RTP-timeout refresh.
                    continue;
                }
                rx_packets.fetch_add(1, Ordering::Relaxed);
                last_rx_ms.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                pending.extend(codec.decode(&pkt.payload));
                while pending.len() >= SIP_FRAME_SAMPLES {
                    let frame: Vec<i16> = pending.drain(..SIP_FRAME_SAMPLES).collect();
                    if up_tx.try_send(frame).is_err() && up_tx.is_closed() {
                        return;
                    }
                }
            }
        });
    }

    // TX
    {
        let socket = socket.clone();
        let tx_packets = tx_packets.clone();
        let latch = latch.clone();
        tokio::spawn(async move {
            let ssrc: u32 = rand::random();
            let mut seq: u16 = rand::random();
            let mut ts: u32 = rand::random();
            let mut first = true;
            loop {
                let frame = tokio::select! {
                    _ = cancel.cancelled() => break,
                    f = down_rx.recv() => match f {
                        Some(f) => f,
                        None => break,
                    },
                };
                let dest = latch.lock().expect("latch lock").destination();
                let Some(dest) = dest else {
                    // Nowhere to send yet (no SDP address and nothing received).
                    ts = ts.wrapping_add(frame.len() as u32);
                    continue;
                };
                let pkt = RtpPacket {
                    marker: first,
                    payload_type: codec.payload_type(),
                    seq,
                    timestamp: ts,
                    ssrc,
                    payload: codec.encode(&frame),
                };
                first = false;
                if let Err(e) = socket.send_to(&pkt.build(), dest).await {
                    warn!(error = %e, %dest, "rtp send failed");
                } else {
                    tx_packets.fetch_add(1, Ordering::Relaxed);
                }
                seq = seq.wrapping_add(1);
                ts = ts.wrapping_add(frame.len() as u32);
            }
        });
    }

    Ok((
        RtpSession {
            local_port,
            last_rx_ms,
            started,
            rx_packets,
            tx_packets,
            latch,
        },
        MediaPort {
            uplink: up_rx,
            downlink: down_tx,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_parse_roundtrip() {
        let p = RtpPacket {
            marker: true,
            payload_type: 8,
            seq: 65535,
            timestamp: 0xDEADBEEF,
            ssrc: 0x01020304,
            payload: vec![0xD5; 160],
        };
        let bytes = p.build();
        assert_eq!(bytes.len(), 172);
        assert_eq!(bytes[0], 0x80);
        assert_eq!(bytes[1], 0x88);
        assert_eq!(RtpPacket::parse(&bytes).unwrap(), p);
    }

    #[test]
    fn parse_with_csrc_extension_padding() {
        let mut b = vec![
            0x80 | 0x20 | 0x10 | 0x01,
            0x00,
            0,
            1,
            0,
            0,
            0,
            160,
            0,
            0,
            0,
            9,
        ];
        b.extend_from_slice(&[0, 0, 0, 7]); // CSRC
        b.extend_from_slice(&[0xBE, 0xDE, 0, 1, 1, 2, 3, 4]); // ext, 1 word
        b.extend_from_slice(&[0xAA, 0xBB]); // payload
        b.extend_from_slice(&[0, 3]); // padding (2 bytes, last = count)
        // padding count must cover itself: fix to 2
        let len = b.len();
        b[len - 1] = 2;
        let p = RtpPacket::parse(&b).unwrap();
        assert_eq!(p.payload, vec![0xAA, 0xBB]);
        assert_eq!(p.payload_type, 0);
        assert_eq!(p.seq, 1);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(RtpPacket::parse(&[0x80, 0, 0]).is_none());
        assert!(RtpPacket::parse(&[0x40; 20]).is_none()); // version 1
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// Feed `n` packets from `from` every 20 ms starting at `t`; returns
    /// per-packet results and the time after the last one.
    fn feed(l: &mut Latch, from: SocketAddr, t: Instant, n: u32) -> (Vec<bool>, Instant) {
        let mut out = Vec::new();
        let mut now = t;
        for _ in 0..n {
            out.push(l.accept(from, now));
            now += Duration::from_millis(20);
        }
        (out, now)
    }

    #[test]
    fn latch_first_source_when_no_sdp_match() {
        let t = Instant::now();
        let mut l = Latch::new(Some(addr("10.0.0.1:4000")));
        assert_eq!(l.destination(), Some(addr("10.0.0.1:4000")));
        assert!(l.accept(addr("192.0.2.1:5000"), t));
        assert_eq!(l.destination(), Some(addr("192.0.2.1:5000")));
    }

    #[test]
    fn latch_sdp_source_wins_over_earlier_unknown_source() {
        let t = Instant::now();
        let sdp = addr("10.0.0.1:4000");
        let mut l = Latch::new(Some(sdp));
        assert!(l.accept(addr("192.0.2.1:5000"), t));
        // SDP source shows up while the unknown one is still active.
        assert!(l.accept(sdp, t + Duration::from_millis(20)));
        assert_eq!(l.destination(), Some(sdp));
        assert!(!l.accept(addr("192.0.2.1:5000"), t + Duration::from_millis(40)));
    }

    #[test]
    fn latch_drops_other_sources_while_current_active() {
        let t = Instant::now();
        let a = addr("192.0.2.1:5000");
        let b = addr("198.51.100.9:6000");
        let mut l = Latch::new(None);
        let (r, mut now) = feed(&mut l, a, t, 3);
        assert!(r.iter().all(|&x| x));
        // b sends a steady stream while a keeps talking: never moves.
        for _ in 0..20 {
            assert!(!l.accept(b, now));
            assert!(l.accept(a, now + Duration::from_millis(10)));
            now += Duration::from_millis(20);
        }
        assert_eq!(l.destination(), Some(a));
    }

    #[test]
    fn latch_moves_only_after_silence_and_steady_new_source() {
        let t = Instant::now();
        let a = addr("192.0.2.1:5000");
        let b = addr("198.51.100.9:6000");
        let mut l = Latch::new(None);
        let (_, now) = feed(&mut l, a, t, 3);
        // a goes silent; b starts too early (silence < 1 s) → still dropped.
        let (r, _) = feed(&mut l, b, now + Duration::from_millis(200), 5);
        assert!(r.iter().all(|&x| !x));
        // After ≥1 s of silence from a, b needs 5 consecutive packets.
        let later = now + LATCH_SILENCE + Duration::from_millis(100);
        let (r, _) = feed(&mut l, b, later, 5);
        assert_eq!(r, vec![false, false, false, false, true]);
        assert_eq!(l.destination(), Some(b));
        assert!(!l.accept(a, later + Duration::from_millis(200)));
    }

    #[test]
    fn latch_sporadic_packets_after_silence_do_not_move() {
        let t = Instant::now();
        let a = addr("192.0.2.1:5000");
        let b = addr("198.51.100.9:6000");
        let mut l = Latch::new(None);
        let (_, now) = feed(&mut l, a, t, 3);
        // Long silence, then b sends one packet every 500 ms (gap > 200 ms).
        let mut at = now + Duration::from_secs(5);
        for _ in 0..10 {
            assert!(!l.accept(b, at));
            at += Duration::from_millis(500);
        }
        assert_eq!(l.destination(), Some(a));
    }

    #[test]
    fn latch_reset_on_new_sdp_address() {
        let t = Instant::now();
        let a = addr("192.0.2.1:5000");
        let new_sdp = addr("203.0.113.5:7000");
        let mut l = Latch::new(None);
        assert!(l.accept(a, t));
        l.reset(Some(new_sdp));
        assert_eq!(l.destination(), Some(new_sdp));
        // After reset the first valid source latches again.
        let c = addr("203.0.113.6:7002");
        assert!(l.accept(c, t + Duration::from_millis(20)));
        assert_eq!(l.destination(), Some(c));
    }

    #[tokio::test]
    async fn rtp_drops_audio_from_non_latched_source() {
        let cancel = CancellationToken::new();
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = sock.local_addr().unwrap();
        let (sess, mut port) = start(sock, G711::Ulaw, None, cancel.clone()).unwrap();
        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let pkt = |seq: u16| RtpPacket {
            marker: false,
            payload_type: 0,
            seq,
            timestamp: seq as u32 * 160,
            ssrc: 5,
            payload: vec![0xFF; 160],
        };
        a.send_to(&pkt(1).build(), local).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), port.uplink.recv())
            .await
            .unwrap()
            .unwrap();
        // Injected packets from b are not decoded and not counted.
        for s in 2..6 {
            b.send_to(&pkt(s).build(), local).await.unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(200), port.uplink.recv())
                .await
                .is_err()
        );
        assert_eq!(sess.rx_packets.load(Ordering::Relaxed), 1);
        // Downlink still goes to a.
        port.downlink.send(vec![0i16; 160]).await.unwrap();
        let mut buf = [0u8; 400];
        tokio::time::timeout(Duration::from_secs(1), a.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        // re-INVITE pointing at b resets the latch: b is accepted.
        sess.control().update_sdp_remote(b.local_addr().unwrap());
        b.send_to(&pkt(7).build(), local).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), port.uplink.recv())
            .await
            .unwrap()
            .unwrap();
        cancel.cancel();
    }

    #[tokio::test]
    async fn allocator_binds_even_ports_in_range() {
        let a = PortAllocator::new("127.0.0.1".parse().unwrap(), 41001, 41009);
        let s1 = a.bind().await.unwrap();
        let s2 = a.bind().await.unwrap();
        let p1 = s1.local_addr().unwrap().port();
        let p2 = s2.local_addr().unwrap().port();
        assert!(p1 % 2 == 0 && (41002..=41009).contains(&p1));
        assert_ne!(p1, p2);
    }

    #[tokio::test]
    async fn symmetric_rtp_and_rechunking() {
        let cancel = CancellationToken::new();
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = sock.local_addr().unwrap();
        // SDP says a bogus address; symmetric RTP must override it.
        let bogus: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let (sess, mut port) = start(sock, G711::Ulaw, Some(bogus), cancel.clone()).unwrap();

        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // Send a 30 ms packet (240 samples) → expect one 160 frame (+80 pending).
        let pkt = RtpPacket {
            marker: false,
            payload_type: 0,
            seq: 1,
            timestamp: 0,
            ssrc: 5,
            payload: vec![0xFF; 240],
        };
        peer.send_to(&pkt.build(), local).await.unwrap();
        // Ignored payload type (telephone-event 101).
        let dtmf = RtpPacket {
            payload_type: 101,
            payload: vec![1, 2, 3, 4],
            ..pkt.clone()
        };
        peer.send_to(&dtmf.build(), local).await.unwrap();
        let f = tokio::time::timeout(Duration::from_secs(1), port.uplink.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(f.len(), 160);
        assert!(f.iter().all(|&s| s == 0));

        // Downlink goes to the latched address.
        port.downlink.send(vec![0i16; 160]).await.unwrap();
        let mut buf = [0u8; 400];
        let (n, _) = tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let back = RtpPacket::parse(&buf[..n]).unwrap();
        assert_eq!(back.payload_type, 0);
        assert!(back.marker);
        assert_eq!(back.payload.len(), 160);
        assert_eq!(sess.rx_packets.load(Ordering::Relaxed), 1);
        assert!(sess.idle_for() < Duration::from_secs(1));
        cancel.cancel();
    }
}
