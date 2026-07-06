# Telemetry MCP Review

Verifier: Codex
Plan contract: `docs/plans/telemetry-mcp.md`

## Test Gate

Command:

```sh
export CARGO_TARGET_DIR=/tmp/target-tahr
cargo test --lib
```

Adjusted baseline per orchestrator guidance: the following 8 failures are known
environmental failures in agent containers without `/var/run/docker.sock`:

- `gateway::tests::test_expose_rejects_zero_port`
- `gateway::tests::test_health_endpoint`
- `gateway::tests::test_session_not_found`
- `gateway::tests::test_sessions_endpoint_ok`
- `gateway::tests::test_status_endpoint`
- `gateway::tests::test_trigger_not_found`
- `gateway::tests::test_triggers_endpoint_empty`
- `gateway::tests::test_unknown_route_returns_404`

Current result after `feat(telemetry)` Phase 1-3:

- `180 passed`
- `9 failed`
- `2 ignored`

New failure beyond adjusted baseline:

- `gateway::tests::test_mcp_integration`

All 9 failures panic at `src/gateway.rs:1734` on
`DockerOps::new(None).unwrap()` with `Socket not found: /var/run/docker.sock`.

## Ownership Audit

Committed telemetry changes since the plan commit (`87d9f69..HEAD`) touch only:

- `src/telemetry.rs`
- `src/gateway.rs`
- `src/lib.rs`
- `mcp-servers/nemesis8-telemetry.toml`

`Cargo.toml` is untouched. Version is unchanged.

No `feat(telemetry-web)` commit is present yet.

## Findings

### High: `/mcp` returns extractor errors instead of JSON-RPC parse errors for malformed bodies

File: `src/gateway.rs:1454`

The handler uses `Json<serde_json::Value>` as an extractor. Malformed JSON never
enters `mcp_handler`, so axum returns its extractor rejection instead of a
JSON-RPC error object. The verifier brief explicitly calls out malformed body
handling under JSON-RPC correctness.

Repro:

```sh
curl -i -X POST http://127.0.0.1:4000/mcp \
  -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":7,"method":"tools/list"'
```

Expected: JSON-RPC error object, preserving parse-error semantics and `id:
null` when the id cannot be recovered.

Actual: axum JSON extractor rejection before the JSON-RPC dispatch path.

### High: `tools/call` results do not match the plan tool-table return schemas

Files:

- `src/gateway.rs:1606`
- `src/gateway.rs:1642`
- `src/gateway.rs:1657`
- `src/gateway.rs:1671`
- `src/gateway.rs:1685`

The plan says the MCP tool table is the contract and lists concrete return
shapes: `fleet_status` returns rows, `agent_events` returns events,
`agent_net` returns per-agent network rows, `event_facets` returns a
`{kind: count}` map, and `telemetry_health` returns a health object.

The implementation wraps every tool result as MCP text content whose `text`
field is a serialized JSON string:

```json
{"content":[{"type":"text","text":"[...]"}]}
```

That forces clients to double-parse a string and prevents callers from receiving
the structured schema promised by the plan.

Repro:

```sh
curl -s -X POST http://127.0.0.1:4000/mcp \
  -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"telemetry_health","arguments":{}}}'
```

Expected result payload matching:

```json
{"events_path":"...","indexed":0,"newest_ts":0,"lag_secs":0,"tagged_ratio":1.0}
```

Actual result payload is a `content[0].text` string containing that JSON.

### High: rollups silently ignore events beyond `EventQuery`'s default limit

Files:

- `src/telemetry.rs:117`
- `src/telemetry.rs:183`
- `src/event_index.rs:152`

`agent_net_stats` and `fleet_rows` pass `limit: 0` to `EventIndex::query`.
In `EventIndex`, `limit == 0` means `DEFAULT_LIMIT` (`500`), not unlimited.
As a result, both rollups only inspect the newest 500 matching metric events,
even though the telemetry state cap is 10000.

Repro: ingest 501 newer metric events for `agent-a`, then one older metric for
`agent-b`, and call `agent_net_stats(&index, 16)`. `agent-b` is omitted even
though it is still indexed.

This violates the plan's fleet aggregation contract for multi-agent views under
busy metric streams.

### Medium: `/mcp` accepts non-JSON-RPC requests as valid method calls

File: `src/gateway.rs:1459`

The dispatch path does not validate `jsonrpc == "2.0"`. Any JSON object with a
`method` field is accepted, including requests missing `jsonrpc` or carrying a
different version.

Repro:

```sh
curl -s -X POST http://127.0.0.1:4000/mcp \
  -H 'content-type: application/json' \
  --data '{"id":42,"method":"tools/list"}'
```

Expected: `-32600 Invalid Request`.

Actual: successful `tools/list` response.

### Medium: telemetry mutex poisoning can panic the gateway request path

Files:

- `src/telemetry.rs:47`
- `src/telemetry.rs:69`
- `src/gateway.rs:1604`
- `src/gateway.rs:1627`
- `src/gateway.rs:1655`
- `src/gateway.rs:1669`
- `src/gateway.rs:1683`

The verifier brief calls out poisoned mutex panic paths. `TelemetryState` and
the MCP handler use `lock().unwrap()` throughout. If a prior request panics
while holding one of these locks, subsequent telemetry requests panic instead of
returning a JSON-RPC internal error.

Repro: poison `state.telemetry.index` in a unit test by panicking while holding
the lock, then call `tools/call` for `telemetry_health`.

