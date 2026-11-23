# zportforward

On-the-fly port forwarding for dev containers, for editors that lack it (Zed, Neovim, plain terminals).

- Ports in `devcontainer.json` `forwardPorts` come up automatically while the container runs.
- Any other port can be added or removed at runtime.
- Reaches services bound to `localhost` inside the container.
- Host side binds `127.0.0.1` only.
- Needs only a working `docker exec` (or `podman exec`): no published ports, no container IP, nothing preinstalled in the image.

## Install

**With Zed:** install the zportforward extension. It downloads the right `zportforward` binary for your
machine and keeps it updated; the only requirement is a working `docker` or `podman` command.

**Without Zed:** download `zportforward` for your platform from the releases page and put it on your
`PATH`, or build it:

```sh
cargo install --path crates/zportforward
```

Building needs the Rust targets `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`
(`rustup target add ...`); the in-container agent is compiled for both and embedded in the binary.

## Use

Run from the project directory (or pass `--container NAME`):

```sh
zportforward ls              # containers, forwarded ports, other listening ports
zportforward add 5173        # container port 5173 -> 127.0.0.1:5173
zportforward add 80:8080     # container port 80   -> 127.0.0.1:8080
zportforward add 6379 --host redis   # a host reachable from inside the container
zportforward rm 5173
zportforward ui              # open the ports page in the browser
zportforward stop            # stop the daemon, close everything
```

The first command starts a background daemon; it exits by itself after ten minutes with no dev
containers.

### Ports page

`zportforward ui` opens a page served by the daemon on `http://127.0.0.1:4480/` (the next free port if
that one is taken; `ZPORTFORWARD_UI_PORT` picks another, `0` turns the page off). It lists every dev
container with its forwarded ports, and lets you add and stop forwards and switch auto mode.

The page is reachable only from this machine. It refuses requests whose `Host` is not its own
loopback address and state-changing requests that do not come from the page itself, so other web
sites cannot drive it. Other local users of the machine can reach it, as they can reach the
forwarded ports.

### Auto mode

In auto mode zportforward also forwards ports that start listening later, and closes those forwards
when the listener goes away. Ports from 32768 up and ports with `onAutoForward: "ignore"` are
skipped. Turn it on with the switch on the ports page, with `zportforward daemon --auto`, or with
`ZPORTFORWARD_AUTO=1` in the environment of `zportforward mcp` (what the Zed extension's `auto` setting does).

Linux has no unprivileged notification for a socket starting to listen, so auto mode polls: every
second for 30 seconds after a change, then every 2 seconds, then every 5 seconds once two minutes
have passed quietly. A scan is one netlink `sock_diag` query for listening sockets, with
`/proc/net/tcp` as the fallback. Without auto mode the agent scans only when `zportforward ls` asks.

`zportforward fwd PORT` forwards a single port in the foreground without the daemon, for any container.

### Host port rules

- The same number as the container port is tried first.
- If it is busy or privileged (below 1024) and `portsAttributes` sets `requireLocalPort`, the
  forward is listed with an error; otherwise a nearby port is used (80 becomes 8080).
- A port removed with `rm` stays removed until it is added again.

## Zed

The extension in `zed-extension/`:

- downloads `zportforward` and starts it when Zed starts, so `forwardPorts` work without any action;
- gives the agent the tools `list_ports`, `forward_port`, `stop_forward` and `open_ports_page`;
- links the ports page from its configuration window, with a keymap snippet to open it by key.

The MCP server also offers a `ports` prompt, which Zed's agent shows as the `/ports` command; it
opens the ports page. Zed extensions cannot add panels, actions or key bindings, which is why the
ports view is a web page and the key binding is a snippet you add yourself:

```json
[{ "bindings": { "alt-shift-p": ["zed::OpenBrowser", { "url": "http://127.0.0.1:4480/" }] } }]
```

With several dev containers running, the agent has to name the container it means; Zed does not
tell a context server which project it belongs to. The ports page shows all of them.

Settings (optional): `auto`, and `binary_path` to use your own binary instead of the download.
To work on the extension, run `zed: install dev extension` and pick `zed-extension`; Zed then
needs `rustc`, `cargo` and `rustup` on its own `PATH`.

Any other MCP client can use `zportforward mcp` (stdio) directly.

## How it works

```
host: zportforward daemon                        container: agent (static binary)
  listens on 127.0.0.1:<port>                      dials 127.0.0.1:<port> per stream
  one `docker exec -i` per container  == yamux ==  reports listening ports on request
```

The agent is copied to `/tmp/.zportforward/` in the container (falling back to `~/.cache/zportforward/`) on
first use and exits when the daemon disconnects.

Environment: `ZPORTFORWARD_RUNTIME` (`docker`/`podman`), `ZPORTFORWARD_AGENT_PATH` (use this agent binary),
`ZPORTFORWARD_AGENT_DIR` (build time: prebuilt agents to embed).

## Tests

```sh
cargo test --workspace
tests/e2e/run.sh        # needs docker; stops any running zportforward daemon
tests/e2e/multi.sh      # several clients and containers at once
```

Tested on Linux with Docker. macOS, Windows and Podman builds type-check but have not been run.
