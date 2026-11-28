//! Wake-on-LAN for "Use your other Mac" (v0.6 M5, plan §5 "Asleep").
//!
//! The sharing Mac sends its Ethernet / Wi-Fi hardware addresses at pairing (encrypted,
//! bound into the confirmation tag, stored as `wake_macs` in the trust store). When the
//! paired Mac cannot be reached, the using Mac sends the standard magic packet
//! (6 × `0xFF` + 16 × the MAC address, 102 bytes) as a UDP broadcast to port 9 and keeps
//! re-dialing for [`WAKE_WINDOW`].
//!
//! Nothing here changes system settings or needs privileges: reading interface addresses
//! is `ifconfig` (read-only), sending a UDP broadcast only needs `SO_BROADCAST` on our own
//! socket. Whether the big Mac actually wakes depends on its "Wake for network access"
//! setting and the network (Ethernet: magic packet; Wi-Fi: usually only the Bonjour Sleep
//! Proxy of an Apple router) — that is checked on real hardware in M6.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

/// How long the use side shows "Waking…" and keeps sending magic packets before it says
/// the other Mac is asleep or off.
pub const WAKE_WINDOW: Duration = Duration::from_secs(20);
/// Magic packets are repeated this often inside the window (UDP may drop one).
pub const WAKE_RESEND: Duration = Duration::from_secs(3);
/// The standard WoL port (discard).
pub const WOL_PORT: u16 = 9;

/// Parse `aa:bb:cc:dd:ee:ff` (or `-` separated).
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.trim().split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        if p.len() != 2 {
            return None;
        }
        out[i] = u8::from_str_radix(p, 16).ok()?;
    }
    Some(out)
}

/// The 102-byte magic packet for `mac`.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut p = [0xFFu8; 102];
    for i in 0..16 {
        p[6 + i * 6..12 + i * 6].copy_from_slice(&mac);
    }
    p
}

/// Hardware addresses of this Mac's Ethernet / Wi-Fi ports (`en*` in `ifconfig`), lowercase,
/// deduplicated, at most 4. Multicast / all-zero addresses are skipped.
pub fn local_macs() -> Vec<String> {
    let out = std::process::Command::new("/sbin/ifconfig").output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    parse_ifconfig_macs(&text)
}

/// [`local_macs`] on a given `ifconfig` output (tests).
pub fn parse_ifconfig_macs(text: &str) -> Vec<String> {
    let mut current_en = false;
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if !line.starts_with(['\t', ' ']) {
            let name = line.split(':').next().unwrap_or("");
            current_en = name.starts_with("en") && name[2..].bytes().all(|b| b.is_ascii_digit()) && name.len() > 2;
            continue;
        }
        if !current_en {
            continue;
        }
        if let Some(rest) = line.trim().strip_prefix("ether ") {
            let mac = rest.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            if let Some(b) = parse_mac(&mac) {
                if b != [0; 6] && b[0] & 1 == 0 && !out.contains(&mac) {
                    out.push(mac);
                }
            }
        }
    }
    out.truncate(4);
    out
}

/// IPv4 broadcast addresses of this Mac's up interfaces (`broadcast x.x.x.x` in
/// `ifconfig`) plus the limited broadcast `255.255.255.255`, each on [`WOL_PORT`].
pub fn broadcast_targets() -> Vec<SocketAddr> {
    let out = std::process::Command::new("/sbin/ifconfig").output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    parse_broadcast_targets(&text)
}

pub fn parse_broadcast_targets(text: &str) -> Vec<SocketAddr> {
    let mut v: Vec<SocketAddr> = vec![SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, WOL_PORT))];
    for line in text.lines() {
        let mut it = line.split_whitespace();
        while let Some(tok) = it.next() {
            if tok == "broadcast" {
                if let Some(ip) = it.next().and_then(|s| s.parse::<Ipv4Addr>().ok()) {
                    let a = SocketAddr::V4(SocketAddrV4::new(ip, WOL_PORT));
                    if !v.contains(&a) {
                        v.push(a);
                    }
                }
            }
        }
    }
    v
}

