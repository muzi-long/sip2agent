//! Source-IP allowlist (`sip.allow`): IPs / CIDRs, IPv4 and IPv6.

use serde::{Deserialize, Deserializer};
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

/// One allowlist entry: an address plus prefix length (`/32` or `/128` for a
/// bare IP). Host bits beyond the prefix are ignored when matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    pub fn contains(&self, ip: IpAddr) -> bool {
        // `::ffff:a.b.c.d` (dual-stack sockets) matches IPv4 entries.
        match (self.addr, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                prefix_eq(u32::from(net).into(), u32::from(ip).into(), self.prefix, 32)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                prefix_eq(u128::from(net), u128::from(ip), self.prefix, 128)
            }
            _ => false,
        }
    }
}

fn prefix_eq(a: u128, b: u128, prefix: u8, bits: u8) -> bool {
    if prefix == 0 {
        return true;
    }
    let shift = u32::from(bits - prefix);
    (a >> shift) == (b >> shift)
}

impl FromStr for IpNet {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let (ip, prefix) = match s.split_once('/') {
            Some((ip, p)) => (ip, Some(p)),
            None => (s, None),
        };
        let addr = IpAddr::from_str(ip).map_err(|_| format!("invalid IP in '{s}'"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => max,
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("invalid prefix length in '{s}'"))?,
        };
        Ok(Self { addr, prefix })
    }
}

impl fmt::Display for IpNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl<'de> Deserialize<'de> for IpNet {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Empty list = allow everyone.
pub fn allowed(list: &[IpNet], ip: IpAddr) -> bool {
    list.is_empty() || list.iter().any(|n| n.contains(ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(s: &str) -> IpNet {
        s.parse().unwrap()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn parse_valid_and_invalid() {
        assert_eq!(net("203.0.113.7").to_string(), "203.0.113.7/32");
        assert_eq!(net(" 10.0.0.0/8 ").to_string(), "10.0.0.0/8");
        assert_eq!(net("2001:db8::/32").to_string(), "2001:db8::/32");
        assert_eq!(net("::1").to_string(), "::1/128");
        for bad in [
            "",
            "10.0.0.0/33",
            "::/129",
            "10.0.0/8",
            "10.0.0.0/x",
            "host.example",
        ] {
            assert!(bad.parse::<IpNet>().is_err(), "{bad} should fail");
        }
    }

    #[test]
    fn matching() {
        let n = net("10.1.0.0/16");
        assert!(n.contains(ip("10.1.255.3")));
        assert!(!n.contains(ip("10.2.0.1")));
        assert!(!n.contains(ip("::1")));
        // host bits in the entry are ignored
        assert!(net("192.168.1.77/24").contains(ip("192.168.1.1")));
        assert!(net("0.0.0.0/0").contains(ip("8.8.8.8")));
        assert!(!net("0.0.0.0/0").contains(ip("2001:db8::1")));
        assert!(net("::/0").contains(ip("2001:db8::1")));
        assert!(net("2001:db8::/32").contains(ip("2001:db8:ffff::1")));
        assert!(!net("2001:db8::/32").contains(ip("2001:db9::1")));
        assert!(net("127.0.0.1").contains(ip("127.0.0.1")));
        assert!(!net("127.0.0.1").contains(ip("127.0.0.2")));
        // IPv4-mapped IPv6 source matches IPv4 entries
        assert!(net("127.0.0.0/8").contains(ip("::ffff:127.0.0.1")));
    }

    #[test]
    fn empty_list_allows_all() {
        assert!(allowed(&[], ip("198.51.100.1")));
        let list = [net("10.0.0.0/8"), net("203.0.113.7")];
        assert!(allowed(&list, ip("203.0.113.7")));
        assert!(allowed(&list, ip("10.9.9.9")));
        assert!(!allowed(&list, ip("203.0.113.8")));
    }
}
