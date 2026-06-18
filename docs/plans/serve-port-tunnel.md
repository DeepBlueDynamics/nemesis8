# Plan: dynamic port exposure for agent-started servers (`serve` reverse tunnel)

## Goal
An agent in a `n8 --danger` container starts a server on some port `P`, calls an
`n8` MCP tool, and gets back a real `localhost:<hostport>` URL on the host — even
though Docker can't add a `-p` mapping to a running container. A host-side
negotiator allocates a free host port, bridges it to the container's `P`, and the
dashboard shows it live.

## Locked decisions (from scoping)
1. **Bridge = reverse tunnel (container → host).** The only always-open path on
   Docker Desktop is container→host (`host.docker.internal`); host→container IP is
   blocked and live `-p` is impossible. So the container dials out and the host
   pipes inbound connections back through that channel.
2. **Reachability = host-only.** Negotiated ports bind `127.0.0.1` — you open them
   in your browser; nothing hits the LAN.
3. **Trigger = explicit.** The agent calls an `expose_port` MCP tool; no
   auto-detection of listening sockets (predictable, and the agent learns the URL).
4. **Daemon = persistent, dashboard-toggled.** Reuses the existing `serve`
   background daemon (`daemon.rs`: `spawn_background`/pid/log); the dashboard
   toggle starts/stops it and lists active mappings.

## What already exists (build on, don't reinvent)
- `nemesis8 serve` — resident gateway + scheduler, axum on `:4000`, daemon mode
  via `--background/--status/--stop` (`src/gateway.rs`, `src/daemon.rs`).
- Containers already know `GATEWAY_URL = http://host.docker.internal:4000`.
- MCP registry (`mcp-servers/*.toml` + `.py`/binary tools) — where the new
  `expose_port` tool plugs in.
- Static port publish (`.nemesis8.toml` `ports`) — the *launch-time* analogue;
  this is the *runtime* counterpart.

## Architecture
```
 agent ──expose_port(P,name)──▶ expose tool (in container)
                                  │  1. probe host.docker.internal:4000 (serve up?)
                                  │  2. POST /expose {agent_id, port:P, name}
                                  ▼
 host: serve/gateway  ──▶ port negotiator: pick free 127.0.0.1:<hostport> (range, bind-test)
                      ──▶ register reverse-tunnel route hostport → this agent's localhost:P
                      ──▶ respond { public_url: "http://127.0.0.1:<hostport>", id }
                                  │
 tunnel client (in container) ──opens reverse tunnel──▶ tunnel server (host, in serve)
   data plane: host accepts on 127.0.0.1:<hostport> → muxed over the tunnel → localhost:P
```

### Components
**A. Host — `serve` additions**
- **Control endpoint** on the gateway: `POST /expose`, `POST /unexpose`,
  `GET /exposed` (list). Authn by the caller's agent token (see Security).
- **Port negotiator**: allocate from a configurable range (default e.g.
  `18000–18999`), confirm free with a `127.0.0.1` bind-test (avoid in-use),
  track allocations, release on unexpose / tunnel-drop / container-exit.
- **Reverse-tunnel server**: accepts the container's outbound tunnel, and for
  each inbound connection on `127.0.0.1:<hostport>` pipes it over the tunnel to
  the container's `localhost:P`. (Transport choice below.)
- **Mapping registry**: `{ id, agent_id, container, internal_port, host_port,
  name, state }` — in-memory + readable by the dashboard (over the gateway or the
  control-plane file the dashboard already reads).

**B. Container — the expose tool + tunnel client**
- **`expose_port` MCP tool** (registry entry, stdio): args `port`, optional
  `name`. Probes serve; on success opens the tunnel for `P` and returns the
  `localhost:<hostport>` URL (and a clear error if serve isn't running:
  "enable Serve in the n8 dashboard").
- **Tunnel client**: dials `host.docker.internal` and maintains the reverse
  tunnel for the allocated route. Likely the same process the tool launches.

**C. Transport (data plane) — recommendation**
- **v1: [chisel](https://github.com/jpillora/chisel)** — proven reverse-TCP-over-
  HTTP/ws tunneler. `chisel server` rides on/next to the gateway; the container
  runs `chisel client host.docker.internal:4000 R:127.0.0.1:<hostport>:localhost:P`.
  Handles connection mux + concurrency for free; two static binaries baked into
  the image + shipped on the host (like `nuts-files`/`shivvr`/`ask`).
- **v2 (optional): Rust-native** tunnel over the existing axum gateway
  (tokio + websocket + a small mux) — keeps it all-Rust, no Go binary, but is the
  bulk of the work. Recommend chisel first; port to native if we want zero deps.

**D. Dashboard (control room)**
- A **Serve** toggle → `daemon::spawn_background` / stop (status from the pid).
- Live **indicator** (serve up/down) + **port list**: each mapping as
  `name  P → 127.0.0.1:<hostport>  ●live`, with a key to **unexpose/kill** one.
- Mirrors the existing detail/overlay patterns; data via the gateway `GET /exposed`.

## Flow (happy path)
1. Agent: `expose_port(3000, "dev")`.
2. Tool probes `host.docker.internal:4000`; serve is up.
3. Tool → `POST /expose {agent_id, port:3000, name:"dev"}`.
4. Negotiator picks free `127.0.0.1:18042`, registers route, replies `{url, id}`.
5. Tool opens the reverse tunnel (`R:127.0.0.1:18042:localhost:3000`).
6. Tool returns to the agent: *"serving at http://127.0.0.1:18042"*.
7. You open `127.0.0.1:18042` on the host → traffic tunnels to container `:3000`.
8. Dashboard shows `dev  3000 → 127.0.0.1:18042  ●live`.

## Security
- Bind **127.0.0.1 only** (decision 2).
- **Authn** every `/expose` with the caller's Hyperia/agent token; a container may
  only expose **its own** container's ports (map agent_id → container, tunnel
  scoped to that container). Don't let any process on the gateway open host ports.
- Cap concurrent mappings per agent; rate-limit allocation.

## Lifecycle / release
- Release a host port + tear down the tunnel when: the agent calls `unexpose`,
  the tunnel drops (server stopped / container exited), or the dashboard kills it.
- serve reconciles on a timer (drop routes whose container is gone).

## Phasing & rough effort
- **P1 — host negotiator + control endpoints** (`/expose`,`/unexpose`,`/exposed`,
  range alloc, registry): ~2–3 d.
- **P2 — transport (chisel orchestration) + container `expose_port` tool + probe**:
  ~2 d.
- **P3 — dashboard toggle + live status + port list + kill**: ~1 d.
- **P4 — auth scoping, lifecycle reconcile, tests**: ~1–2 d.
- **Total: ~1–1.5 weeks** for a solid v1 (chisel transport). v2 native tunnel is a
  separate follow-up.

## Open questions / risks
- **Transport dep**: OK to bake `chisel` (Go binaries) into the image + host, or
  do we want Rust-native from the start (more time)? (Recommend chisel for v1.)
- **Same gateway port vs sibling**: run the tunnel server on `:4000` (multiplexed
  with the API) or a dedicated tunnel port? (Sibling is simpler; one more
  host.docker.internal port.)
- **Container identity → container mapping**: serve needs to resolve a request to
  the right container/tunnel; rely on the agent token + the agent_id↔container
  label we already set.
- **Non-HTTP servers**: the tunnel is raw TCP (works for any protocol); the
  reported URL assumes HTTP — fine to just report `host:port` and let the agent
  say what it is.
- **Windows host specifics**: `daemon.rs` already detaches on Windows; confirm the
  tunnel server + 127.0.0.1 binds behave under Docker Desktop.
