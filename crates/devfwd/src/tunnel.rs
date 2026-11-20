//! The exec session to one container: installs the agent, starts it, and
//! exposes stream dialing plus the agent's view of listening ports.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use devfwd_proto::{self as proto, ControlMsg, Listener, Mode, Opener, Stream};
use futures::AsyncReadExt;
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

use crate::discovery::Target;
use crate::runtime::Runtime;

mod agents {
    include!(concat!(env!("OUT_DIR"), "/agents.rs"));
}

/// Where the agent may be placed inside the container, in order of preference.
/// `$HOME` is expanded by the container's shell.
const INSTALL_DIRS: [&str; 2] = ["/tmp/.devfwd", "$HOME/.cache/devfwd"];
const HELLO_TIMEOUT: Duration = Duration::from_secs(20);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(3);

type Listeners = BTreeSet<Listener>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Listeners>>>>;

pub struct Tunnel {
    opener: Opener,
    /// Last report from the agent; `None` until the first one arrives.
    listeners: watch::Receiver<Option<Listeners>>,
    requests: mpsc::Sender<ControlMsg>,
    pending: Pending,
    next_id: AtomicU64,
    _child: Child,
}

impl Tunnel {
    /// With `watch`, the agent keeps reporting changes to the listening
    /// ports; without it the agent scans only when [`Tunnel::snapshot`] asks.
    pub async fn connect(rt: &Runtime, container_id: &str, watch: bool) -> Result<Tunnel> {
        let agent = install(rt, container_id).await.context("installing the agent")?;
        let mut child = rt
            .command()
            .args(["exec", "-i", container_id, &agent, "serve"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("starting the agent")?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let (opener, mut inbound) = proto::session(tokio::io::join(stdout, stdin).compat(), Mode::Client);

        // The agent's first (and only) stream is the control channel.
        let hello = async {
            let mut ctl = inbound.recv().await.ok_or_else(|| anyhow!("agent exited during startup"))?;
            match proto::read_msg(&mut ctl).await? {
                Some(ControlMsg::Hello { proto: version, .. }) if version == proto::PROTO_VERSION => Ok(ctl),
                Some(ControlMsg::Hello { proto: version, .. }) => {
                    bail!("agent speaks protocol {version}, expected {}", proto::PROTO_VERSION)
                }
                other => bail!("unexpected first message from agent: {other:?}"),
            }
        };
        let ctl = tokio::time::timeout(HELLO_TIMEOUT, hello)
            .await
            .map_err(|_| anyhow!("agent did not answer within {HELLO_TIMEOUT:?}"))??;

        let (mut reader, mut writer) = ctl.split();
        let (cache, listeners) = watch::channel(None);
        let (requests, mut outbox) = mpsc::channel::<ControlMsg>(16);
        let pending = Pending::default();

        tokio::spawn(async move {
            while let Some(msg) = outbox.recv().await {
                if proto::write_msg(&mut writer, &msg).await.is_err() {
                    break;
                }
            }
        });
        let waiting = pending.clone();
        tokio::spawn(async move {
            while let Ok(Some(msg)) = proto::read_msg(&mut reader).await {
                let ControlMsg::Listeners { reply_to, listeners } = msg else { continue };
                let set = Listeners::from_iter(listeners);
                if let Some(reply) = reply_to.and_then(|id| waiting.lock().unwrap().remove(&id)) {
                    let _ = reply.send(set.clone());
                }
                // Wake watchers only when something actually changed.
                cache.send_if_modified(|current| {
                    let changed = current.as_ref() != Some(&set);
                    if changed {
                        *current = Some(set);
                    }
                    changed
                });
            }
            // Dropping the senders fails any snapshot still waiting.
            waiting.lock().unwrap().clear();
        });

        if watch {
            requests.send(ControlMsg::Watch { on: true }).await.map_err(|_| anyhow!("tunnel closed"))?;
        }
        Ok(Tunnel { opener, listeners, requests, pending, next_id: AtomicU64::new(1), _child: child })
    }

    /// Open a stream connected to `target` as seen from inside the container.
    pub async fn dial(&self, target: &Target) -> Result<Compat<Stream>> {
        let mut stream = self.opener.open().await?;
        proto::write_dial(&mut stream, &target.host, target.port).await?;
        Ok(stream.compat())
    }

    /// Ask the agent which TCP ports are listening right now.
    pub async fn snapshot(&self) -> Result<Listeners> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let answer = async {
            self.requests.send(ControlMsg::Query { id }).await.map_err(|_| anyhow!("tunnel closed"))?;
            rx.await.map_err(|_| anyhow!("tunnel closed"))
        };
        let result = tokio::time::timeout(SNAPSHOT_TIMEOUT, answer)
            .await
            .map_err(|_| anyhow!("agent did not answer within {SNAPSHOT_TIMEOUT:?}"))
            .and_then(|r| r);
        if result.is_err() {
            self.pending.lock().unwrap().remove(&id);
        }
        result
    }

    /// Turn the agent's change reports on or off.
    pub async fn set_watch(&self, on: bool) {
        let _ = self.requests.send(ControlMsg::Watch { on }).await;
    }

    /// The agent's reports, for following changes. Only updated on its own
    /// when the tunnel was connected with `watch`.
    pub fn watch_listeners(&self) -> watch::Receiver<Option<Listeners>> {
        self.listeners.clone()
    }

    pub async fn closed(&self) {
        self.opener.closed().await
    }
}

/// Make sure the right agent binary is in the container; returns its path there.
async fn install(rt: &Runtime, cid: &str) -> Result<String> {
    let arch = rt.output(&["exec", cid, "uname", "-m"]).await?;
    let bytes: Cow<'static, [u8]> = match std::env::var_os("DEVFWD_AGENT_PATH") {
        Some(path) => Cow::Owned(std::fs::read(&path).with_context(|| format!("reading {path:?}"))?),
        None => Cow::Borrowed(match arch.as_str() {
            "x86_64" | "amd64" => agents::X86_64,
            "aarch64" | "arm64" => agents::AARCH64,
            other => bail!("unsupported container architecture '{other}'"),
        }),
    };
    if bytes.is_empty() {
        bail!("this build has no agent for {arch}; set DEVFWD_AGENT_PATH to a devfwd-agent binary");
    }
    // Content-addressed so upgrades never run a stale agent and never re-upload.
    let name = format!("agent-{:016x}", fnv1a(&bytes));

