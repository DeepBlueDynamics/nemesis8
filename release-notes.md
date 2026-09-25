# nemesis8 v0.26.0 — Reach across 🌉

Run a Hermes backend on one machine and use it from Hermes Desktop on another, with nothing but n8's gateway crossing between them. `n8 connect hermes --remote http://host:9801` on your desk gives Hermes Desktop a local `http://127.0.0.1:8642` that is really the container on the host — same URL, same token, no login. `n8 shell` and `n8 attach` work across the same wire, so you can drop into a remote agent's container or its TUI from wherever you are. To get it: `n8 update` on both machines; on the host, set a gateway token (`n8 secrets set NEMESIS8_AUTH_TOKEN`) and restart `n8 serve`.

## One path: the gateway

Everything that crosses machines goes through the gateway on 9801, authenticated with a bearer token. Hermes itself stays exactly as before — bound to `127.0.0.1` inside its container and reached over the reverse tunnel — so its password/OAuth gate never engages and the desktop only needs the session token n8 issues. No SSH, no published ports, no Docker on the desk.

`n8 connect <provider>` is a pure gateway client. It finds the provider's exposed backend, fetches the desktop token, listens on `127.0.0.1:<port>` locally, and bridges each connection over a WebSocket (`GET /exposed/{host_port}/stream`) onto the same container-side tunnel the host uses. The listener never goes away while it runs; if the gateway restarts, the desktop's own reconnect picks it back up.

## Shell and TUI, remotely

`n8 shell <agent> --remote …` opens a shell inside the agent's container; `n8 attach <agent> --remote …` attaches to the agent's terminal — for a backend container that means its interactive TUI. Both go over `GET /agents/{id}/pty`, a WebSocket that the gateway wires to a Docker/Podman exec (or an attach to the agent's own TTY) through the engine API, so it works from a Windows host too. Keystrokes and resizes flow one way, terminal bytes the other; `Ctrl-]` then `q` detaches and leaves the agent running. Locally, `n8 shell <agent>` now execs into a running container as well (bare `n8 shell` still starts a scratch container).

## The gateway remembers its tunnels

Tunnel mappings are persisted (`~/.nemesis8/home/tunnels.json`) and restored when the gateway restarts: each comes back as degraded, and the health monitor from 0.25.1 re-attaches the container clients on its next tick. A gateway restart no longer needs the backend re-launched. A desktop reconnecting during that window gets a brief retry instead of a hard refusal.

## Locked by default, and says so

The gateway now reads its bearer token from the OS keychain as well as the environment, so `n8 serve --background` needs no prefix, and it logs plainly whether it is enforcing auth — or is open. `/health` stays public. Every n8 client sends the token: the containers' own registration and the Hermes plugin, `serve-backend`, `n8 agents`, `n8 connect`, `n8 shell`, `n8 attach`. Until TLS lands (next), the tokens cross the wire in cleartext — keep desk and host on a private network.

## Also

- Everything in [v0.25.4](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.25.4) and [v0.25.5](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.25.5) is included: the OAuth callback tunnel no longer loses the race with the gateway's reconcile, the gateway port-exhaustion fix (keep-alive monitor, log tailer from EOF, telemetry push removed), containers speaking to Hyperia as themselves (per-container token file), the `nuts-files` wedge fix, and `n8 serve --stop` finding a foreground gateway.
- `GET /exposed` reports `attached_clients` and `provider` per mapping; `GET /serve-tokens/{provider}` hands a remote client the desktop token.
- `wss://` is not supported yet (no TLS in the WebSocket client); `n8 serve --bind` and per-client tokens are the remaining hardening items.
- Hermes sessions no longer vanish from the picker when one of them has no working directory (a backend or desktop session): the sessions scan tolerates NULL columns instead of aborting with `Invalid column type Null … name: cwd` — a warning that used to print into whatever session you were in.

Coming from further back? [v0.25.5](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.25.5) was the previous published build.
