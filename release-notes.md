# nemesis8 v0.25.4 — Quiet on the wire 🔌

A busy swarm could run the host out of network ports. This release stops the in-container monitor from opening a new connection for every telemetry event, stops it replaying whole log files on start, and fixes two ways a container could talk to Hyperia under another identity. To get it: `n8 update`, then `n8 build` for the container image, then recreate your containers.

## The port exhaustion

Every n8 container runs a small monitor that reports activity to the gateway. It opened a fresh TCP connection for every event, sent `Connection: close`, and never read the reply. With a busy agent that meant dozens of new connections a second, each one parked in TIME_WAIT for two minutes on Windows. On one host 15,475 sockets sat in TIME_WAIT at once, 94 % of the ephemeral range, and every loopback connect on the machine started failing at random.

Two things fed it:

- The monitor's log tailer replayed every existing `*.log` from byte 0 on each launch, including a 26,000-line database WAL under a `cache/` directory, one event per line. It also walked `.git`, `node_modules`, `cache` and `target` over the container mount every 3 seconds.
- The event push went to `POST /agents/{id}/events`, a route the gateway never had. Every push was a 404. The gateway reads the same events from the shared `events.jsonl` file, so the push carried nothing.

What changed:

- The monitor writes the JSONL file only. The push is gone.
- The HTTP client the entry still uses for registration and pulses keeps one connection per host and reuses it, reading each reply in full.
- The log tailer starts at the end of a file the first time it sees it, skips hidden and build/cache directories, and emits at most 500 lines per poll with a notice for the rest.

## Identity: a container must speak as itself

n8 mints a distinct Hyperia identity for each container. Two bugs let a container present someone else's:

- The per-container token file that the Hyperia shim reads was written from the host `n8` process's own token, which is the launching pane's identity, not the container's minted token. Attaching from another pane rewrote it too. Launch now writes the container's own token and attach leaves the file alone.
- The Hyperia stdio shim (`hyperia-mcp.py`) reads that token file before every request, so it picks up a rotated token and ignores a stale one baked into a shared provider config. It also rebuilds its upstream session after a failed or hung call instead of answering errors forever.

## Also

- `n8 serve --stop` (and the control room's Gateway ▸ Stop) now stops a gateway that was started in the foreground: without a pid file it finds the process listening on the port and stops it if it is an n8 binary. A stale pid file is cleared instead of killing whatever now has that PID, and the gateway on the given port takes precedence over a daemon recorded on another port. A port owned by another program is reported and left alone.
- `nuts-files` 0.1.1: a cancelled or timed-out `nuts_search` no longer wedges every later call. Requests run concurrently, cancellation is honoured, and every tree walk is capped at 50,000 entries or 20 seconds with a `truncated` flag.
- Hermes sessions with a NULL working directory no longer produce a warning on every `n8 sessions`.

Coming from further back? [v0.25.3](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.25.3) was the previous published build.