    let dirs = INSTALL_DIRS.map(|d| format!("\"{d}\"")).join(" ");
    let probe = format!(
        r#"for d in {dirs}; do p="$d/{name}"; if [ -x "$p" ] && "$p" --version >/dev/null 2>&1; then echo "$p"; exit 0; fi; done; exit 1"#
    );
    if let Ok(path) = rt.output(&["exec", cid, "sh", "-c", &probe]).await {
        if !path.is_empty() {
            return Ok(path);
        }
    }

    let mut last_err = None;
    for dir in INSTALL_DIRS {
        // Write to a temp name and rename, so a concurrent start never sees half a file.
        // The final `--version` run catches `noexec` mounts.
        let upload = format!(
            r#"d="{dir}"; p="$d/{name}"; mkdir -p "$d" && cat > "$p.$$" && chmod 755 "$p.$$" && mv -f "$p.$$" "$p" && "$p" --version >/dev/null && echo "$p""#
        );
        match rt.output_with_stdin(&["exec", "-i", cid, "sh", "-c", &upload], Some(&bytes)).await {
            Ok(path) if !path.is_empty() => return Ok(path),
            Ok(_) => last_err = Some(anyhow!("upload to {dir} produced no path")),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.expect("at least one install dir"))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

/// The current tunnel to a container, `None` while (re)connecting.
pub type Slot = watch::Receiver<Option<Arc<Tunnel>>>;

/// Keep a tunnel to `container_id` alive, reconnecting with backoff.
/// Abort the returned task to stop.
/// `watch` is read at every (re)connect, so the caller can flip it later.
pub fn supervise(rt: Runtime, container_id: String, watch: Arc<AtomicBool>) -> (Slot, JoinHandle<()>) {
    let (tx, slot) = watch::channel(None);
    let task = tokio::spawn(async move {
        let mut backoff = Duration::from_millis(500);
        let short = &container_id[..container_id.len().min(12)];
        // Retries repeat the same failure (e.g. container stopped): log it once.
        let mut last_error = String::new();
        loop {
            match Tunnel::connect(&rt, &container_id, watch.load(Ordering::Relaxed)).await {
                Ok(tunnel) => {
                    backoff = Duration::from_millis(500);
                    last_error.clear();
                    let tunnel = Arc::new(tunnel);
                    if tx.send(Some(tunnel.clone())).is_err() {
                        return;
                    }
                    tunnel.closed().await;
                    crate::log!("tunnel to {short} closed, reconnecting");
                    if tx.send(None).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let error = format!("{e:#}");
                    if error != last_error {
                        crate::log!("tunnel to {short} failed: {error}");
                        last_error = error;
                    }
                    if tx.is_closed() {
                        return;
                    }
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    });
    (slot, task)
}
