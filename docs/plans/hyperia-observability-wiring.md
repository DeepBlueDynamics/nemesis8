# Hyperia ↔ n8 observability wiring — reconciliation + plan

Companion to Hyperia's Maximus observability epic (#102) and its poker/pulse
liveness thread (#13 / proud-soaring-acorn Phase 2). Written 2026-07-06 against
n8 v0.18.15. **Hand this to the Hyperia agent — it corrects two stale
assumptions and maps every thread to what already exists.**

## 0. Port truth (settles the 40008 confusion)

`gateway::DEFAULT_PORT = 9801` — commit `d994f10`, which SUPERSEDED `daa9cb8`
(40008 lived ~15 minutes). Verified live: 9801/health = 200, 40008 refused.
**Hyperia's 9801 settings (agent-config.html, ghost/api.rs probe) are correct
as-is.** The DeepBlue port family:

| port | owner | bind | what |
|---|---|---|---|
| 9800 | Hyperia sidecar | loopback | panes/MCP (n8 agents call in) |
| 9801 | n8 gateway | 0.0.0.0 | control plane REST + `/mcp` + `/fleet` |
| 9802 | n8 trainer | 127.0.0.1 ONLY | tool-run training data (transcripts) |

## 1. What ALREADY exists (don't rebuild these)

### n8 → Hyperia: pulse liveness (the poker's authoritative signal) — BUILT
`src/pulse.rs` + `src/monitor.rs` (in every agent container, since v0.16.x):

- **POST `{HYPERIA_URL}/api/pulse/liveness`** with `Authorization: Bearer
  {HYPERIA_AGENT_TOKEN}` (the pane token — so Hyperia knows WHICH pane).
- Busy: `{"state":"busy","ttl_secs":10}` — sent on idle→busy transition,
  keepalive every ~5s while busy.
- Idle: `{"state":"idle"}` — sent after 3 consecutive idle ticks (~6s grace).
- Tick = 2s; busy = whole-container CPU/net/io thresholds
  (`collectors::Sample::busy`).
- Silent no-op when `HYPERIA_AGENT_TOKEN` is absent (container launched
  outside a Hyperia pane).

**So "wire n8's liveness to the poker" is a Hyperia-side task**: make
`/api/pulse/liveness` authoritative and the screen-staleness heuristic the
fallback. n8 is already talking; if the poker false-idled, either the endpoint
wasn't consuming these posts or the pane-token association was lost.

### Hyperia → n8: fleet/logs pull — BUILT TONIGHT (#84, n8 side)
- **`POST :9801/mcp`** (stateless streamable-HTTP MCP): `fleet_status`,
  `agent_events` (kind/agent/since/text filters over the monitor event
  stream), `agent_net`, `event_facets`, `telemetry_health`. Results carry
  `structuredContent`.
- **`GET :9801/fleet/data.json`** — same data as one JSON blob;
  `GET :9801/fleet` — human dashboard.
- Event kinds available today: `metric` (cpu/mem/net per agent), `fs`,
  `heartbeat`, `status` (incl. `pulse`/`pulse_post`/`pulse_error` — the pulse
  transitions are ALREADY queryable events).
- This is Hyperia's `n8:log` fragment loader backend: poll
  `agent_events {since}` — no new n8 code needed.

### Tool-call / model-turn extraction (Maximus #71) — mostly built
n8's trainer API (`127.0.0.1:9802`, host-reachable by Hyperia) already parses
transcripts into tool-run records: `GET /v1/tool-runs?since=<ts>&format=jsonl`
streams `{provider, tool, arguments, context, result_preview, ts}`. A
since-cursor poll gives Maximus real tool-call events without any
screen-scraping. Optional bearer `SAILFISH_N8_TOKEN`.

## 2. Gaps — the actual n8-side work

### A. Pulse hardening (S)
- Emit an initial `idle` (or `hello`) post at monitor start so the pane
  association registers BEFORE the first busy transition — a fresh container
  currently says nothing until work starts, which a poker can misread.
- Error backoff (currently retries every tick while Hyperia is down; harmless
  but chatty in events.jsonl).
- The contract table above becomes a shared doc both repos link.

### B. Lifecycle events (S — overlaps podman-plan Phase 5)
`agent_died {agent_id, exit_code, oom}` emitted on container exit → shows up
in `agent_events` automatically. Gives Maximus its alert-worthy terminal
events (OOM-kill vs crash vs clean exit).

### C. SSE live feed (M — only if polling hurts)
Hyperia's #73 wants a live feed. Start with `agent_events {since}` polling
(2s cadence matches the monitor's own resolution). If that's ever too slow,
add `GET :9801/monitor/events/stream` (SSE) on the gateway — the TeeSink
already fans out, so a broadcast sink is contained work. Don't build until
polling demonstrably hurts.

### D. NOT n8's job
Blocks/stability classification, alerting, Ferricula routing, delta
compression (#121) — Maximus-side, consuming the feeds above.

## 3. Suggested sequence

1. **Hyperia side**: point the poker at `/api/pulse/liveness` (n8 already
   posts); re-read n8 local main for the 9801 truth.
2. **n8 A** (pulse hello + backoff) — small, hardens the poker fix.
3. **n8 B** (agent_died) — rides with podman plan Phase 5.
4. Maximus consumes `agent_events` + trainer tool-runs; C only if needed.