/// Send one magic packet per MAC to every target. Returns the number of datagrams sent.
/// Errors per target are ignored (an interface may be down); no MACs → `Ok(0)`.
pub fn send_wake(macs: &[String], targets: &[SocketAddr]) -> io::Result<usize> {
    let packets: Vec<[u8; 102]> = macs.iter().filter_map(|m| parse_mac(m)).map(magic_packet).collect();
    if packets.is_empty() || targets.is_empty() {
        return Ok(0);
    }
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_broadcast(true)?;
    let mut sent = 0;
    for t in targets {
        for p in &packets {
            if sock.send_to(p, t).is_ok() {
                sent += 1;
            }
        }
    }
    Ok(sent)
}

/// Where magic packets go: the real broadcast targets, or (tests) a fixed list.
#[derive(Debug, Clone, Default)]
pub enum WakeTargets {
    /// `255.255.255.255:9` + each interface's broadcast address.
    #[default]
    Broadcast,
    /// Exactly these (tests send to a loopback socket instead of the LAN).
    Fixed(Vec<SocketAddr>),
    /// Never send (wake disabled).
    Off,
}

impl WakeTargets {
    pub fn resolve(&self) -> Vec<SocketAddr> {
        match self {
            WakeTargets::Broadcast => broadcast_targets(),
            WakeTargets::Fixed(v) => v.clone(),
            WakeTargets::Off => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_packet_layout() {
        let mac = parse_mac("A4:83:e7:01:02:03").unwrap();
        assert_eq!(mac, [0xa4, 0x83, 0xe7, 1, 2, 3]);
        let p = magic_packet(mac);
        assert_eq!(&p[..6], &[0xFF; 6]);
        for i in 0..16 {
            assert_eq!(&p[6 + 6 * i..12 + 6 * i], &mac);
        }
        assert!(parse_mac("a4:83:e7:01:02").is_none());
        assert!(parse_mac("a4:83:e7:01:02:zz").is_none());
        assert_eq!(parse_mac("a4-83-e7-01-02-03"), Some(mac));
    }

    #[test]
    fn ifconfig_parsing() {
        let text = "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384\n\tinet 127.0.0.1 netmask 0xff000000\n\
en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n\tether a4:83:e7:01:02:03\n\tinet 192.168.1.20 netmask 0xffffff00 broadcast 192.168.1.255\n\
en1: flags=8963<UP,BROADCAST> mtu 1500\n\tether A4:83:E7:01:02:04\n\
bridge0: flags=8863<UP> mtu 1500\n\tether 36:10:a0:00:00:01\n\
awdl0: flags=8943<UP> mtu 1484\n\tether 9e:00:00:00:00:01\n\
en5: flags=8863<UP> mtu 1500\n\tether a4:83:e7:01:02:03\n\
en6: flags=8863<UP> mtu 1500\n\tether 00:00:00:00:00:00\n\
utun0: flags=8051<UP> mtu 1380\n\tinet 10.8.0.2 --> 10.8.0.1 netmask 0xffffff00\n\
en7: flags=8863<UP> mtu 1500\n\tinet 10.0.0.5 netmask 0xffff0000 broadcast 10.0.255.255\n";
        assert_eq!(parse_ifconfig_macs(text), vec!["a4:83:e7:01:02:03", "a4:83:e7:01:02:04"]);
        let t = parse_broadcast_targets(text);
        assert_eq!(
            t,
            vec![
                "255.255.255.255:9".parse::<SocketAddr>().unwrap(),
                "192.168.1.255:9".parse().unwrap(),
                "10.0.255.255:9".parse().unwrap()
            ]
        );
    }

    /// The packet really leaves through a UDP socket (to a loopback listener, never the LAN).
    #[test]
    fn send_wake_to_a_fixed_target() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let target = rx.local_addr().unwrap();
        let n = send_wake(&["a4:83:e7:01:02:03".into(), "junk".into()], &[target]).unwrap();
        assert_eq!(n, 1);
        let mut buf = [0u8; 200];
        let (len, _) = rx.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..len], &magic_packet([0xa4, 0x83, 0xe7, 1, 2, 3])[..]);
        assert_eq!(send_wake(&[], &[target]).unwrap(), 0);
        assert!(WakeTargets::Off.resolve().is_empty());
    }
}
