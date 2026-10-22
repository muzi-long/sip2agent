//! sip2agent — bridge inbound SIP calls (G.711) to realtime speech-to-speech
//! models over WebSocket (Doubao Seeduplex, Qwen-Omni realtime, raw PCM WS).

pub mod acl;
pub mod audio;
pub mod call;
pub mod config;
pub mod realtime;
pub mod rtp;
pub mod sdp;
pub mod sip;
pub mod uac;
pub mod webhook;

/// Install the process-wide rustls crypto provider (ring). Idempotent.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
