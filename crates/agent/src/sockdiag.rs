//! Listening TCP sockets via `NETLINK_SOCK_DIAG`, the interface `ss` uses.
//!
//! The dump is restricted to sockets in LISTEN state, so the kernel walks
//! only its listening table. It needs no privileges and reports the sockets
//! of the caller's network namespace.

use std::collections::BTreeSet;
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};

use devfwd_proto::Listener;

const NETLINK_SOCK_DIAG: libc::c_int = 4;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const TCP_LISTEN: u8 = 10;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 0x001;
const NLM_F_DUMP: u16 = 0x300;

/// `struct nlmsghdr`
const HEADER_LEN: usize = 16;
/// `struct inet_diag_req_v2`
const REQUEST_LEN: usize = 56;

/// Set after a failure, so a kernel without sock_diag is not retried on every scan.
static UNAVAILABLE: AtomicBool = AtomicBool::new(false);

/// Every listening socket, not deduplicated.
pub fn listeners() -> io::Result<BTreeSet<Listener>> {
    if UNAVAILABLE.load(Ordering::Relaxed) {
        return Err(io::ErrorKind::Unsupported.into());
    }
    let result = query();
    if result.is_err() {
        UNAVAILABLE.store(true, Ordering::Relaxed);
    }
    result
}

fn query() -> io::Result<BTreeSet<Listener>> {
    let fd = open()?;
    let mut out = BTreeSet::new();
    for family in [libc::AF_INET, libc::AF_INET6] {
        dump(&fd, family as u8, &mut out)?;
    }
    Ok(out)
}

fn open() -> io::Result<OwnedFd> {
    // SAFETY: plain syscall; the result is checked before use.
    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, NETLINK_SOCK_DIAG) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a freshly created descriptor that nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // The kernel answers a dump synchronously; the timeout only guards
    // against blocking the agent's single thread if that ever stops holding.
    let timeout = libc::timeval { tv_sec: 1, tv_usec: 0 };
    // SAFETY: the pointer and length describe `timeout`, which outlives the call.
    unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw const timeout).cast(),
            size_of::<libc::timeval>() as libc::socklen_t,
        );
    }
    Ok(fd)
}

/// `nlmsghdr` followed by `inet_diag_req_v2`, asking for all listening TCP
/// sockets of one address family. Netlink uses host byte order.
fn request(family: u8) -> [u8; HEADER_LEN + REQUEST_LEN] {
    let mut msg = [0u8; HEADER_LEN + REQUEST_LEN];
    let len = msg.len() as u32;
    msg[0..4].copy_from_slice(&len.to_ne_bytes()); // nlmsg_len
    msg[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes()); // nlmsg_type
    msg[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes()); // nlmsg_flags
    msg[8..12].copy_from_slice(&1u32.to_ne_bytes()); // nlmsg_seq
    msg[16] = family; // sdiag_family
    msg[17] = libc::IPPROTO_TCP as u8; // sdiag_protocol
    msg[20..24].copy_from_slice(&(1u32 << TCP_LISTEN).to_ne_bytes()); // idiag_states
    msg
}

