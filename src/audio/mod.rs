//! Audio primitives: G.711, resampling, PCM16 byte helpers.

pub mod g711;
pub mod resample;

/// SIP side sample rate (G.711 is always 8 kHz).
pub const SIP_RATE: u32 = 8000;
/// Samples per 20 ms frame at 8 kHz.
pub const SIP_FRAME_SAMPLES: usize = 160;
/// Media tick (ptime).
pub const FRAME_MS: u64 = 20;

/// Number of samples in one 20 ms frame at `rate`.
pub fn frame_samples(rate: u32) -> usize {
    (rate as usize) * FRAME_MS as usize / 1000
}

/// PCM16 mono → little-endian bytes.
pub fn pcm_to_bytes(pcm: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Little-endian bytes → PCM16 mono (a trailing odd byte is dropped).
pub fn bytes_to_pcm(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_bytes_roundtrip() {
        let pcm = vec![-1i16, 32767, -32768, 42];
        let bytes = pcm_to_bytes(&pcm);
        assert_eq!(bytes.len(), 8);
        assert_eq!(bytes_to_pcm(&bytes), pcm);
        let mut odd = bytes.clone();
        odd.push(0xAB);
        assert_eq!(bytes_to_pcm(&odd), pcm);
    }

    #[test]
    fn frame_sizes() {
        assert_eq!(frame_samples(8000), 160);
        assert_eq!(frame_samples(16000), 320);
        assert_eq!(frame_samples(24000), 480);
    }
}
