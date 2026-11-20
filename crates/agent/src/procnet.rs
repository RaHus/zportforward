//! Listening TCP sockets, read from `/proc/net/tcp` and `/proc/net/tcp6`.

use std::collections::BTreeSet;

use zportforward_proto::Listener;

const TCP_LISTEN: &str = "0A";

/// Every listening socket, not deduplicated.
pub fn listeners() -> BTreeSet<Listener> {
    let mut out = BTreeSet::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(text) = std::fs::read_to_string(path) {
            parse(&text, &mut out);
        }
    }
    out
}

fn parse(text: &str, out: &mut BTreeSet<Listener>) {
    for line in text.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let (Some(_slot), Some(local), Some(_remote), Some(state)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if state != TCP_LISTEN {
            continue;
        }
        let Some((addr, port)) = local.rsplit_once(':') else { continue };
        let Ok(port) = u16::from_str_radix(port, 16) else { continue };
        let Some(loopback) = is_loopback(addr) else { continue };
        out.insert(Listener { port, loopback });
    }
}

/// The kernel prints each 32-bit word of the address with `%08X` in host byte
/// order, so the in-memory (network order) bytes are `to_ne_bytes` of each word.
fn is_loopback(hex: &str) -> Option<bool> {
    if hex.len() != 8 && hex.len() != 32 {
        return None;
    }
    let mut bytes = Vec::with_capacity(16);
    for word in hex.as_bytes().chunks(8) {
        let word = u32::from_str_radix(std::str::from_utf8(word).ok()?, 16).ok()?;
        bytes.extend_from_slice(&word.to_ne_bytes());
    }
    crate::scan::is_loopback(&bytes)
}

#[cfg(all(test, target_endian = "little"))]
mod tests {
    use super::*;

    const TCP4: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:AEFD 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1 1 0 100 0 0 10 0
   1: 0B00007F:8F97 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 2 1 0 100 0 0 10 0
   2: 00000000:1538 00000000:0000 0A 00000000:00000000 00:00000000 00000000   999        0 3 1 0 100 0 0 10 0
   3: 0100007F:1539 00000000:0000 0A 00000000:00000000 00:00000000 00000000   999        0 4 1 0 100 0 0 10 0
   4: 020014AC:D2F0 030014AC:1538 01 00000000:00000000 02:000AFC3E 00000000  1000        0 5 2 0 10 -1
";
    const TCP6: &str = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:1538 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000   999        0 6 1 0 100 0 0 10 0
   1: 00000000000000000000000001000000:1539 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000   999        0 7 1 0 100 0 0 10 0
   2: 0000000000000000FFFF00000100007F:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000   999        0 8 1 0 100 0 0 10 0
";

    fn l(port: u16, loopback: bool) -> Listener {
        Listener { port, loopback }
    }

    #[test]
    fn parses_ipv4_listeners_only() {
        let mut out = BTreeSet::new();
        parse(TCP4, &mut out);
        assert_eq!(
            out.into_iter().collect::<Vec<_>>(),
            vec![l(5432, false), l(5433, true), l(36759, true), l(44797, true)]
        );
    }

    #[test]
    fn parses_ipv6_including_v4_mapped() {
        let mut out = BTreeSet::new();
        parse(TCP6, &mut out);
        assert_eq!(
            out.into_iter().collect::<Vec<_>>(),
            vec![l(3000, true), l(5432, false), l(5433, true)]
        );
    }

    #[test]
    fn ignores_garbage() {
        let mut out = BTreeSet::new();
        parse("header\nnot a line\n 0: zz:1 x 0A\n 1: 0100007F:zzzz x 0A\n", &mut out);
        assert!(out.is_empty());
    }
}
