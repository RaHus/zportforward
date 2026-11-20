mod control;
mod daemon;
mod discovery;
mod forwards;
mod mcp;
mod runtime;
mod tunnel;
mod web;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use control::{Client, ContainerInfo, Request, Response};
use discovery::Target;
use runtime::Runtime;

/// Log to stderr with a common prefix.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { eprintln!("devfwd: {}", format!($($arg)*)) };
}

#[derive(Parser)]
#[command(version, about = "On-the-fly port forwarding for dev containers")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List dev containers, their forwarded ports and other listening ports
    Ls {
        /// Print the daemon's JSON reply
        #[arg(long)]
        json: bool,
    },
    /// Forward a container port to 127.0.0.1 on the host
    Add {
        /// PORT, or PORT:HOST_PORT to choose the host port
        port: String,
        /// Container name or ID (default: the dev container for this directory)
        #[arg(short, long)]
        container: Option<String>,
        /// Forward to this host as seen from inside the container (e.g. a compose service)
        #[arg(long)]
        host: Option<String>,
    },
    /// Stop forwarding a container port
    Rm {
        port: u16,
        #[arg(short, long)]
        container: Option<String>,
    },
    /// Open the ports page in the browser
    Ui {
        /// Only print the address
        #[arg(long)]
        no_open: bool,
    },
    /// Stop the background daemon and close all forwards
    Stop,
    /// Run the daemon in the foreground (normally started on demand)
    Daemon {
        /// Also forward newly detected listening ports below 32768
        #[arg(long)]
        auto: bool,
        /// Started by a client: exit quietly if a daemon is already there
        #[arg(long, hide = true)]
        on_demand: bool,
    },
    /// Serve the Model Context Protocol on stdio (for editor agents such as Zed's)
    Mcp,
    /// Forward one port in the foreground, without the daemon (Ctrl-C to stop)
    Fwd {
        /// PORT, or PORT:HOST_PORT to choose the host port
        port: String,
        /// Any container name or ID (default: the dev container for this directory)
        #[arg(short, long)]
        container: Option<String>,
    },
}

/// `3000` or `3000:8080` (container port, then host port).
fn parse_port_spec(spec: &str) -> Result<(u16, Option<u16>)> {
    let parse = |s: &str| s.parse::<u16>().with_context(|| format!("'{s}' is not a port number"));
    match spec.split_once(':') {
        Some((port, host_port)) => Ok((parse(port)?, Some(parse(host_port)?))),
        None => Ok((parse(spec)?, None)),
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("devfwd: error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let cwd = std::env::current_dir().ok();
    match cli.command {
        Cmd::Ls { json } => {
            let response = Client::connect_or_start().await?.request(&Request::List).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                print_list(&response);
            }
        }
        Cmd::Add { port, container, host } => {
            let (port, host_port) = parse_port_spec(&port)?;
            let request = Request::Add { container, cwd, port, host_port, host };
            let response = Client::connect_or_start().await?.request(&request).await?;
            println!("{}", response.message.unwrap_or_default());
        }
        Cmd::Rm { port, container } => {
            let request = Request::Remove { container, cwd, port };
            let response = Client::connect_or_start().await?.request(&request).await?;
            println!("{}", response.message.unwrap_or_default());
        }
        Cmd::Ui { no_open } => {
            let url = ports_page_url().await?;
            println!("{url}");
            if !no_open {
                open_browser(&url).context("could not open a browser; open the address above yourself")?;
            }
        }
        Cmd::Stop => match Client::connect().await {
            Ok(mut client) => {
                client.stop().await?;
                println!("daemon stopped");
            }
            Err(_) => println!("daemon is not running"),
        },
        Cmd::Daemon { auto, on_demand } => daemon::run(auto, on_demand).await?,
        Cmd::Mcp => mcp::run().await?,
        Cmd::Fwd { port, container } => fwd(&port, container.as_deref()).await?,
    }
    Ok(())
}

/// The address of the daemon's ports page, starting the daemon if needed.
pub async fn ports_page_url() -> Result<String> {
    let response = Client::connect_or_start().await?.request(&Request::List).await?;
    response.daemon.ui_url.context("the ports page is turned off (DEVFWD_UI_PORT=0) or could not bind a port")
}

pub fn open_browser(url: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/c", "start", ""]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = Command::new("xdg-open");
    command.arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(drop)
}

fn print_list(response: &Response) {
    if response.containers.is_empty() {
        println!("no running dev containers");
        return;
    }
    for (i, c) in response.containers.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_container(c);
    }
}

fn print_container(c: &ContainerInfo) {
    let folder = c.folder.as_ref().map(|f| format!("  {}", f.display())).unwrap_or_default();
    let link = match (c.running, c.connected) {
        (false, _) => "  [stopped]",
        (true, false) => "  [connecting]",
        (true, true) => "",
    };
    println!("{}{folder}{link}", c.name);
    if c.forwards.is_empty() {
        println!("  no forwarded ports");
    } else {
        println!("  {:<18} {:<18} {:<6} {:<9} LABEL", "CONTAINER", "HOST", "STATE", "ORIGIN");
    }
    for f in &c.forwards {
        let target = if f.target_host.is_empty() {
            f.port.to_string()
        } else {
            format!("{}:{}", f.target_host, f.port)
        };
        let host = f.host_port.map(|p| format!("127.0.0.1:{p}")).unwrap_or_else(|| "-".into());
        // up: something is listening; idle: forward is ready but nothing listens yet.
        let state = match (&f.error, f.listening) {
            (Some(_), _) => "error",
            (None, true) => "up",
            (None, false) => "idle",
        };
        let mut note = f.label.clone().unwrap_or_default();
        if let Some(error) = &f.error {
            note = if note.is_empty() { format!("({error})") } else { format!("{note}  ({error})") };
        }
        println!("  {target:<18} {host:<18} {state:<6} {:<9} {note}", f.origin);
    }
    if !c.detected.is_empty() {
        let ports: Vec<String> = c
            .detected
            .iter()
            .filter(|d| d.port < forwards::EPHEMERAL_START)
            .map(|d| d.port.to_string())
            .collect();
        let ephemeral = c.detected.len() - ports.len();
        let mut line = format!("  also listening, not forwarded: {}", ports.join(" "));
        if ephemeral > 0 {
            line.push_str(&format!("{}{ephemeral} ephemeral", if ports.is_empty() { "" } else { " + " }));
        }
        println!("{line}");
    }
}

async fn fwd(spec: &str, reference: Option<&str>) -> Result<()> {
    let (port, host_port) = parse_port_spec(spec)?;
    let rt = Runtime::detect().await?;
    let containers = match reference {
        // An explicit reference may be any container, not only a labelled dev container.
        Some(reference) => discovery::inspect(&rt, &[reference]).await?,
        None => discovery::list(&rt).await?,
    };
    let cwd = std::env::current_dir().ok();
    let container = match reference {
        Some(_) => containers.first().context("container not found")?,
        None => discovery::select(&containers, None, cwd.as_deref())?,
    };

    let (listener, bound) = match forwards::bind(host_port.unwrap_or(port), host_port.is_some()).await {
        Ok(ok) => ok,
        Err(reason) => bail!("{reason}"),
    };
    let (slot, supervisor) = tunnel::supervise(rt, container.id.clone(), Default::default());
    let task = forwards::spawn(listener, slot, Target { host: String::new(), port });
    log!("{} port {port} -> http://127.0.0.1:{bound}  (Ctrl-C to stop)", container.name);
    tokio::signal::ctrl_c().await?;
    task.abort();
    supervisor.abort();
    Ok(())
}
