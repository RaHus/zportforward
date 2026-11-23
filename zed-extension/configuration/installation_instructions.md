zportforward forwards ports from running dev containers to `127.0.0.1` on your machine.
There is nothing to install: the extension downloads `zportforward` for you, and the
only requirement is a working `docker` (or `podman`) command.

Ports listed under `forwardPorts` in `devcontainer.json` are forwarded as soon
as the container runs.

**[Open the ports page](http://127.0.0.1:4480/)** to see every forwarded port and
to add or remove forwards. In an agent thread you can type `/ports`, or ask the
agent to "forward port 5173".

To open the ports page with a key, add this to your keymap (`zed: open keymap file`),
choosing any key you like:

```json
[
  {
    "bindings": {
      "alt-shift-p": ["zed::OpenBrowser", { "url": "http://127.0.0.1:4480/" }]
    }
  }
]
```

Settings, all optional:

- `auto`: also forward ports that start listening later (below 32768).
- `binary_path`: use your own `zportforward` binary instead of the downloaded one.