Expected: JSON-RPC `-32603` error object.

Actual: request task panics on `PoisonError`.

### Medium: new MCP integration test expands the adjusted failure set in agent containers

File: `src/gateway.rs:1992`

The orchestrator-approved environmental baseline contains 8 gateway failures
caused by missing `/var/run/docker.sock`. The new `test_mcp_integration` calls
`test_router()`, which calls `test_state()`, which unwraps
`DockerOps::new(None)` at `src/gateway.rs:1734`. In this verifier container it
adds a ninth failure with the same socket error.

Repro:

```sh
export CARGO_TARGET_DIR=/tmp/target-tahr
cargo test --lib gateway::tests::test_mcp_integration
```

Expected under the adjusted gate: no failures beyond the known 8 environmental
gateway tests.

Actual: one new Docker-socket failure.

## Re-Verification Verdict — 2026-07-06

Commits reviewed:

- `5d54d23 fix(telemetry): address review findings 1-6`
- `0f895fe feat(telemetry-web): fleet dashboard — /fleet page + /fleet/data.json blob`

Adjusted gate command:

```sh
export CARGO_TARGET_DIR=/tmp/target-tahr
cargo test --lib
```

Result:

- `189 passed`
- `8 failed`
- `2 ignored`

The remaining 8 failures are exactly the approved environmental
`/var/run/docker.sock` baseline:

- `gateway::tests::test_expose_rejects_zero_port`
- `gateway::tests::test_health_endpoint`
- `gateway::tests::test_session_not_found`
- `gateway::tests::test_sessions_endpoint_ok`
- `gateway::tests::test_status_endpoint`
- `gateway::tests::test_trigger_not_found`
- `gateway::tests::test_triggers_endpoint_empty`
- `gateway::tests::test_unknown_route_returns_404`

`gateway::tests::test_mcp_integration` now passes/skips without a Docker socket.

Prior findings:

- Finding 1, malformed `/mcp` body: fixed. `mcp_handler` now parses the raw body
  and returns JSON-RPC `-32700`.
- Finding 2, tool result schema: accepted-fixed per orchestrator ruling. The MCP
  content envelope is kept, and `structuredContent` is added alongside it for
  every tool result.
- Finding 3, `EventQuery` default limit in telemetry rollups: fixed.
  `agent_net_stats` and `fleet_rows` use `usize::MAX`.
- Finding 4, missing `jsonrpc == "2.0"` validation: fixed.
- Finding 5, poisoned telemetry mutex panics: fixed for the reviewed MCP/telemetry
  paths by recovering poisoned locks.
- Finding 6, new MCP integration test expands container failure set: fixed.

Ownership audit:

- Skunk fix commit `5d54d23` touched only `src/gateway.rs` and
  `src/telemetry.rs`.
- Trout dashboard commit `0f895fe` touched only `src/telemetry_web.rs`,
  `web/fleet.html`, and the `src/lib.rs` module append.
- `Cargo.toml` is untouched. Version is unchanged.

HTML external request audit:

- `web/fleet.html` has no external scripts, stylesheets, images, imports, or
  remote fetches. It only fetches relative `data.json`.

### High: `/fleet/data.json` `events` are summaries, not plan-compatible `agent_events`

File: `src/telemetry_web.rs:69`

The plan tool table defines `agent_events` as filtered events, newest first, a
thin wrapper over `EventQuery`. The dashboard blob documents itself as shaped
per `agent_events`, but serializes each event as:

```json
{"kind":"...","agent":"...","ts":123,"summary":"..."}
```

This drops the raw event fields and renames `agent_id` to `agent`. Consumers of
`/fleet/data.json` cannot treat `events` as the plan's `agent_events` output.

Repro: ingest an event with fields beyond `kind`, `agent_id`, `ts`, and a
summary source field, then call `GET /fleet/data.json`; the extra event fields
are absent from `events[]`.

Expected: newest-first raw event objects matching `EventQuery`/`agent_events`.

Actual: lossy dashboard summaries.

### Medium: `/fleet/data.json` can return `health: null` instead of the plan health object

File: `src/telemetry_web.rs:106`

The plan tool table defines `telemetry_health` as an object with
`events_path`, `indexed`, `newest_ts`, `lag_secs`, and `tagged_ratio`. The
dashboard schema uses `health: Option<TelemetryHealth>` and returns `null` when
the index is empty and the events file is missing.

Repro: run the dashboard with no `events.jsonl` present and request
`GET /fleet/data.json`.

Expected:

```json
{"health":{"events_path":"...","indexed":0,"newest_ts":0,"lag_secs":0,"tagged_ratio":1.0}}
```

Actual:

```json
{"health":null}
```

### Medium: `/fleet/data.json` fleet rows inherit the fixed telemetry limit bug

File: `src/telemetry_web.rs:131`

`build_fleet` calls `EventIndex::query` with `limit: 0`, which means
`DEFAULT_LIMIT` (`500`), not unlimited. This is the same class of bug fixed in
`src/telemetry.rs` for the MCP rollups. A busy agent can fill the newest 500
metric events and hide an older-but-still-indexed agent from the dashboard
fleet.

Repro: ingest 501 newer metric events for `agent-a`, then one older metric for
`agent-b`, and call `build_fleet`. `agent-b` is omitted.

Expected: dashboard fleet aggregation considers the full in-memory index, or
uses the shared fixed telemetry rollup.

Actual: dashboard fleet aggregation is capped at the newest 500 metric events.
