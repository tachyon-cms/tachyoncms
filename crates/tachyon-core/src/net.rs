//! Which addresses a fetch the server makes for someone else may reach.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// IPv4 ranges a server-side fetch never reaches, as (network, prefix length).
const V4_REFUSED: &[([u8; 4], u8)] = &[
    ([0, 0, 0, 0], 8),       // "this network", 0.0.0.0 included
    ([10, 0, 0, 0], 8),      // RFC 1918
    ([100, 64, 0, 0], 10),   // carrier-grade NAT
    ([127, 0, 0, 0], 8),     // loopback
    ([169, 254, 0, 0], 16),  // link-local, the cloud metadata service included
    ([172, 16, 0, 0], 12),   // RFC 1918
    ([192, 0, 0, 0], 24),    // IETF protocol assignments
    ([192, 0, 2, 0], 24),    // documentation
    ([192, 168, 0, 0], 16),  // RFC 1918
    ([198, 18, 0, 0], 15),   // benchmarking
    ([198, 51, 100, 0], 24), // documentation
    ([203, 0, 113, 0], 24),  // documentation
    ([224, 0, 0, 0], 4),     // multicast
    ([240, 0, 0, 0], 4),     // reserved, the broadcast address included
];

/// IPv6 ranges refused outright; those carrying an IPv4 address are judged by it instead.
const V6_REFUSED: &[(u128, u8)] = &[
    (0, 128),                     // unspecified
    (1, 128),                     // loopback
    (0x0064_ff9b_0001 << 80, 48), // local-use NAT64
    (0x0100 << 112, 64),          // discard
    (0x2001_0db8 << 96, 32),      // documentation
    (0x3fff << 112, 20),          // documentation
    (0xfc00 << 112, 7),           // unique local, fd00:ec2::254 included
    (0xfe80 << 112, 10),          // link-local
    (0xfec0 << 112, 10),          // site-local, deprecated but still routed by some stacks
    (0xff00 << 112, 8),           // multicast
];

fn in_range(addr: u128, net: u128, len: u8, bits: u32) -> bool {
    let shift = bits - u32::from(len);
    len == 0 || addr >> shift == net >> shift
}

/// Whether `ip` is a public address a fetch made for someone else may connect to: not
/// loopback, private, link-local, shared, reserved, documentation or multicast. An IPv6
/// address that carries an IPv4 one (mapped, translated, compatible, NAT64, 6to4, Teredo) is judged by
/// the IPv4 inside it.
pub fn is_public_addr(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => public_v6(v6),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let addr = u128::from(u32::from(ip));
    !V4_REFUSED
        .iter()
        .any(|(net, len)| in_range(addr, u128::from(u32::from_be_bytes(*net)), *len, 32))
}

fn public_v6(ip: Ipv6Addr) -> bool {
    let addr = u128::from(ip);
    if V6_REFUSED
        .iter()
        .any(|(net, len)| in_range(addr, *net, *len, 128))
    {
        return false;
    }
    let v4 = |bits: u128| Ipv4Addr::from((bits & 0xffff_ffff) as u32);
    // Mapped (::ffff:0:0/96), translated (::ffff:0:0:0/96), compatible (::/96) and NAT64
    // (64:ff9b::/96): the last 32 bits.
    if in_range(addr, 0xffff << 32, 96, 128)
        || in_range(addr, 0xffff << 48, 96, 128)
        || in_range(addr, 0, 96, 128)
        || in_range(addr, 0x0064_ff9b << 96, 96, 128)
    {
        return public_v4(v4(addr));
    }
    // 6to4 (2002::/16): the 32 bits after the prefix.
    if in_range(addr, 0x2002 << 112, 16, 128) {
        return public_v4(v4(addr >> 80));
    }
    // Teredo (2001::/32): the server's address, then the client's, stored inverted.
    if in_range(addr, 0x2001 << 112, 32, 128) {
        return public_v4(v4(addr >> 64)) && public_v4(v4(!addr));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(cases: &[&str]) {
        for case in cases {
            let ip: IpAddr = case.parse().unwrap();
            assert!(!is_public_addr(ip), "{case} passed");
        }
    }

    /// Each range's first, a middle and its last address are refused.
    #[test]
    fn public_addr_table() {
        refused(&[
            "0.0.0.0",
            "0.1.2.3",
            "0.255.255.255",
            "10.0.0.0",
            "10.20.30.40",
            "10.255.255.255",
            "100.64.0.0",
            "100.100.1.1",
            "100.127.255.255",
            "127.0.0.0",
            "127.0.0.1",
            "127.255.255.255",
            "169.254.0.0",
            "169.254.169.254",
            "169.254.255.255",
            "172.16.0.0",
            "172.20.1.1",
            "172.31.255.255",
            "192.0.0.0",
            "192.0.0.100",
            "192.0.0.255",
            "192.0.2.0",
            "192.0.2.100",
            "192.0.2.255",
            "192.168.0.0",
            "192.168.1.1",
            "192.168.255.255",
            "198.18.0.0",
            "198.19.1.1",
            "198.19.255.255",
            "198.51.100.0",
            "198.51.100.7",
            "198.51.100.255",
            "203.0.113.0",
            "203.0.113.9",
            "203.0.113.255",
            "224.0.0.0",
            "230.1.2.3",
            "239.255.255.255",
            "240.0.0.0",
            "250.1.2.3",
            "255.255.255.255",
        ]);
        refused(&[
            "::",
            "::1",
            "64:ff9b:1::",
            "64:ff9b:1:8000::1",
            "64:ff9b:1:ffff:ffff:ffff:ffff:ffff",
            "100::",
            "100::8000:0:0:1",
            "100::ffff:ffff:ffff:ffff",
            "2001:db8::",
            "2001:db8:8000::1",
            "2001:db8:ffff:ffff:ffff:ffff:ffff:ffff",
            "3fff::",
            "3fff:800::1",
            "3fff:fff:ffff:ffff:ffff:ffff:ffff:ffff",
            "fc00::",
            "fd00:ec2::254",
            "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "fe80::",
            "fe80::1",
            "febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "fec0::",
            "fed0::1",
            "feff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "ff00::",
            "ff02::1",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        ]);
        // IPv4 inside IPv6 is judged as itself.
        refused(&[
            "::ffff:127.0.0.1",
            // IPv4-translated (SIIT, ::ffff:0:0:0/96).
            "::ffff:0:127.0.0.1",
            "::ffff:0:10.0.0.1",
            "::ffff:0:a9fe:a9fe",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "::10.0.0.1",
            "64:ff9b::127.0.0.1",
            "64:ff9b::a00:1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::",
            "2002:a00:1::1",
            "2002:c0a8:101:ffff::",
            // Teredo, server 127.0.0.1; then server public, client 10.0.0.1 (inverted).
            "2001:0:7f00:1::1",
            "2001:0:808:808:0:0:f5ff:fffe",
        ]);
        for ok in [
            "8.8.8.8",
            "1.1.1.1",
            "100.63.255.255",
            "100.128.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.0.1.1",
            "192.169.0.1",
            "198.17.255.255",
            "198.20.0.0",
            "223.255.255.255",
            "2606:4700::1111",
            "2a00:1450:4001::1",
            "::ffff:8.8.8.8",
            "::ffff:0:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
            "2001:0:808:808:0:0:f7f7:f7f7",
            "2001:db7:ffff:ffff:ffff:ffff:ffff:ffff",
            "2001:db9::",
        ] {
            assert!(is_public_addr(ok.parse().unwrap()), "{ok} refused");
        }
    }
}
