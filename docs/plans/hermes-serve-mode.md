# Plan: `n8` provider serve-mode (Hermes desktop backend via n8)

## Goal

Launch a provider's **backend server** (Hermes: `hermes serve`) through n8 — so the
Hermes One Desktop connects to a backend that n8 spun up, reusing all of n8's
container machinery instead of a hand-assembled `docker run`.

One command, e.g.:

```
n8 --provider hermes serve-backend --port 8642
```

→ n8 launches the container, nemesis8-entry writes the hermes config, runs
`hermes serve` bound to container-loopback, exposes it to the host over the
reverse tunnel, and prints the URL for the desktop.

## Why through n8 (what we get for free)

Verified in-tree:
- **Config**: `write_provider_config` (`entry.rs:174`) already writes `~/.hermes/config.yaml`
  with the **container-correct** ollama URL (`host.docker.internal:11434`, per
  `providers/hermes.toml`), MCP tools, the bundled `nemesis8` plugin, and the
  installer pin. A raw `docker run` gets none of this.
- **Workspace mount**: n8 already bind-mounts the cwd → `/workspace` on every launch.
- **Runtime abstraction**: `docker.rs` already picks docker vs podman
  (`NEMESIS8_RUNTIME`, detection, `host.docker.internal` vs `host.containers.internal`,
  `docker.rs:289–348`). Podman is (mostly) free.

## Key design decision: loopback + reverse tunnel (no auth), not `-p`

Hermes's June-2026 hardening requires an auth provider for any **non-loopback**
bind. So:
- **Bind loopback in the container**: `hermes serve --host 127.0.0.1 --port <p>`.
  Loopback bind ⇒ **no auth required**.
- **Expose it via the existing reverse tunnel** (`gateway.rs:expose_port` :1118 →
  `start_tunnel_clients` :1314 → `nemesis8-entry --tunnel-client <addr> <host_port>
  <internal_port>`, client at `entry.rs:70`). The tunnel-client (in the container)
  dials the gateway acceptor and forwards the container's `127.0.0.1:<p>` out to a
  host port. Desktop → host port → tunnel → container-loopback Hermes. Auth-free.
- Fallback: `--publish` (direct `-p`) exists (`gateway.rs:1137`) but reintroduces
  `0.0.0.0` + the auth requirement. Tunnel is preferred; publish is the escape hatch.

Tunnel reuse is also **more rootless-podman-friendly** (outbound dial vs inbound publish).

**Caveat / dependency:** the expose/tunnel path carries the known OAuth-tunnel bugs
(container_id-not-registered 404 + docker-exec 502 — the nemesis8#106 binding gap).
P2 leans on that path; shore up the container_id binding if it bites (or ship P1 on
`--publish` first).

## Invocation (avoid the `serve` collision)

`Command::Serve` is already the **gateway** daemon (`cli.rs:157`). Do NOT overload it.
Options (decide at impl):
- New command `ServeBackend` invoked as `n8 --provider <p> serve-backend`.
- Or a `--serve` flag on the run path that, with `--provider`, launches the backend.
Chosen: **`serve-backend`** subcommand (explicit, no collision), `--port`, `--publish`
(escape hatch), reusing the global `--provider`/`--danger`.

## Phases

### P1 — serve-mode + launch (baseline)
- `provider_def`: add a `[provider.serve]` capability (subcommand `serve`, host flag
  `--host`, port flag `--port`, default port). Populate in `providers/hermes.toml`.
- `entry.rs run_provider` (:562): a serve branch — after config write, exec
  `hermes serve --host 127.0.0.1 --port <p>` instead of the interactive/exec agent.
- `docker.rs`: a **detached + restart=unless-stopped** launch variant (agent runs are
  attached). Reuse the existing workspace mount + env forwarding.
- `cli.rs`/`main.rs`: `serve-backend` command + `--port`/`--publish`; wire to the launch.
- Baseline exposure via `--publish` first (simplest correct path), so P1 works end-to-end.

### P2 — reuse the reverse tunnel (auth-free)  ✅ implemented
- Default path now binds container-loopback (`NEMESIS8_SERVE_HOST=127.0.0.1` ⇒
  Hermes needs no auth) and routes to the host via the existing reverse tunnel:
  the host `serve-backend` arm health-checks the gateway, launches loopback (no
  `-p`), waits for the reconcile loop (10s tick) to bind the container into the
  registry by its `nemesis8.agent_id` label, then POSTs `/expose` and prints the
  returned `public_url`.
- `--no-tunnel` forces the P1 direct `-p` (0.0.0.0) publish; it is also the
  automatic fallback when no gateway is reachable.
- **No container/entry change needed**: reconcile already populates
  `container_id`/`container_name` on every record from `docker ps`, so
  `resolve_tunnel_container` finds the serve container. Host-binary-only change.
- Verified: Hermes binds `127.0.0.1:<p>` with **no auth wall** (container log
  `Hermes backend listening on 127.0.0.1:<p>`); the container registers,
  reconcile binds the ref, `/expose` resolves it and starts the host forwarder.
  Verified end-to-end on a 0.25.0 gateway: Hermes answers HTTP 200 through the
  tunnel on the host port, no auth.
- **Known transient:** the gateway's `docker exec` of the tunnel client can fail
  (`502 starting tunnel client failed: docker exec exited with exit code: 1`)
  on the first try right after container launch — the container is healthy and
  Hermes is listening; a retry of the identical `/expose` succeeds. (`docker exec
  -d` returns rc=0 even for a failing command, so that exit-1 is the exec itself
  not starting, not a handshake problem. An earlier "version skew" diagnosis was
  wrong.) The gateway rolls the mapping back on failure, so retrying is clean —
  the host loop now retries 502 instead of aborting.
- TODO (P3): teardown (`unexpose_port`) on stop.

### P3 — polish
- `--status` / `--stop` (map to the container + `hermes serve --status/--stop`).
- Port default (Hermes serve default 9119; allow 8642 to match the desktop default).
- Podman verification on a rootless host (Clint's box if reachable).

## Files
- `src/provider_def.rs`, `providers/hermes.toml` — serve capability
- `src/entry.rs` (`run_provider`) — serve branch
- `src/docker.rs` — detached/persistent launch + runtime-agnostic
- `src/cli.rs`, `src/main.rs` — `serve-backend` command + flags + expose wiring
- `src/gateway.rs` — reuse `expose_port` (P2)

## Verification
- `cargo check` after each phase.
- `n8 build --from-source` (from repo dir), then `n8 --provider hermes serve-backend
  --port 8642` → desktop connects; chat answers via local ollama (glm-5.3:cloud).
- Repeat on podman (runtime-agnostic paths).
- Never trust the pipeline exit — read the actual build/run output.
