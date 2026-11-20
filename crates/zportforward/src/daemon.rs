//! The singleton daemon: owns every host listener, follows dev containers
//! as they start and stop, and serves the control socket.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Result};
use zportforward_proto::Listener;
use interprocess::local_socket::tokio::{prelude::*, Stream};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::control::{self, ContainerInfo, DaemonInfo, DetectedInfo, ForwardInfo, Request, Response};
use crate::discovery::{self, Container, PortConfig, Target};
use crate::forwards;
use crate::web;
use crate::runtime::Runtime;
use crate::tunnel::{self, Slot, Tunnel};

const RESCAN_EVERY: Duration = Duration::from_secs(5);
/// A stopped container keeps its host ports this long, so a restart is seamless.
const STOPPED_GRACE: Duration = Duration::from_secs(120);
/// Exit after this long with no dev containers and no client requests.
const IDLE_EXIT: Duration = Duration::from_secs(600);

pub(crate) enum Event {
    Rescan,
    Control(Request, oneshot::Sender<Response>),
    /// The set of listening ports in this container changed.
    Detected(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Declared,
    Manual,
    Auto,
}

impl Origin {
    fn as_str(self) -> &'static str {
        match self {
            Origin::Declared => "declared",
            Origin::Manual => "manual",
            Origin::Auto => "auto",
        }
    }
}

enum State {
    Active { host_port: u16, task: JoinHandle<()> },
    Error(String),
}

struct Forward {
    origin: Origin,
    state: State,
    /// Host port the user asked for explicitly, kept for retries.
    wanted_host_port: Option<u16>,
}

impl Drop for Forward {
    fn drop(&mut self) {
        if let State::Active { task, .. } = &self.state {
            task.abort(); // closes the host listener
        }
    }
}

struct Managed {
    container: Container,
    config: PortConfig,
    config_mtime: Option<SystemTime>,
    slot: Slot,
    supervisor: JoinHandle<()>,
    watcher: JoinHandle<()>,
    forwards: BTreeMap<Target, Forward>,
    /// Ports the user removed; not brought back automatically.
    suppressed: BTreeSet<Target>,
    /// Set while the container exists but is not running.
    stopped_since: Option<Instant>,
}

impl Drop for Managed {
    fn drop(&mut self) {
        self.supervisor.abort();
        self.watcher.abort();
    }
}

impl Managed {
    /// Ask the agent what is listening right now; `None` without a tunnel.
    async fn listeners(&self) -> Option<BTreeSet<Listener>> {
        let tunnel = self.slot.borrow().clone()?;
        tunnel.snapshot().await.ok()
    }

    /// The agent's latest report, as kept up to date in auto mode.
    fn watched_listeners(&self) -> Option<BTreeSet<Listener>> {
        let tunnel = self.slot.borrow().clone()?;
        let latest = tunnel.watch_listeners().borrow().clone();
        latest
    }

    fn config_mtime(&self) -> Option<SystemTime> {
        let path = self.container.config_file.as_ref()?;
        std::fs::metadata(path).and_then(|m| m.modified()).ok()
    }

    async fn start(&mut self, target: Target, origin: Origin, host_port: Option<u16>) {
        let strict = host_port.is_some() || self.config.attrs(target.port).require_local_port;
        let state = match forwards::bind(host_port.unwrap_or(target.port), strict).await {
            Ok((listener, bound)) => State::Active {
                host_port: bound,
                task: forwards::spawn(listener, self.slot.clone(), target.clone()),
            },
            Err(reason) => State::Error(reason),
        };
        self.forwards.insert(target, Forward { origin, state, wanted_host_port: host_port });
    }

    /// Start `forwardPorts` entries that are not running yet and retry failed binds.
    async fn sync(&mut self) {
        let mtime = self.config_mtime();
        if mtime != self.config_mtime {
            self.config = self.container.port_config();
            self.config_mtime = mtime;
        }
        for target in self.config.forward_ports.clone() {
            if !self.forwards.contains_key(&target) && !self.suppressed.contains(&target) {
                self.start(target, Origin::Declared, None).await;
            }
        }
        let failed: Vec<(Target, Origin, Option<u16>)> = self
            .forwards
            .iter()
            .filter(|(_, f)| matches!(f.state, State::Error(_)))
            .map(|(t, f)| (t.clone(), f.origin, f.wanted_host_port))
            .collect();
        for (target, origin, host_port) in failed {
            self.start(target, origin, host_port).await;
        }
    }

