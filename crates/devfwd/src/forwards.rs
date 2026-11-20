//! Host-side listeners and the policy for choosing host ports.

use std::io::ErrorKind;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::discovery::{PortConfig, Target};
use crate::tunnel::{Slot, Tunnel};

/// How long a new connection waits for a tunnel that is (re)connecting.
const TUNNEL_WAIT: Duration = Duration::from_secs(10);
/// Detected ports at or above this are treated as ephemeral and never auto-forwarded.
pub const EPHEMERAL_START: u16 = 32768;

/// Accept connections on `listener` and bridge each to `target` in the container.
pub fn spawn(listener: TcpListener, slot: Slot, target: Target) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let tcp = match listener.accept().await {
                Ok((tcp, _)) => tcp,
                Err(e) => {
                    crate::log!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            };
            let (slot, target) = (slot.clone(), target.clone());
            tokio::spawn(async move {
                if let Err(e) = bridge(tcp, slot, &target).await {
                    crate::log!("connection to {} dropped: {e:#}", describe(&target));
                }
            });
        }
    })
}

async fn bridge(mut tcp: TcpStream, slot: Slot, target: &Target) -> Result<()> {
    let _ = tcp.set_nodelay(true);
    let tunnel = current_tunnel(slot).await?;
    let mut stream = tunnel.dial(target).await?;
    // Errors past this point are ordinary resets by either peer.
    let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
    Ok(())
}

async fn current_tunnel(mut slot: Slot) -> Result<Arc<Tunnel>> {
    let wait = slot.wait_for(|t| t.is_some());
    match tokio::time::timeout(TUNNEL_WAIT, wait).await {
        Ok(Ok(tunnel)) => Ok(tunnel.clone().expect("checked by wait_for")),
        Ok(Err(_)) => Err(anyhow!("container is gone")),
        Err(_) => Err(anyhow!("no tunnel to the container after {TUNNEL_WAIT:?}")),
    }
}

pub fn describe(target: &Target) -> String {
    if target.host.is_empty() {
        format!("port {}", target.port)
    } else {
        format!("{}:{}", target.host, target.port)
    }
}

/// Bind `127.0.0.1:port`. With `strict`, a failure is final; otherwise nearby
/// ports are tried. Returns the listener and the port actually bound.
pub async fn bind(port: u16, strict: bool) -> std::result::Result<(TcpListener, u16), String> {
    let first = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
        Ok(listener) => return Ok((listener, port)),
        Err(e) => e,
    };
    let reason = match first.kind() {
        ErrorKind::AddrInUse => format!("host port {port} is already in use"),
        ErrorKind::PermissionDenied => format!("host port {port} needs elevated privileges"),
        _ => format!("cannot bind host port {port}: {first}"),
    };
    if strict {
        return Err(reason);
    }
    for candidate in fallback_ports(port, first.kind()) {
        if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, candidate)).await {
            return Ok((listener, candidate));
        }
    }
    Err(format!("{reason}, and no nearby port was free"))
}

/// Alternative host ports to try when the wanted one cannot be bound.
pub fn fallback_ports(port: u16, kind: ErrorKind) -> Vec<u16> {
    let start = match kind {
        // 80 -> 8080, 443 -> 8443: the conventional unprivileged twins.
        ErrorKind::PermissionDenied if port < 1024 => port + 8000,
        _ => port.saturating_add(1).max(1024),
    };
    (start..=start.saturating_add(20)).filter(|p| *p != port).collect()
}

/// Whether a listener the agent detected should be forwarded in `--auto` mode.
pub fn should_auto_forward(port: u16, cfg: &PortConfig) -> bool {
    port < EPHEMERAL_START && !cfg.attrs(port).ignore_auto_forward
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privileged_ports_fall_back_to_their_twins() {
        let ports = fallback_ports(80, ErrorKind::PermissionDenied);
        assert_eq!(ports.first(), Some(&8080));
        assert!(ports.iter().all(|p| *p >= 1024));
    }

    #[test]
    fn busy_ports_fall_back_to_the_next_ones() {
        assert_eq!(fallback_ports(3000, ErrorKind::AddrInUse)[..3], [3001, 3002, 3003]);
        // A busy privileged port must not fall back to another privileged port.
        assert!(fallback_ports(80, ErrorKind::AddrInUse).iter().all(|p| *p >= 1024));
        // No overflow at the top of the range.
        assert!(fallback_ports(u16::MAX, ErrorKind::AddrInUse).is_empty());
    }

    #[tokio::test]
    async fn busy_port_strict_fails_lenient_moves() {
        let taken = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = taken.local_addr().unwrap().port();

        let err = bind(port, true).await.unwrap_err();
        assert!(err.contains("already in use"), "{err}");

        let (listener, bound) = bind(port, false).await.unwrap();
        assert_ne!(bound, port);
        assert_eq!(listener.local_addr().unwrap().ip(), Ipv4Addr::LOCALHOST);
    }

    #[test]
    fn auto_forward_policy() {
        let cfg = PortConfig::from_devcontainer_json(
            r#"{ "portsAttributes": { "3000": { "onAutoForward": "ignore" } } }"#,
        )
        .unwrap();
        assert!(!should_auto_forward(3000, &cfg));
        assert!(should_auto_forward(8080, &cfg));
        assert!(!should_auto_forward(44805, &cfg));
    }
}
