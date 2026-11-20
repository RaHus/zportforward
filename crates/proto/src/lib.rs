//! Wire protocol shared by the host (`zportforward`) and the in-container agent.
//!
//! One yamux connection runs over the stdio of `docker exec -i`. The host is
//! the yamux client, the agent the server.
//!
//! * The agent opens exactly one stream towards the host: the control stream,
//!   carrying length-prefixed JSON [`ControlMsg`]s in both directions. The
//!   agent reports listening ports only when asked (`Query`) or while the
//!   host has turned watching on (`Watch`).
//! * The host opens one stream per forwarded TCP connection. It starts with a
//!   dial header (see [`write_dial`]); after the agent's reply the stream
//!   carries raw bytes.

use std::collections::VecDeque;
use std::future::poll_fn;
use std::task::Poll;

use anyhow::{anyhow, bail, Context, Result};
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

pub use yamux::{Mode, Stream};

/// Bumped on any incompatible change to the wire format.
pub const PROTO_VERSION: u32 = 2;

const MAX_MSG: usize = 1 << 20;

/// A TCP socket in LISTEN state inside the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Listener {
    pub port: u16,
    /// Bound to a loopback address only (not reachable via the container IP).
    pub loopback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMsg {
    /// Agent to host: first message on the control stream.
    Hello { proto: u32, agent_version: String },
    /// Host to agent: start or stop reporting changes to the listening ports.
    Watch { on: bool },
    /// Host to agent: report the listening ports once, now.
    Query { id: u64 },
    /// Agent to host: every listening port. `reply_to` names the `Query`
    /// being answered; it is `None` for a report sent because of `Watch`.
    Listeners { reply_to: Option<u64>, listeners: Vec<Listener> },
}

pub async fn write_msg<W: AsyncWrite + Unpin>(w: &mut W, msg: &ControlMsg) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Returns `Ok(None)` on a clean end of stream.
pub async fn read_msg<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<ControlMsg>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_MSG {
        bail!("control message too large ({len} bytes)");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body).context("bad control message")?))
}

/// Host side: ask the agent to connect this stream to `host:port` as seen
/// from inside the container. An empty `host` means the container's loopback.
pub async fn write_dial<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
    host: &str,
    port: u16,
) -> Result<()> {
    if host.len() > u8::MAX as usize {
        bail!("host name too long");
    }
    let mut header = Vec::with_capacity(3 + host.len());
    header.extend_from_slice(&port.to_be_bytes());
    header.push(host.len() as u8);
    header.extend_from_slice(host.as_bytes());
    s.write_all(&header).await?;
    s.flush().await?;
    let mut status = [0u8; 1];
    s.read_exact(&mut status).await.context("agent closed the stream")?;
    if status[0] == 0 {
        return Ok(());
    }
    let mut len = [0u8; 2];
    s.read_exact(&mut len).await?;
    let mut msg = vec![0u8; u16::from_be_bytes(len) as usize];
    s.read_exact(&mut msg).await?;
    Err(anyhow!("{}", String::from_utf8_lossy(&msg)))
}

/// Agent side: read the target the host wants.
pub async fn read_dial<S: AsyncRead + Unpin>(s: &mut S) -> Result<(String, u16)> {
    let mut head = [0u8; 3];
    s.read_exact(&mut head).await?;
    let mut host = vec![0u8; head[2] as usize];
    s.read_exact(&mut host).await?;
    Ok((String::from_utf8(host).context("bad host name")?, u16::from_be_bytes([head[0], head[1]])))
}

/// Agent side: report whether the dial worked.
pub async fn write_dial_reply<S: AsyncWrite + Unpin>(
    s: &mut S,
    result: std::result::Result<(), &str>,
) -> Result<()> {
    match result {
        Ok(()) => s.write_all(&[0]).await?,
        Err(msg) => {
            let msg = &msg.as_bytes()[..msg.len().min(u16::MAX as usize)];
            s.write_all(&[1]).await?;
            s.write_all(&(msg.len() as u16).to_be_bytes()).await?;
            s.write_all(msg).await?;
        }
    }
    s.flush().await?;
    Ok(())
}

/// Handle for opening outbound streams on a running [`session`].
#[derive(Clone)]
pub struct Opener {
    tx: mpsc::Sender<oneshot::Sender<Result<Stream>>>,
}

impl Opener {
    pub async fn open(&self) -> Result<Stream> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(tx).await.map_err(|_| anyhow!("tunnel closed"))?;
        rx.await.map_err(|_| anyhow!("tunnel closed"))?
    }

    /// Resolves once the session has ended.
    pub async fn closed(&self) {
        self.tx.closed().await
    }
}

