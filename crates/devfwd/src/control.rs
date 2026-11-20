//! Control socket between the daemon and its clients (CLI, MCP front).
//! One JSON request per line, one JSON response per line.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use interprocess::local_socket::tokio::{prelude::*, Listener, Stream};
use interprocess::local_socket::{ListenerOptions, Name};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    List,
    Add {
        container: Option<String>,
        cwd: Option<PathBuf>,
        port: u16,
        host_port: Option<u16>,
        /// Host as seen from inside the container; `None` for its loopback.
        host: Option<String>,
    },
    Remove {
        container: Option<String>,
        cwd: Option<PathBuf>,
        port: u16,
    },
    /// Turn auto-forwarding of detected ports on or off.
    SetAuto {
        on: bool,
    },
    Stop,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Response {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub daemon: DaemonInfo,
    #[serde(default)]
    pub containers: Vec<ContainerInfo>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub version: String,
    pub auto: bool,
    /// Address of the ports page, when the daemon serves one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub folder: Option<PathBuf>,
    #[serde(default = "yes")]
    pub running: bool,
    /// Whether the tunnel into the container is currently up.
    pub connected: bool,
    pub forwards: Vec<ForwardInfo>,
    /// Ports listening in the container that are not forwarded.
    pub detected: Vec<DetectedInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ForwardInfo {
    /// Port inside the container.
    pub port: u16,
    /// Non-empty when the target is another host reachable from the container.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target_host: String,
    /// Bound port on the host's 127.0.0.1; `None` when binding failed.
    pub host_port: Option<u16>,
    /// `declared` (forwardPorts), `manual` or `auto`.
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Whether something in the container is listening on the port right now.
    pub listening: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DetectedInfo {
    pub port: u16,
    /// Bound to the container's loopback only.
    pub loopback: bool,
}

fn yes() -> bool {
    true
}

#[cfg(unix)]
fn socket_name() -> Result<Name<'static>> {
    use interprocess::local_socket::{GenericFilePath, ToFsName};
    let path = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("devfwd.sock"),
        None => {
            let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
            std::env::temp_dir().join(format!("devfwd-{user}.sock"))
        }
    };
    Ok(path.to_fs_name::<GenericFilePath>()?)
}

#[cfg(windows)]
fn socket_name() -> Result<Name<'static>> {
    use interprocess::local_socket::{GenericNamespaced, ToNsName};
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    Ok(format!("devfwd-{user}").to_ns_name::<GenericNamespaced>()?)
}

/// Held by the daemon for its whole life, so that clients starting it at the
/// same moment cannot end up with two daemons fighting over the same ports.
pub fn lock_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| {
        log_path().parent().map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
    });
    dir.join("devfwd.lock")
}

pub fn listen() -> Result<Listener> {
    ListenerOptions::new()
        .name(socket_name()?)
        // Replace a socket file left behind by a crashed daemon.
        .try_overwrite(true)
        .create_tokio()
        .context("creating the control socket")
}

pub struct Client {
    conn: BufReader<Stream>,
}

impl Client {
    /// Connect to a running daemon.
    pub async fn connect() -> Result<Client> {
        let conn = Stream::connect(socket_name()?).await?;
        Ok(Client { conn: BufReader::new(conn) })
    }

    /// Connect, starting the daemon in the background if it is not running.
    pub async fn connect_or_start() -> Result<Client> {
        if let Ok(client) = Self::connect().await {
            return Ok(client);
        }
        spawn_daemon()?;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(client) = Self::connect().await {
                return Ok(client);
            }
        }
        bail!("the daemon did not come up; see {}", log_path().display())
    }

    /// Ask the daemon to exit. It may go away before its reply arrives, so a
    /// closed connection counts as success.
    pub async fn stop(&mut self) -> Result<()> {
        let mut reply = String::new();
        self.conn.get_mut().write_all(b"{\"op\":\"stop\"}\n").await?;
        self.conn.get_mut().flush().await?;
        let _ = self.conn.read_line(&mut reply).await;
        // Wait until the socket is really gone, so a following start cannot race.
        for _ in 0..50 {
            if Self::connect().await.is_err() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        bail!("the daemon did not stop")
    }

    pub async fn request(&mut self, req: &Request) -> Result<Response> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        self.conn.get_mut().write_all(line.as_bytes()).await?;
        self.conn.get_mut().flush().await?;
        let mut reply = String::new();
        if self.conn.read_line(&mut reply).await? == 0 {
            bail!("the daemon closed the connection");
        }
        let response: Response = serde_json::from_str(&reply).context("bad reply from daemon")?;
        match response.error {
            Some(error) => Err(anyhow!(error)),
            None => Ok(response),
        }
    }
}

pub fn log_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("devfwd").join("daemon.log")
}

fn spawn_daemon() -> Result<()> {
    let log = log_path();
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let log_file = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(["daemon", "--on-demand"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(log_file);
    if std::env::var_os("DEVFWD_AUTO").is_some() {
        cmd.arg("--auto");
    }
    #[cfg(unix)]
    {
        // Own process group: a Ctrl-C in the starting terminal must not stop it.
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = cmd.spawn().context("starting the daemon")?;
    // A long-lived client (the MCP server) would otherwise keep the daemon
    // as a zombie after it exits.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