fn dump(fd: &OwnedFd, family: u8, out: &mut BTreeSet<Listener>) -> io::Result<()> {
    let msg = request(family);
    // SAFETY: an all-zero `sockaddr_nl` is valid; pid 0 addresses the kernel.
    let mut kernel: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    kernel.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    // SAFETY: both pointers reference live values of the stated lengths.
    let sent = unsafe {
        libc::sendto(
            fd.as_raw_fd(),
            msg.as_ptr().cast(),
            msg.len(),
            0,
            (&raw const kernel).cast(),
            size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        // SAFETY: `buf` is writable for `buf.len()` bytes.
        let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if parse(&buf[..n as usize], out)? {
            return Ok(());
        }
    }
}

/// Parse one datagram of netlink messages. Returns `true` at the end of the dump.
fn parse(mut data: &[u8], out: &mut BTreeSet<Listener>) -> io::Result<bool> {
    while data.len() >= HEADER_LEN {
        let len = u32::from_ne_bytes(data[0..4].try_into().unwrap()) as usize;
        let kind = u16::from_ne_bytes(data[4..6].try_into().unwrap());
        if len < HEADER_LEN || len > data.len() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let payload = &data[HEADER_LEN..len];
        match kind {
            NLMSG_DONE => return Ok(true),
            NLMSG_ERROR => {
                let errno = payload.get(0..4).map(|b| i32::from_ne_bytes(b.try_into().unwrap()));
                match errno {
                    Some(0) => {} // an acknowledgement, not an error
                    Some(errno) => return Err(io::Error::from_raw_os_error(-errno)),
                    None => return Err(io::ErrorKind::InvalidData.into()),
                }
            }
            SOCK_DIAG_BY_FAMILY => out.extend(listener(payload)),
            _ => {}
        }
        // Messages are padded to four bytes.
        data = &data[((len + 3) & !3).min(data.len())..];
    }
    Ok(false)
}

/// Read the fields we need from a `struct inet_diag_msg`:
/// family (0), state (1), source port (4..6, big endian), source address (8..24).
fn listener(msg: &[u8]) -> Option<Listener> {
    if msg.len() < 24 || msg[1] != TCP_LISTEN {
        return None;
    }
    let addr = match libc::c_int::from(msg[0]) {
        libc::AF_INET => &msg[8..12],
        libc::AF_INET6 => &msg[8..24],
        _ => return None,
    };
    Some(Listener {
        port: u16::from_be_bytes([msg[4], msg[5]]),
        loopback: crate::scan::is_loopback(addr)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv6Addr, TcpListener};

    /// One netlink message carrying an `inet_diag_msg` (72 bytes).
    fn diag_msg(family: libc::c_int, state: u8, port: u16, addr: &[u8]) -> Vec<u8> {
        let mut msg = vec![0u8; HEADER_LEN + 72];
        let len = msg.len() as u32;
        msg[0..4].copy_from_slice(&len.to_ne_bytes());
        msg[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
        msg[16] = family as u8;
        msg[17] = state;
        msg[20..22].copy_from_slice(&port.to_be_bytes());
        msg[24..24 + addr.len()].copy_from_slice(addr);
        msg
    }

    fn done() -> Vec<u8> {
        let mut msg = vec![0u8; HEADER_LEN + 4];
        msg[0..4].copy_from_slice(&20u32.to_ne_bytes());
        msg[4..6].copy_from_slice(&NLMSG_DONE.to_ne_bytes());
        msg
    }

    #[test]
    fn parses_a_dump() {
        let mut data = diag_msg(libc::AF_INET, TCP_LISTEN, 5432, &[0, 0, 0, 0]);
        data.extend(diag_msg(libc::AF_INET, TCP_LISTEN, 5433, &[127, 0, 0, 1]));
        data.extend(diag_msg(libc::AF_INET6, TCP_LISTEN, 3000, &Ipv6Addr::LOCALHOST.octets()));
        data.extend(diag_msg(libc::AF_INET, 1, 9999, &[127, 0, 0, 1])); // established: ignored
        let mut out = BTreeSet::new();
        assert!(!parse(&data, &mut out).unwrap(), "no DONE yet");
        assert!(parse(&done(), &mut out).unwrap());
        assert_eq!(
            out.into_iter().collect::<Vec<_>>(),
            vec![
                Listener { port: 3000, loopback: true },
                Listener { port: 5432, loopback: false },
                Listener { port: 5433, loopback: true },
            ]
        );
    }

    #[test]
    fn reports_kernel_errors_and_rejects_garbage() {
        let mut err = vec![0u8; HEADER_LEN + 4];
        err[0..4].copy_from_slice(&20u32.to_ne_bytes());
        err[4..6].copy_from_slice(&NLMSG_ERROR.to_ne_bytes());
        err[16..20].copy_from_slice(&(-libc::ENOENT).to_ne_bytes());
        let e = parse(&err, &mut BTreeSet::new()).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::ENOENT));

        let mut truncated = diag_msg(libc::AF_INET, TCP_LISTEN, 80, &[0, 0, 0, 0]);
        truncated.truncate(40);
        assert!(parse(&truncated, &mut BTreeSet::new()).is_err());
    }

    /// Against the real kernel: must agree with `/proc/net/tcp` for sockets we own.
    #[test]
    fn finds_real_listeners() {
        let local = TcpListener::bind("127.0.0.1:0").unwrap();
        let any = TcpListener::bind("0.0.0.0:0").unwrap();
        let v6 = TcpListener::bind("[::1]:0").ok();
        let found = query().expect("sock_diag query");
        let has = |l: &TcpListener, loopback| {
            found.contains(&Listener { port: l.local_addr().unwrap().port(), loopback })
        };
        assert!(has(&local, true));
        assert!(has(&any, false));
        if let Some(v6) = &v6 {
            assert!(has(v6, true));
        }
        let procfs = crate::procnet::listeners();
        for l in [Some(&local), Some(&any), v6.as_ref()].into_iter().flatten() {
            let port = l.local_addr().unwrap().port();
            assert_eq!(
                found.iter().filter(|f| f.port == port).collect::<Vec<_>>(),
                procfs.iter().filter(|f| f.port == port).collect::<Vec<_>>()
            );
        }
    }
}
