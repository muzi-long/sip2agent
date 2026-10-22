//! Minimal SDP offer/answer for a single G.711 audio stream.
//!
//! Only PCMA / PCMU at 8 kHz, ptime 20 ms. No DTMF (telephone-event is never
//! offered back). The answer picks the first of PCMA/PCMU in the offer's
//! m-line order.

use crate::audio::g711::G711;
use std::net::{IpAddr, SocketAddr};

/// What we need from the remote offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// Remote RTP address (connection address + m=audio port).
    pub remote_rtp: Option<SocketAddr>,
    /// Chosen codec.
    pub codec: G711,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfferError {
    /// No `m=audio` line.
    NoAudio,
    /// Neither PCMA nor PCMU offered (→ 488).
    NoCompatibleCodec,
}

impl std::fmt::Display for OfferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OfferError::NoAudio => write!(f, "no m=audio in offer"),
            OfferError::NoCompatibleCodec => write!(f, "no PCMA/PCMU in offer"),
        }
    }
}

impl std::error::Error for OfferError {}

/// Parse an SDP offer and choose the codec.
pub fn parse_offer(sdp: &str) -> Result<Offer, OfferError> {
    let mut session_ip: Option<IpAddr> = None;
    let mut media_ip: Option<IpAddr> = None;
    let mut audio_port: Option<u16> = None;
    let mut payloads: Vec<u8> = Vec::new();
    let mut rtpmap: Vec<(u8, String, u32)> = Vec::new();
    let mut in_audio = false;
    let mut seen_audio = false;

    for raw in sdp.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            if seen_audio && in_audio {
                // Only the first audio stream is used.
                in_audio = false;
                continue;
            }
            let mut it = rest.split_whitespace();
            let media = it.next().unwrap_or("");
            in_audio = media == "audio" && !seen_audio;
            if in_audio {
                seen_audio = true;
                audio_port = it.next().and_then(|p| p.parse().ok());
                let _proto = it.next();
                payloads = it.filter_map(|p| p.parse().ok()).collect();
            }
        } else if let Some(rest) = line.strip_prefix("c=") {
            // c=IN IP4 1.2.3.4
            let ip = rest
                .split_whitespace()
                .nth(2)
                .and_then(|a| a.split('/').next())
                .and_then(|a| a.parse::<IpAddr>().ok());
            if in_audio {
                media_ip = ip;
            } else if !seen_audio {
                session_ip = ip;
            }
        } else if let Some(rest) = line.strip_prefix("a=rtpmap:")
            && in_audio
        {
            // a=rtpmap:8 PCMA/8000
            let mut it = rest.splitn(2, ' ');
            let pt = it.next().and_then(|p| p.trim().parse::<u8>().ok());
            let enc = it.next().unwrap_or("");
            let mut parts = enc.split('/');
            let name = parts.next().unwrap_or("").to_string();
            let rate = parts.next().and_then(|r| r.parse().ok()).unwrap_or(0);
            if let Some(pt) = pt {
                rtpmap.push((pt, name, rate));
            }
        }
    }

    if !seen_audio {
        return Err(OfferError::NoAudio);
    }

    let codec = payloads
        .iter()
        .find_map(|&pt| codec_for_payload(pt, &rtpmap))
        .ok_or(OfferError::NoCompatibleCodec)?;

    let ip = media_ip.or(session_ip);
    let remote_rtp = match (ip, audio_port) {
        (Some(ip), Some(port)) if port != 0 && !ip.is_unspecified() => {
            Some(SocketAddr::new(ip, port))
        }
        _ => None,
    };
    Ok(Offer { remote_rtp, codec })
}

fn codec_for_payload(pt: u8, rtpmap: &[(u8, String, u32)]) -> Option<G711> {
    if let Some((_, name, rate)) = rtpmap.iter().find(|(p, _, _)| *p == pt) {
        let c = G711::from_name(name)?;
        return (*rate == 8000 || *rate == 0).then_some(c);
    }
    // Static payload types without rtpmap.
    match pt {
        0 => Some(G711::Ulaw),
        8 => Some(G711::Alaw),
        _ => None,
    }
}