/// Start a yamux session over `io`. Must be called inside a tokio runtime.
///
/// Returns a handle for opening streams and a receiver of streams opened by
/// the peer. The receiver yields `None` when the session ends.
pub fn session<T>(io: T, mode: Mode) -> (Opener, mpsc::Receiver<Stream>)
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut cfg = yamux::Config::default();
    cfg.set_max_num_streams(4096);
    let mut conn = yamux::Connection::new(io, cfg, mode);
    let (open_tx, mut open_rx) = mpsc::channel::<oneshot::Sender<Result<Stream>>>(64);
    let (in_tx, in_rx) = mpsc::channel::<Stream>(64);

    tokio::spawn(async move {
        let mut pending: VecDeque<oneshot::Sender<Result<Stream>>> = VecDeque::new();
        let mut requests_open = true;
        poll_fn(|cx| {
            while requests_open {
                match open_rx.poll_recv(cx) {
                    Poll::Ready(Some(reply)) => pending.push_back(reply),
                    Poll::Ready(None) => requests_open = false,
                    Poll::Pending => break,
                }
            }
            while !pending.is_empty() {
                match conn.poll_new_outbound(cx) {
                    Poll::Ready(result) => {
                        let reply = pending.pop_front().expect("checked non-empty");
                        let _ = reply.send(result.map_err(Into::into));
                    }
                    Poll::Pending => break,
                }
            }
            loop {
                match conn.poll_next_inbound(cx) {
                    Poll::Ready(Some(Ok(stream))) => {
                        // Dropping the stream resets it if the consumer is gone or swamped.
                        let _ = in_tx.try_send(stream);
                    }
                    Poll::Ready(Some(Err(_))) | Poll::Ready(None) => return Poll::Ready(()),
                    Poll::Pending => return Poll::Pending,
                }
            }
        })
        .await;
        for reply in pending {
            let _ = reply.send(Err(anyhow!("tunnel closed")));
        }
    });

    (Opener { tx: open_tx }, in_rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

    /// A miniature agent: dials 127.0.0.1:port for every inbound stream.
    fn spawn_agent(io: tokio::io::DuplexStream) {
        let (_opener, mut inbound) = session(io.compat(), Mode::Server);
        tokio::spawn(async move {
            let _keep = _opener;
            while let Some(mut stream) = inbound.recv().await {
                tokio::spawn(async move {
                    let (_host, port) = read_dial(&mut stream).await.unwrap();
                    match TcpStream::connect(("127.0.0.1", port)).await {
                        Ok(mut tcp) => {
                            write_dial_reply(&mut stream, Ok(())).await.unwrap();
                            let mut stream = stream.compat();
                            let _ = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await;
                        }
                        Err(e) => {
                            let _ = write_dial_reply(&mut stream, Err(&e.to_string())).await;
                        }
                    }
                });
            }
        });
    }

    async fn echo_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let (mut r, mut w) = sock.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                    let _ = w.shutdown().await;
                });
            }
        });
        port
    }

    fn pair() -> Opener {
        let (a, b) = tokio::io::duplex(64 * 1024);
        spawn_agent(b);
        let (opener, _inbound) = session(a.compat(), Mode::Client);
        opener
    }

    #[tokio::test]
    async fn echo_with_half_close() {
        let port = echo_server().await;
        let opener = pair();
        let mut s = opener.open().await.unwrap();
        write_dial(&mut s, "", port).await.unwrap();
        let mut s = s.compat();
        s.write_all(b"hello").await.unwrap();
        s.shutdown().await.unwrap(); // half-close: the echo must still come back
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"hello");
    }

    #[tokio::test]
    async fn large_transfer() {
        let port = echo_server().await;
        let opener = pair();
        let mut s = opener.open().await.unwrap();
        write_dial(&mut s, "", port).await.unwrap();
        let (mut r, mut w) = tokio::io::split(s.compat());
        const TOTAL: usize = 100 * 1024 * 1024;
        let writer = tokio::spawn(async move {
            let chunk = vec![0xabu8; 64 * 1024];
            let mut sent = 0;
            while sent < TOTAL {
                w.write_all(&chunk).await.unwrap();
                sent += chunk.len();
            }
            w.shutdown().await.unwrap();
        });
        let mut got = 0usize;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = r.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            assert!(buf[..n].iter().all(|b| *b == 0xab));
            got += n;
        }
        writer.await.unwrap();
        assert_eq!(got, TOTAL);
    }

    #[tokio::test]
    async fn many_concurrent_streams() {
        let port = echo_server().await;
        let opener = pair();
        let mut tasks = Vec::new();
        for i in 0..200u32 {
            let opener = opener.clone();
            tasks.push(tokio::spawn(async move {
                let mut s = opener.open().await.unwrap();
                write_dial(&mut s, "", port).await.unwrap();
                let mut s = s.compat();
                let msg = format!("stream-{i}");
                s.write_all(msg.as_bytes()).await.unwrap();
                s.shutdown().await.unwrap();
                let mut out = String::new();
                s.read_to_string(&mut out).await.unwrap();
                assert_eq!(out, msg);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
    }

    #[tokio::test]
    async fn dial_failure_is_reported() {
        // Bind then drop to get a port nothing listens on.
        let port = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
        let opener = pair();
        let mut s = opener.open().await.unwrap();
        let err = write_dial(&mut s, "", port).await.unwrap_err().to_string();
        assert!(err.to_lowercase().contains("refused"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn control_messages_round_trip() {
        let (a, b) = tokio::io::duplex(1024);
        let (mut a, mut b) = (a.compat(), b.compat());
        let msg = ControlMsg::Listeners {
            reply_to: Some(7),
            listeners: vec![Listener { port: 3000, loopback: true }],
        };
        write_msg(&mut a, &msg).await.unwrap();
        drop(a);
        assert_eq!(read_msg(&mut b).await.unwrap(), Some(msg));
        assert_eq!(read_msg(&mut b).await.unwrap(), None);
    }
}