    /// `--auto`: follow the listeners the agent reports.
    async fn apply_detected(&mut self) {
        let Some(listening) = self.watched_listeners() else { return };
        let ports: BTreeSet<u16> = listening.iter().map(|l| l.port).collect();
        for port in &ports {
            let target = Target { host: String::new(), port: *port };
            if !self.forwards.contains_key(&target)
                && !self.suppressed.contains(&target)
                && forwards::should_auto_forward(*port, &self.config)
            {
                self.start(target, Origin::Auto, None).await;
            }
        }
        self.forwards.retain(|t, f| f.origin != Origin::Auto || ports.contains(&t.port));
    }

    async fn info(&self) -> ContainerInfo {
        let listening = self.listeners().await;
        let is_listening = |port: u16| listening.as_ref().is_some_and(|set| set.iter().any(|l| l.port == port));
        let forwards = self
            .forwards
            .iter()
            .map(|(target, f)| ForwardInfo {
                port: target.port,
                target_host: target.host.clone(),
                host_port: match &f.state {
                    State::Active { host_port, .. } => Some(*host_port),
                    State::Error(_) => None,
                },
                origin: f.origin.as_str().to_string(),
                error: match &f.state {
                    State::Error(reason) => Some(reason.clone()),
                    State::Active { .. } => None,
                },
                label: self.config.label(target.port).map(str::to_string),
                // Remote targets cannot be observed from /proc; assume reachable.
                listening: !target.host.is_empty() || is_listening(target.port),
            })
            .collect();
        let forwarded_local: BTreeSet<u16> =
            self.forwards.keys().filter(|t| t.host.is_empty()).map(|t| t.port).collect();
        let detected = listening
            .unwrap_or_default()
            .into_iter()
            .filter(|l| !forwarded_local.contains(&l.port))
            .map(|l| DetectedInfo { port: l.port, loopback: l.loopback })
            .collect();
        ContainerInfo {
            id: self.container.id.clone(),
            name: self.container.name.clone(),
            folder: self.container.local_folder.clone(),
            running: self.stopped_since.is_none(),
            connected: self.slot.borrow().is_some(),
            forwards,
            detected,
        }
    }
}

struct Daemon {
    rt: Runtime,
    auto: bool,
    /// Mirrors `auto` for the tunnel supervisors, which read it on reconnect.
    watch: Arc<AtomicBool>,
    ui_url: Option<String>,
    events: mpsc::Sender<Event>,
    containers: BTreeMap<String, Managed>,
    last_active: Instant,
    last_rescan: Option<Instant>,
}

impl Daemon {
    /// Reconcile with the containers that are running now.
    async fn rescan(&mut self) {
        self.last_rescan = Some(Instant::now());
        let states = match discovery::list_states(&self.rt).await {
            Ok(states) => states,
            Err(e) => {
                crate::log!("listing containers failed: {e:#}");
                return;
            }
        };
        let states: BTreeMap<&str, bool> = states.iter().map(|(id, running)| (id.as_str(), *running)).collect();
        self.containers.retain(|id, m| match states.get(id.as_str()) {
            Some(true) => {
                if m.stopped_since.take().is_some() {
                    crate::log!("{} is running again", m.container.name);
                }
                true
            }
            Some(false) => {
                let since = *m.stopped_since.get_or_insert_with(|| {
                    crate::log!("{} stopped; holding its ports for {STOPPED_GRACE:?}", m.container.name);
                    Instant::now()
                });
                let keep = since.elapsed() < STOPPED_GRACE;
                if !keep {
                    crate::log!("{} stayed stopped; its forwards are closed", m.container.name);
                }
                keep
            }
            None => {
                crate::log!("{} was removed; its forwards are closed", m.container.name);
                false
            }
        });
        // Only running containers are picked up; old stopped ones must not claim ports.
        let new: Vec<&str> = states
            .iter()
            .filter(|(id, running)| **running && !self.containers.contains_key(**id))
            .map(|(id, _)| *id)
            .collect();
        match discovery::inspect(&self.rt, &new).await {
            Ok(found) => {
                for container in found {
                    self.adopt(container);
                }
            }
            Err(e) => crate::log!("inspecting containers failed: {e:#}"),
        }
        for m in self.containers.values_mut() {
            m.sync().await;
        }
        if !self.containers.is_empty() {
            self.last_active = Instant::now();
        }
    }