/// Build our SDP answer.
pub fn build_answer(local_ip: IpAddr, rtp_port: u16, codec: G711, session_id: u64) -> String {
    let ip_ver = if local_ip.is_ipv6() { "IP6" } else { "IP4" };
    let pt = codec.payload_type();
    format!(
        "v=0\r\n\
         o=sip2agent {session_id} {session_id} IN {ip_ver} {local_ip}\r\n\
         s=sip2agent\r\n\
         c=IN {ip_ver} {local_ip}\r\n\
         t=0 0\r\n\
         m=audio {rtp_port} RTP/AVP {pt}\r\n\
         a=rtpmap:{pt} {}/8000\r\n\
         a=ptime:20\r\n\
         a=sendrecv\r\n",
        codec.name()
    )
}

/// Build an offer (used by the test UAC).
pub fn build_offer(local_ip: IpAddr, rtp_port: u16, codecs: &[G711]) -> String {
    let ip_ver = if local_ip.is_ipv6() { "IP6" } else { "IP4" };
    let pts: Vec<String> = codecs
        .iter()
        .map(|c| c.payload_type().to_string())
        .collect();
    let mut s = format!(
        "v=0\r\no=uac 1 1 IN {ip_ver} {local_ip}\r\ns=uac\r\nc=IN {ip_ver} {local_ip}\r\nt=0 0\r\nm=audio {rtp_port} RTP/AVP {}\r\n",
        pts.join(" ")
    );
    for c in codecs {
        s.push_str(&format!(
            "a=rtpmap:{} {}/8000\r\n",
            c.payload_type(),
            c.name()
        ));
    }
    s.push_str("a=ptime:20\r\na=sendrecv\r\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\no=- 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
m=audio 4000 RTP/AVP 9 0 8 101\r\na=rtpmap:9 G722/8000\r\na=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\na=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-16\r\n";

    #[test]
    fn picks_first_g711_in_offer_order() {
        let o = parse_offer(OFFER).unwrap();
        assert_eq!(o.codec, G711::Ulaw);
        assert_eq!(o.remote_rtp, Some("10.0.0.1:4000".parse().unwrap()));
    }

    #[test]
    fn pcma_first() {
        let sdp = OFFER.replace("RTP/AVP 9 0 8 101", "RTP/AVP 8 0");
        assert_eq!(parse_offer(&sdp).unwrap().codec, G711::Alaw);
    }

    #[test]
    fn static_payload_without_rtpmap() {
        let sdp = "v=0\r\nc=IN IP4 1.2.3.4\r\nm=audio 5000 RTP/AVP 8\r\n";
        let o = parse_offer(sdp).unwrap();
        assert_eq!(o.codec, G711::Alaw);
    }

    #[test]
    fn media_level_connection_wins() {
        let sdp = "v=0\r\nc=IN IP4 1.1.1.1\r\nm=audio 5000 RTP/AVP 0\r\nc=IN IP4 2.2.2.2\r\n";
        assert_eq!(
            parse_offer(sdp).unwrap().remote_rtp,
            Some("2.2.2.2:5000".parse().unwrap())
        );
    }

    #[test]
    fn rejects_unsupported() {
        let sdp =
            "v=0\r\nc=IN IP4 1.1.1.1\r\nm=audio 5000 RTP/AVP 9 101\r\na=rtpmap:9 G722/8000\r\n";
        assert_eq!(parse_offer(sdp), Err(OfferError::NoCompatibleCodec));
        assert_eq!(
            parse_offer("v=0\r\nm=video 1 RTP/AVP 96\r\n"),
            Err(OfferError::NoAudio)
        );
        // Dynamic PT mapped to a wideband rate is not G.711.
        let sdp =
            "v=0\r\nc=IN IP4 1.1.1.1\r\nm=audio 5000 RTP/AVP 96\r\na=rtpmap:96 PCMA/16000\r\n";
        assert_eq!(parse_offer(sdp), Err(OfferError::NoCompatibleCodec));
    }

    #[test]
    fn answer_has_no_telephone_event() {
        let a = build_answer("192.0.2.1".parse().unwrap(), 20000, G711::Alaw, 42);
        assert!(a.contains("m=audio 20000 RTP/AVP 8\r\n"));
        assert!(a.contains("a=rtpmap:8 PCMA/8000"));
        assert!(a.contains("a=ptime:20"));
        assert!(a.contains("c=IN IP4 192.0.2.1"));
        assert!(!a.contains("telephone-event"));
        let o = parse_offer(&a).unwrap();
        assert_eq!(o.codec, G711::Alaw);
    }
}
