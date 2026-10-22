//! G.711 A-law (PCMA) / µ-law (PCMU) codecs, ITU-T G.711 reference algorithm.

/// Segment end points for the 13-bit A-law magnitude.
const SEG_AEND: [i16; 8] = [0x1F, 0x3F, 0x7F, 0xFF, 0x1FF, 0x3FF, 0x7FF, 0xFFF];

fn search_seg(val: i16) -> usize {
    SEG_AEND.iter().position(|&end| val <= end).unwrap_or(8)
}

/// Encode one 16-bit linear sample to A-law.
pub fn linear_to_alaw(pcm: i16) -> u8 {
    // Work on the 13-bit magnitude as in the reference implementation.
    let mut pcm_val = pcm >> 3;
    let mask: u8 = if pcm_val >= 0 {
        0xD5
    } else {
        pcm_val = -pcm_val - 1;
        0x55
    };
    let seg = search_seg(pcm_val);
    let aval: u8 = if seg >= 8 {
        0x7F
    } else {
        let shift = if seg < 2 { 1 } else { seg };
        ((seg as u8) << 4) | (((pcm_val >> shift) & 0x0F) as u8)
    };
    aval ^ mask
}

/// Decode one A-law byte to 16-bit linear.
pub fn alaw_to_linear(a_val: u8) -> i16 {
    let a_val = a_val ^ 0x55;
    let mut t: i16 = ((a_val & 0x0F) as i16) << 4;
    let seg = (a_val & 0x70) >> 4;
    match seg {
        0 => t += 8,
        1 => t += 0x108,
        _ => {
            t += 0x108;
            t <<= seg - 1;
        }
    }
    if a_val & 0x80 != 0 { t } else { -t }
}

const BIAS: i32 = 0x84;
const CLIP: i32 = 32635;

/// Encode one 16-bit linear sample to µ-law.
pub fn linear_to_ulaw(pcm: i16) -> u8 {
    let mut v = pcm as i32;
    let sign: u8 = if v < 0 {
        v = -v;
        0x80
    } else {
        0
    };
    if v > CLIP {
        v = CLIP;
    }
    v += BIAS;
    let mut exponent: u8 = 7;
    let mut mask = 0x4000;
    while exponent > 0 && (v & mask) == 0 {
        exponent -= 1;
        mask >>= 1;
    }
    let mantissa = ((v >> (exponent as i32 + 3)) & 0x0F) as u8;
    !(sign | (exponent << 4) | mantissa)
}

/// Decode one µ-law byte to 16-bit linear.
pub fn ulaw_to_linear(u_val: u8) -> i16 {
    let u = !u_val;
    let mut t: i32 = (((u & 0x0F) as i32) << 3) + BIAS;
    t <<= (u & 0x70) >> 4;
    let v = if u & 0x80 != 0 { BIAS - t } else { t - BIAS };
    v as i16
}

/// The two codecs we negotiate. Nothing else is supported by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum G711 {
    /// PCMA, static payload type 8.
    Alaw,
    /// PCMU, static payload type 0.
    Ulaw,
}

impl G711 {
    pub fn payload_type(self) -> u8 {
        match self {
            G711::Alaw => 8,
            G711::Ulaw => 0,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            G711::Alaw => "PCMA",
            G711::Ulaw => "PCMU",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "PCMA" => Some(G711::Alaw),
            "PCMU" => Some(G711::Ulaw),
            _ => None,
        }
    }

    pub fn encode(self, pcm: &[i16]) -> Vec<u8> {
        match self {
            G711::Alaw => pcm.iter().map(|&s| linear_to_alaw(s)).collect(),
            G711::Ulaw => pcm.iter().map(|&s| linear_to_ulaw(s)).collect(),
        }
    }

    pub fn decode(self, data: &[u8]) -> Vec<i16> {
        match self {
            G711::Alaw => data.iter().map(|&b| alaw_to_linear(b)).collect(),
            G711::Ulaw => data.iter().map(|&b| ulaw_to_linear(b)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_codes() {
        assert_eq!(linear_to_alaw(0), 0xD5);
        assert_eq!(linear_to_ulaw(0), 0xFF);
        assert_eq!(alaw_to_linear(0xD5), 8);
        assert_eq!(ulaw_to_linear(0xFF), 0);
    }

    #[test]
    fn alaw_roundtrip_is_idempotent_on_codewords() {
        for b in 0..=255u8 {
            let lin = alaw_to_linear(b);
            assert_eq!(linear_to_alaw(lin), b, "a-law codeword {b:#x}");
        }
    }

    #[test]
    fn ulaw_roundtrip_is_idempotent_on_codewords() {
        for b in 0..=255u8 {
            // 0x7F and 0xFF both decode to 0 (negative/positive zero).
            if b == 0x7F {
                continue;
            }
            let lin = ulaw_to_linear(b);
            assert_eq!(linear_to_ulaw(lin), b, "u-law codeword {b:#x}");
        }
    }

    #[test]
    fn quantization_error_is_bounded() {
        for codec in [G711::Alaw, G711::Ulaw] {
            for s in (-32000i32..32000).step_by(37) {
                let s = s as i16;
                let back = codec.decode(&codec.encode(&[s]))[0];
                let err = (s as i32 - back as i32).abs();
                // Max quantization step in the top segment is 1024 (A) / 1024 (µ).
                assert!(err <= 1024, "{codec:?} {s} -> {back}");
                // Sign preserved for non-tiny values.
                if s.abs() > 64 {
                    assert_eq!(s.signum(), back.signum(), "{codec:?} {s} -> {back}");
                }
            }
        }
    }

    #[test]
    fn names_and_payload_types() {
        assert_eq!(G711::from_name("pcma"), Some(G711::Alaw));
        assert_eq!(G711::from_name("PCMU"), Some(G711::Ulaw));
        assert_eq!(G711::from_name("opus"), None);
        assert_eq!(G711::Alaw.payload_type(), 8);
        assert_eq!(G711::Ulaw.payload_type(), 0);
    }
}
