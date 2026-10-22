//! Linear-interpolation resamplers.
//!
//! Ported verbatim from restsend/rustpbx
//! (`src/call/runtime/conference_media_bridge.rs`), MIT License,
//! Copyright (c) 2025 Restsend. Only the visibility changed
//! (`pub(crate)` → `pub`).
//!
//! Used for 8 kHz → 16 kHz (SIP uplink to the realtime model) and
//! 24 kHz → 8 kHz (model audio downlink to SIP).

/// Resample audio using linear interpolation into `out` (reused across
/// frames to avoid per-frame allocation in audio hot paths).
pub fn resample_linear_into(samples: &[i16], src_rate: u32, dst_rate: u32, out: &mut Vec<i16>) {
    if src_rate == dst_rate {
        out.clear();
        out.extend_from_slice(samples);
        return;
    }

    let ratio = src_rate as f32 / dst_rate as f32;
    let new_len = (samples.len() as f32 / ratio) as usize;
    out.clear();
    out.reserve(new_len);

    for i in 0..new_len {
        let src_idx = i as f32 * ratio;
        let src_idx_floor = src_idx.floor() as usize;
        let src_idx_ceil = (src_idx.ceil() as usize).min(samples.len().saturating_sub(1));
        let frac = src_idx - src_idx.floor();

        let sample = if src_idx_floor == src_idx_ceil {
            samples[src_idx_floor]
        } else {
            let s0 = samples[src_idx_floor] as f32;
            let s1 = samples[src_idx_ceil] as f32;
            (s0 + frac * (s1 - s0)) as i16
        };
        out.push(sample);
    }
}

/// Resample audio using linear interpolation.
pub fn resample_linear(samples: &[i16], src_rate: u32, dst_rate: u32) -> Vec<i16> {
    let mut out = Vec::new();
    resample_linear_into(samples, src_rate, dst_rate, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_rate_is_copy() {
        let s = vec![1i16, 2, 3];
        assert_eq!(resample_linear(&s, 8000, 8000), s);
    }

    #[test]
    fn upsample_8k_to_16k_doubles_and_interpolates() {
        let s: Vec<i16> = (0..160).map(|i| (i * 10) as i16).collect();
        let out = resample_linear(&s, 8000, 16000);
        assert_eq!(out.len(), 320);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], 5); // midpoint
        assert_eq!(out[2], 10);
    }

    #[test]
    fn downsample_24k_to_8k_takes_every_third() {
        let s: Vec<i16> = (0..480).map(|i| i as i16).collect();
        let out = resample_linear(&s, 24000, 8000);
        assert_eq!(out.len(), 160);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], 3);
        assert_eq!(out[159], 477);
    }

    #[test]
    fn into_reuses_buffer() {
        let mut out = vec![9i16; 1000];
        resample_linear_into(&[1, 2], 8000, 16000, &mut out);
        assert_eq!(out.len(), 4);
    }
}
