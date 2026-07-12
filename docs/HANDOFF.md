# nemesis8 — working-state handoff

> For any agent (or human) picking up work. Snapshot: **2026-07-12, v0.19.0.**
> Keep this file current when you finish a work arc; it is the first thing an
> incoming session should read after CLAUDE.md.

## Current state

- **Released**: v0.19.0 (signed, Latest on GitHub) — event search on lume,
  tool_call feed, fleet dashboard v2, telemetry MCP, port family, config test
  harness, trainer API. main == origin/main == the release.
- **Image**: `nemesis8:latest` rebuilt 2026-07-10, current with 0.19.0.
- **Dev box**: host `n8` = 0.19.0; gateway daemon on **9801** (`n8 serve
  --status`); trainer rides along on **9802** (loopback only).

## The port family (fixed convention, do not invent ports)

| port | what | bind |
|---|---|---|
| 9800 | Hyperia sidecar (theirs) | loopback |
| **9801** | n8 gateway — REST control plane + `POST /mcp` (fleet tools) + `/fleet` dashboard + SSE | 0.0.0.0 |
| **9802** | trainer API (Sailfish tool-run training data) | 127.0.0.1 ONLY (private transcripts) |
| 9124 | Meridian sidecar (theirs) | loopback |
| 18000-18999 | chisel exposure range — do not squat | — |

One definition: `gateway::DEFAULT_PORT` / `trainer_api::TRAINER_PORT`.

## Observability surfaces (all shipped)

- **MCP** `:9801/mcp`: `fleet_status · agent_events · agent_net ·
  event_facets · telemetry_health` (results carry `structuredContent`).
- **Dashboard** `:9801/fleet` (+`/fleet/data.json`, `/fleet/events/stream` SSE).
- **Search**: `q=` routes to the lume store (`src/event_store.rs`, full
  history, all kinds, hyphen-aware); no `q` → live ring. tool_call events are
  synthesized from session transcripts (`src/tool_events.rs`; claude+codex
  dialects — agy is protobuf, see #90).
- cpu/mem/net come from the RUNTIME stats API (docker-parity), not /proc.

## Working rules (hard-won, do not relearn)

1. `docs/PROVIDER-TESTING.md` — run **`n8 mcp test`** after touching MCP/
   providers/config-gen. `docs/RELEASING.md` — the four channels; bump via
   `scripts/bump.sh` BEFORE building; MINOR only when the owner says so.
2. Owner rulings in force: **never mock data** (fail loudly);
   **timestamps Zulu+date, first column, newest-first**; **search must
   surface everything a container does** (all kinds indexed); **runtime
   stats over in-container /proc**.
3. Multi-agent work: strict file ownership per agent, private
   `CARGO_TARGET_DIR`, no Cargo.toml edits by agents — see the
   driving-pane-agents memory. Container test baseline: 8 docker-socket
   gateway tests fail without /var/run/docker.sock; gate on no NEW failures.
4. Kill n8 processes by exact PID only. If launches hang: check
   `docker image inspect nemesis8:latest` entrypoint FIRST (tag-clobber
   incident 2026-07-10, see #92).

## In-flight / uncommitted (working tree)

- `web/fleet.html` — sortable-headers diff, author unknown → **#95** (adopt/drop).
- `mcp-bins/hyperia-cli.js` — #75 WIP, not n8-agent-owned. Leave alone.

## Open issue map (2026-07-12)

- **Fresh follow-ups**: #92 image identity check · #93 launch-time dependency
  probe (blender addon) · #94 session-workspaces container-path bug ·
  #95 sortable headers decision · #96 trainer zip stub · #97 model from
  transcript for default launches.
- **Planned, docs ready**: #85 podman 6 (`docs/plans/podman6-adoption.md`) ·
  #86 Hyperia observability wiring · #87 LOGPANE net panel · #52 secrets
  (`docs/plans/secrets-store.md`) — all shovel-ready briefs.
- **Decisions needed from owner**: #88 ACP keep-or-kill · #95 · TUI v3 train
  (#40-46) prune-or-schedule · April tail (#19-33) sweep.
- **Analytics arc**: #77 epic → #81 view, #82 insights, #83 aggregation
  container (the lume store is its seed).
- **Investigate**: #90 agy protobuf tool calls · #76 agy version pin.

## Cross-repo contracts

- **Hyperia**: consumes `:9801/mcp`; n8 pulse posts busy/idle to
  `:9800/api/pulse/liveness` (pane-token auth) — their poker should treat it
  as authoritative (`docs/plans/hyperia-observability-wiring.md`). Ctrl+C
  findings sticky: "Ctrl+C findings — n8 side".
- **Meridian**: shim `MCP/meridian-mcp.py` + registry `mcp-servers/meridian.toml`
  (:9124). Their build pipeline once clobbered our image tag — see #92.
- **Sailfish**: trainer API :9802 is Part 1 of their handoff spec
  (`SAILFISH_N8_URL` override on their side; port moved off 18042).
- **Secrets interop**: shared OS-keychain namespace `deepbluedynamics`
  (convention over connection — NEITHER product calls the other). See
  `docs/plans/secrets-store.md`.