    fn adopt(&mut self, container: Container) {
        crate::log!("found dev container {} ({})", container.name, container.short_id());
        let (slot, supervisor) = tunnel::supervise(self.rt.clone(), container.id.clone(), self.watch.clone());
        let watcher = tokio::spawn(watch_detected(slot.clone(), container.id.clone(), self.events.clone()));
        let managed = Managed {
            config: PortConfig::default(),
            config_mtime: Some(SystemTime::UNIX_EPOCH), // forces the first load in sync()
            container,
            slot,
            supervisor,
            watcher,
            forwards: BTreeMap::new(),
            suppressed: BTreeSet::new(),
            stopped_since: None,
        };
        self.containers.insert(managed.container.id.clone(), managed);
    }

    fn select(&mut self, reference: Option<&str>, cwd: Option<&std::path::Path>) -> Result<&mut Managed> {
        let all: Vec<Container> = self.containers.values().map(|m| m.container.clone()).collect();
        let id = discovery::select(&all, reference, cwd)?.id.clone();
        Ok(self.containers.get_mut(&id).expect("selected from the same map"))
    }

    async fn handle(&mut self, req: Request) -> Response {
        self.last_active = Instant::now();
        // The ports page polls; do not shell out to the runtime for every request.
        if self.last_rescan.is_none_or(|at| at.elapsed() > Duration::from_secs(1)) {
            self.rescan().await;
        }
        let result = match req {
            Request::List | Request::Stop => Ok(None),
            Request::SetAuto { on } => Ok(Some(self.set_auto(on).await)),
            Request::Add { container, cwd, port, host_port, host } => {
                self.add(container.as_deref(), cwd.as_deref(), port, host_port, host).await.map(Some)
            }
            Request::Remove { container, cwd, port } => {
                self.remove(container.as_deref(), cwd.as_deref(), port).map(Some)
            }
        };
        let mut containers = Vec::with_capacity(self.containers.len());
        for m in self.containers.values() {
            containers.push(m.info().await);
        }
        let mut response = Response {
            daemon: DaemonInfo {
                pid: std::process::id(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                auto: self.auto,
                ui_url: self.ui_url.clone(),
            },
            containers,
            ..Default::default()
        };
        match result {
            Ok(message) => response.message = message,
            Err(e) => response.error = Some(format!("{e:#}")),
        }
        response
    }

    async fn set_auto(&mut self, on: bool) -> String {
        if self.auto != on {
            self.auto = on;
            self.watch.store(on, Ordering::Relaxed);
            for m in self.containers.values_mut() {
                let tunnel = m.slot.borrow().clone();
                if let Some(tunnel) = tunnel {
                    tunnel.set_watch(on).await;
                }
                if on {
                    m.apply_detected().await;
                } else {
                    m.forwards.retain(|_, f| f.origin != Origin::Auto);
                }
            }
            crate::log!("auto-forward turned {}", if on { "on" } else { "off" });
        }
        format!("auto-forward is {}", if on { "on" } else { "off" })
    }

    async fn add(
        &mut self,
        reference: Option<&str>,
        cwd: Option<&std::path::Path>,
        port: u16,
        host_port: Option<u16>,
        host: Option<String>,
    ) -> Result<String> {
        let m = self.select(reference, cwd)?;
        let target = Target { host: host.unwrap_or_default(), port };
        // Re-adding an active forward is a no-op unless a different host port is wanted.
        if let Some(Forward { state: State::Active { host_port: bound, .. }, .. }) = m.forwards.get(&target) {
            if host_port.is_none_or(|wanted| wanted == *bound) {
                return Ok(format!(
                    "{} is already forwarded to 127.0.0.1:{bound}",
                    forwards::describe(&target)
                ));
            }
        }
        m.forwards.remove(&target); // frees the old host port before rebinding
        m.suppressed.remove(&target);
        m.start(target.clone(), Origin::Manual, host_port).await;
        match &m.forwards[&target].state {
            State::Active { host_port, .. } => Ok(format!(
                "{}: {} -> 127.0.0.1:{host_port}",
                m.container.name,
                forwards::describe(&target)
            )),
            State::Error(reason) => {
                let reason = reason.clone();
                m.forwards.remove(&target);
                bail!("{reason}; choose a different host port for container port {port}")
            }
        }
    }

    fn remove(&mut self, reference: Option<&str>, cwd: Option<&std::path::Path>, port: u16) -> Result<String> {
        let m = self.select(reference, cwd)?;
        let targets: Vec<Target> = m.forwards.keys().filter(|t| t.port == port).cloned().collect();
        if targets.is_empty() {
            bail!("port {port} is not forwarded for {}", m.container.name);
        }
        for target in targets {
            m.forwards.remove(&target);
            m.suppressed.insert(target);
        }
        Ok(format!("{}: stopped forwarding port {port}", m.container.name))
    }
}

pub async fn run(auto: bool, on_demand: bool) -> Result<()> {
    // Several clients may start a daemon at the same moment (one per editor
    // window, say). The lock lets exactly one of them through.
    let lock_path = control::lock_path();
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path)?;
    if lock.try_lock().is_err() || control::Client::connect().await.is_ok() {
        if on_demand {
            return Ok(()); // another client won the race to start it
        }
        bail!("a zportforward daemon is already running");
    }
    let rt = Runtime::detect().await?;
    let listener = control::listen()?;
    let (events, mut inbox) = mpsc::channel::<Event>(64);
    tokio::spawn(serve_control(listener, events.clone()));
    tokio::spawn(watch_runtime_events(rt.clone(), events.clone()));
    let ui_url = web::start(events.clone()).await;

