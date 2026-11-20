//! Finding the TCP ports that are listening in this network namespace.

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use devfwd_proto::Listener;

/// All listening TCP ports. Uses a netlink query limited to listening
/// sockets where possible; reading `/proc/net/tcp` makes the kernel walk
/// every TCP socket and is kept as the fallback.
pub fn listeners() -> BTreeSet<Listener> {
    #[cfg(target_os = "linux")]
    if let Ok(found) = crate::sockdiag::listeners() {
        return dedupe(found);
    }
    dedupe(crate::procnet::listeners())
}

/// A port bound on both a loopback and a wildcard address counts as reachable.
pub fn dedupe(all: BTreeSet<Listener>) -> BTreeSet<Listener> {
    all.iter()
        .filter(|l| !(l.loopback && all.contains(&Listener { port: l.port, loopback: false })))
        .copied()
        .collect()
}

/// Whether a raw IPv4 (4 bytes) or IPv6 (16 bytes) address is a loopback one.
pub fn is_loopback(addr: &[u8]) -> Option<bool> {
    Some(match addr.len() {
        4 => Ipv4Addr::new(addr[0], addr[1], addr[2], addr[3]).is_loopback(),
        16 => {
            let v6 = Ipv6Addr::from(<[u8; 16]>::try_from(addr).ok()?);
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        _ => return None,
    })
}

/// How long to wait before the next scan while watching, given how long the
/// set of listeners has been unchanged. Servers tend to start in bursts, so
/// scan often right after a change and rarely once things are quiet.
pub fn poll_interval(quiet: Duration) -> Duration {
    match quiet.as_secs() {
        0..30 => Duration::from_secs(1),
        30..120 => Duration::from_secs(2),
        _ => Duration::from_secs(5),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(port: u16, loopback: bool) -> Listener {
        Listener { port, loopback }
    }

    #[test]
    fn wildcard_bind_wins_over_loopback() {
        let all = BTreeSet::from([l(80, true), l(80, false), l(3000, true)]);
        assert_eq!(dedupe(all).into_iter().collect::<Vec<_>>(), vec![l(80, false), l(3000, true)]);
    }

    #[test]
    fn loopback_addresses() {
        assert_eq!(is_loopback(&[127, 0, 0, 1]), Some(true));
        assert_eq!(is_loopback(&[127, 0, 0, 11]), Some(true));
        assert_eq!(is_loopback(&[0, 0, 0, 0]), Some(false));
        assert_eq!(is_loopback(&[172, 20, 0, 2]), Some(false));
        assert_eq!(is_loopback(&Ipv6Addr::LOCALHOST.octets()), Some(true));
        assert_eq!(is_loopback(&Ipv6Addr::UNSPECIFIED.octets()), Some(false));
        assert_eq!(is_loopback(&Ipv4Addr::LOCALHOST.to_ipv6_mapped().octets()), Some(true));
        assert_eq!(is_loopback(&[1, 2, 3]), None);
    }

    #[test]
    fn polling_backs_off_when_quiet() {
        let at = |secs| poll_interval(Duration::from_secs(secs)).as_secs();
        assert_eq!((at(0), at(29)), (1, 1));
        assert_eq!((at(30), at(119)), (2, 2));
        assert_eq!((at(120), at(86_400)), (5, 5));
    }
}
