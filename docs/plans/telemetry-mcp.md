# Telemetry over MCP — fleet aggregation Hyperia (and agents) can query

Agent telemetry today is a shared JSONL file (`~/.nemesis8/home/.monitor/
events.jsonl`) plus a best-effort HTTP push to the gateway. Nothing can *ask*
n8 "what are your agents doing" — Hyperia included. This plan puts an
**MCP server on the gateway** serving the aggregated fleet view, so any MCP
client (Hyperia first) gets live per-agent telemetry with one connection.

Written against **v0.18.12**. Verify line numbers before editing.

## Prerequisite (shipped, pending swap)

`3e9b06f` — interactive launches now pass `NEMESIS8_AGENT_ID`, so events are
per-agent tagged again. Without it every aggregation below collapses into one
anonymous stream. (Binary swap pending on the dev box — open n8 TUIs hold the
exe; containers must be relaunched to pick up the env.)

## Architecture

```
 containers ──nemesis8-monitor──► events.jsonl (shared volume, rotating)
                                        │
                              gateway (n8 serve, :4000)
                              EventIndex tail-refresh + roll-ups
                                        │
                              POST /mcp  (streamable HTTP, STATELESS)
                               ├── Hyperia:  http://127.0.0.1:4000/mcp
                               ├── agents:   http://host.docker.internal:4000/mcp
                               │             (registry def, opt-in)
                               └── anything else that speaks MCP
```

Decisions:
- **On the gateway, not a new daemon** — `n8 serve` is already resident
  (gateway_auto_start), already has axum + bearer auth
  (`gateway.rs::auth_middleware` ~L609). One new route.
- **Stateless streamable HTTP** (the meridian-sidecar precedent): every POST
  self-contained — `initialize`, `tools/list`, `tools/call` — no session
  state, so a gateway restart never strands a client.
- **Hand-rolled JSON-RPC dispatch** (~150 lines) instead of a new MCP SDK
  dep: we serve a fixed small tool set, stateless, tools-only capability.
  Fallback to the rmcp crate if protocol drift ever makes this a burden.
- **The MCP tool schemas are the contract.** The aggregation container (#83)
  later swaps in as a richer backend (lume index, cross-machine) WITHOUT
  changing the tools — clients never notice.

## The tools

| tool | args | returns |
|---|---|---|
| `fleet_status` | — | one row per agent: `{agent_id, provider, workspace, state, uptime, cpu_pct, mem_used_kb, net_rx_bps, net_tx_bps, last_ts}` — joins docker ps (labels `nemesis8.provider` / `nemesis8.workspace`) with each agent's newest `metric` event |
| `agent_events` | `agent_id?, kinds?[], since?, q?, limit=100` | filtered events, newest first (thin wrapper over `EventQuery`) |
| `agent_net` | `window=16` | per-agent `{rx_bps, tx_bps, history[]}` — THE SAME aggregation the LOGPANE network panel plan specifies (`AgentNet`), shared |
| `event_facets` | — | `{kind: count}` map (existing `EventIndex::facets`) |
| `telemetry_health` | — | `{events_path, indexed, newest_ts, lag_secs, tagged_ratio}` — the "is aggregation even working" probe (would have caught the tagging regression in one call) |

## Phases

### Phase 1 — shared aggregation lib (S)
New `src/telemetry.rs` (lib):
- `AgentNet` + `agent_net_stats(index, window)` — lifted from the LOGPANE
  network-panel plan §2 so the TUI panel and the MCP tool are one
  implementation (build order with that plan is flexible; whoever lands first
  creates the module).
- `fleet_rows(index, containers)` — the fleet_status join.
- `health(index, path)` — incl. `tagged_ratio` (untagged/total in the window;
  <1.0 after old containers cycle out is the signal something regressed).
- Gateway state gains an `EventIndex` over `events.jsonl` (+ the `.1`
  rotation sibling), tail-refreshed on query with the mtime+size pattern
  proven in `trainer_api.rs`.
Unit tests: roll-ups over synthetic tagged/untagged event mixes.

### Phase 2 — the MCP endpoint (M)
`gateway.rs`: `POST /mcp` route inside the existing auth layer.
- JSON-RPC: `initialize` (protocolVersion echo, `capabilities.tools`,
  serverInfo `nemesis8`), `tools/list` (static schemas), `tools/call`
  (dispatch to Phase-1 fns), JSON-RPC error objects for unknown
  methods/tools. Accept both `application/json` response and SSE-framed
  (`data:` line) responses per streamable-HTTP — stateless either way.
- Bearer: reuses the gateway token (`NEMESIS8_AUTH_TOKEN`) — unset on a
  loopback dev box = open, same posture as today's gateway.
Tests: dispatch unit tests + an integration test hitting a spawned router
with initialize → tools/list → tools/call(fleet_status).

### Phase 3 — consumers (S)
- **Hyperia**: hand them one line — `http://127.0.0.1:4000/mcp` (+ optional
  bearer). Their existing MCP client does the rest. Contract = the tool
  table above; changes are additive-only.
- **Agents in containers** (opt-in): `mcp-servers/nemesis8-telemetry.toml` —
  `url = "http://host.docker.internal:4000/mcp"`, `enabled_by_default =
  false`. Agents can then self-inspect fleet state ("is my sibling agent
  OOMing?"). antigravity: no stdio shim for this one initially — the
  `http_mcp_unsupported` substitution simply drops it with a note (n8gw
  passthrough is the later path if wanted).
- `n8 mcp test` covers the new registry def automatically.
- README + PROVIDER-TESTING pointer.

### Phase 4 — later, out of this plan's scope
- **#83 aggregation container** slots under the same tools (lume-backed,
  incremental, cross-machine federation via the gateway).
- Trainer/transcript tools (`tool_runs_stats` etc.) on the same endpoint —
  one MCP surface for "everything n8 knows," telemetry + usage.
- Podman-plan Phase 5 `agent_died {oom}` events flow through `agent_events`
  for free once emitted.

## Acceptance
- With two tagged agents running: Hyperia (or `curl -X POST :4000/mcp` with
  a tools/call body) gets `fleet_status` rows for both, with distinct
  agent_ids, live cpu/net numbers, and correct workspace labels.
- `agent_net` histories fill over ~16 metric ticks; `telemetry_health.lag_secs`
  stays single-digit while agents run.
- Gateway restart mid-session: next client call succeeds (stateless).
- No new port, no new daemon, no change to the monitor or event schema.

## Touchpoints
**new** `src/telemetry.rs`; `src/gateway.rs` (route + state);
`mcp-servers/nemesis8-telemetry.toml`; tests; docs. Nothing container-side.