    let mut daemon = Daemon {
        rt,
        auto,
        watch: Arc::new(AtomicBool::new(auto)),
        ui_url,
        events,
        containers: BTreeMap::new(),
        last_active: Instant::now(),
        last_rescan: None,
    };
    crate::log!("daemon started (pid {}, auto-forward {})", std::process::id(), if auto { "on" } else { "off" });
    let mut tick = tokio::time::interval(RESCAN_EVERY);
    loop {
        tokio::select! {
            _ = tick.tick() => daemon.rescan().await,
            Some(event) = inbox.recv() => match event {
                Event::Rescan => daemon.rescan().await,
                Event::Detected(id) => {
                    if let (true, Some(m)) = (daemon.auto, daemon.containers.get_mut(&id)) {
                        m.apply_detected().await;
                    }
                }
                Event::Control(req, reply) => {
                    let stop = matches!(req, Request::Stop);
                    let _ = reply.send(daemon.handle(req).await);
                    if stop {
                        break;
                    }
                }
            },
            _ = tokio::signal::ctrl_c() => break,
        }
        if daemon.containers.is_empty() && daemon.last_active.elapsed() > IDLE_EXIT {
            crate::log!("idle for {IDLE_EXIT:?} with no dev containers");
            break;
        }
    }
    crate::log!("daemon stopping");
    drop(lock);
    Ok(())
}

async fn serve_control(listener: interprocess::local_socket::tokio::Listener, events: mpsc::Sender<Event>) {
    loop {
        match listener.accept().await {
            Ok(conn) => {
                tokio::spawn(serve_client(conn, events.clone()));
            }
            Err(e) => {
                crate::log!("control socket accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn serve_client(conn: Stream, events: mpsc::Sender<Event>) {
    let mut conn = BufReader::new(conn);
    let mut line = String::new();
    loop {
        line.clear();
        match conn.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => {
                let (tx, rx) = oneshot::channel();
                if events.send(Event::Control(req, tx)).await.is_err() {
                    return;
                }
                match rx.await {
                    Ok(response) => response,
                    Err(_) => return,
                }
            }
            Err(e) => Response { error: Some(format!("bad request: {e}")), ..Default::default() },
        };
        let mut out = serde_json::to_string(&response).expect("response serializes");
        out.push('\n');
        if conn.get_mut().write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// React to container start/stop at once instead of waiting for the next rescan.
async fn watch_runtime_events(rt: Runtime, events: mpsc::Sender<Event>) {
    loop {
        let child = rt
            .command()
            .args(["events", "--filter", "type=container", "--filter", "event=start", "--filter", "event=die"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
            while let Ok(Some(_)) = lines.next_line().await {
                if events.send(Event::Rescan).await.is_err() {
                    return;
                }
            }
        }
        // The periodic rescan covers us while the event stream is down.
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Tell the daemon whenever the listening ports in a container change.
async fn watch_detected(mut slot: Slot, id: String, events: mpsc::Sender<Event>) {
    loop {
        let tunnel = match slot.wait_for(|t| t.is_some()).await {
            Ok(tunnel) => tunnel.clone().expect("checked by wait_for"),
            Err(_) => return,
        };
        let mut listeners = tunnel.watch_listeners();
        loop {
            if events.send(Event::Detected(id.clone())).await.is_err() {
                return;
            }
            tokio::select! {
                changed = listeners.changed() => if changed.is_err() { break },
                _ = tunnel.closed() => break,
            }
        }
        // Wait until the slot no longer holds this tunnel (it may already hold the next one).
        let replaced = |t: &Option<Arc<Tunnel>>| !t.as_ref().is_some_and(|t| Arc::ptr_eq(t, &tunnel));
        if slot.wait_for(replaced).await.is_err() {
            return;
        }
    }
}
