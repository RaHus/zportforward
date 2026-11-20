zportforward forwards ports from running dev containers to `127.0.0.1` on your machine.
There is nothing to install: the extension downloads `zportforward` for you, and the
only requirement is a working `docker` (or `podman`) command.

**[Open the ports page](http://127.0.0.1:4480/)** to see every forwarded port and
to add or remove forwards. You can also type `/ports` in a text thread, or ask
the agent to "open the ports page" or "forward port 5173".

Ports listed under `forwardPorts` in `devcontainer.json` are forwarded as soon
as the container runs.

Settings, all optional:

- `auto`: also forward ports that start listening later (below 32768).
- `binary_path`: use your own `zportforward` binary instead of the downloaded one.
