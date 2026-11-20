//! In-container half of zportforward. Started by the host as
//! `docker exec -i <container> <agent> serve`; speaks yamux on stdio.

mod procnet;
mod scan;
#[cfg(target_os = "linux")]
mod sockdiag;

use std::collections::BTreeSet;
use std::time::Instant;

use anyhow::Result;
use zportforward_proto::{self as proto, ControlMsg, Listener, Mode, Opener, Stream};
use futures::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("serve") => {}
        Some("scan") => return print_scan(),
        Some("--version") => {
            println!("zportforward-agent {VERSION} proto {}", proto::PROTO_VERSION);
            return;
        }
        _ => {
            eprintln!("zportforward-agent is started by zportforward; usage: zportforward-agent serve | scan");
            std::process::exit(2);
        }
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(serve());
    // tokio's stdin reader thread would otherwise keep the process alive.
    std::process::exit(0);
}

async fn serve() {
    let io = tokio::io::join(tokio::io::stdin(), tokio::io::stdout()).compat();
    let (opener, mut inbound) = proto::session(io, Mode::Server);

    tokio::spawn(control(opener.clone()));

    // Ends when the host closes stdin or the session breaks.
    while let Some(stream) = inbound.recv().await {
        tokio::spawn(async move {
            let _ = handle_dial(stream).await;
        });
    }
}

async fn handle_dial(mut stream: Stream) -> Result<()> {
    let (host, port) = proto::read_dial(&mut stream).await?;
    let tcp = if host.is_empty() {
        // Try IPv4 loopback first, then IPv6: dev servers bind either.
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(tcp) => Ok(tcp),
            Err(v4) => TcpStream::connect(("::1", port)).await.map_err(|_| v4),
        }
    } else {
        TcpStream::connect((host.as_str(), port)).await
    };
    match tcp {
        Ok(mut tcp) => {
            let _ = tcp.set_nodelay(true);
            proto::write_dial_reply(&mut stream, Ok(())).await?;
            let mut stream = stream.compat();
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await;
        }
        Err(e) => {
            proto::write_dial_reply(&mut stream, Err(&format!("port {port}: {e}"))).await?;
        }
    }
    Ok(())
}

/// The control stream: answers `Query`, and while `Watch` is on, reports the
/// listening ports whenever they change. Nothing is scanned otherwise.
async fn control(opener: Opener) -> Result<()> {
    let (mut reader, mut writer) = opener.open().await?.split();
    proto::write_msg(
        &mut writer,
        &ControlMsg::Hello { proto: proto::PROTO_VERSION, agent_version: VERSION.into() },
    )
    .await?;

    // `read_msg` must not be cancelled half way, so it gets its own task.
    let (tx, mut requests) = mpsc::channel(16);
    tokio::spawn(async move {
        while let Ok(Some(msg)) = proto::read_msg(&mut reader).await {
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    let report = |reply_to, set: &BTreeSet<Listener>| ControlMsg::Listeners {
        reply_to,
        listeners: set.iter().copied().collect(),
    };
    let mut watching = false;
    let mut known = BTreeSet::new();
    let mut last_change = Instant::now();
    loop {
        let next_scan = async {
            if watching {
                tokio::time::sleep(scan::poll_interval(last_change.elapsed())).await
            } else {
                std::future::pending().await
            }
        };
        tokio::select! {
            request = requests.recv() => match request {
                None => return Ok(()),
                Some(ControlMsg::Query { id }) => {
                    let now = scan::listeners();
                    if now != known {
                        known = now;
                        last_change = Instant::now();
                    }
                    proto::write_msg(&mut writer, &report(Some(id), &known)).await?;
                }
                Some(ControlMsg::Watch { on }) => {
                    watching = on;
                    if on {
                        known = scan::listeners();
                        last_change = Instant::now();
                        proto::write_msg(&mut writer, &report(None, &known)).await?;
                    }
                }
                Some(_) => {}
            },
            _ = next_scan => {
                let now = scan::listeners();
                if now != known {
                    known = now;
                    last_change = Instant::now();
                    proto::write_msg(&mut writer, &report(None, &known)).await?;
                }
            }
        }
    }
}

/// `zportforward-agent scan`: print what a scan finds and what each method costs.
fn print_scan() {
    for l in scan::listeners() {
        println!("{:>5}  {}", l.port, if l.loopback { "loopback only" } else { "all interfaces" });
    }
    let time = |f: &dyn Fn() -> bool| {
        const RUNS: u32 = 200;
        let start = Instant::now();
        let ok = (0..RUNS).all(|_| f());
        ok.then(|| start.elapsed() / RUNS)
    };
    #[cfg(target_os = "linux")]
    match time(&|| sockdiag::listeners().is_ok()) {
        Some(cost) => println!("sock_diag query: {cost:?} per scan"),
        None => println!("sock_diag query: unavailable, using /proc/net/tcp"),
    }
    if let Some(cost) = time(&|| {
        procnet::listeners();
        true
    }) {
        println!("/proc/net/tcp:   {cost:?} per scan");
    }
}
