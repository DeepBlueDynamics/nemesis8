# Capsule catalog — n8 as the service substrate Hyperia deploys through

> The pokeball idea, made concrete. n8 already knows how to describe, launch,
> and reconcile containers; this plan unifies *every* deployable thing behind
> one homogeneous model (a **Capsule**) and one gateway surface, so a client
> like Hyperia installs n8, asks it "what can I deploy and what's running,"
> and drives docker/podman entirely through n8 — never the socket directly.

## The flow this enables

```
Hyperia (or any client)
  1. installs nemesis8            (curl | sh — one binary)
  2. GET /catalog                 → every Capsule: available · running · external · in-process
  3. throws a capsule             → POST /catalog/<id>/up   (agent, service, or app)
  4. recalls it                   → POST /catalog/<id>/down
  5. watches                      → /fleet SSE + /catalog re-poll
```

n8 owns the container runtime (docker OR podman, already abstracted via
`runtime_binary` + `host_gateway_alias`). Hyperia owns the UI and the human.
Neither product calls the other's internals — n8 exposes a catalog, Hyperia
consumes it. Same convention-over-connection posture as the secrets plan.

## The abstraction: Capsule

One schema describes anything n8 can surface or launch. A capsule has a
**kind**, a **state**, and enough identity to act on it.

```jsonc
{
  "id": "ferricula",              // stable handle
  "kind": "service",              // agent | app | service | external | inproc
  "name": "Ferricula",
  "state": "up",                  // available | running | up | down | not-configured
  "source": "services/ferricula.toml",   // where the definition lives (or "probe"/"builtin")
  "endpoint": "http://host.docker.internal:8773",  // when addressable
  "detail": { "image": "...", "workspace": "...", "model": "...", "last_seen": "..." }
}
```

The five kinds, and where each already comes from in n8 today:

| kind | what | source of truth (exists now) |
|---|---|---|
| **agent** | an AI CLI in a container | `providers/*.toml` (available) ⊕ running containers by label (running) |
| **app** | foreground non-AI tool (glint) | `apps/*.toml` |
| **service** | background dependency container | `services/*.toml` (chisel, ferricula, transcription) |
| **external** | something n8 didn't launch but can reach | `[integrations]` + config probes (Ollama :11434, Ferricula, Sailfish, Whisper) |
| **inproc** | in-process module, no port | declared, self-reported (Kokoro TTS, Maximus) — invisible to port probes, so it must be *declared* not discovered |

The pokeball intuition: `providers/`, `apps/`, `services/` are the capsules on
the belt; `/catalog/<id>/up` throws one; the running/external/inproc states are
what's currently out. **No new authoring format** — the three TOML taxonomies
already exist; the Capsule is the read-model over them plus live state.

## The catalog engine

New `src/catalog.rs` — a pure `catalog()` that merges four sources into one
`Vec<Capsule>`, each source already implemented somewhere in the tree:

1. **Declarative (available)** — `ProviderRegistry`, apps registry,
   `ServiceRegistry` → capsules in state `available`.
2. **Running** — `docker/podman ps` reconciled by `nemesis8.*` labels (the
   fleet already does this) → flips matching capsules to `running`, adds
   unrecognized-but-labeled ones.
3. **External probes** — for each `[integrations]` entry and known external
   (Ollama, Ferricula, Sailfish, Whisper), a bounded TCP/HTTP health check →
   `up` / `down` / `not-configured`. **Honest states, never a hardcoded port**
   (this is the exact defect in Hyperia's dashboard: a static card said
   Ferricula :8765 while the real snapshot carried :8773 — the catalog reads
   the configured endpoint, so it can't drift).
4. **In-process** — capsules that declare `kind = "inproc"` with a
   liveness field the owning process updates (Kokoro: model present +
   last-spoke; Maximus: enabled + model + last-extraction). Declared because a
   port probe can't see them.

Bounded, cached, degrades gracefully: a down external is a `down` capsule, not
an error; a missing runtime is an empty running-set, not a failure.

## The gateway surface

`src/gateway.rs` — inside the existing auth layer, no new host port:

| route | method | what |
|---|---|---|
| `/catalog` | GET | the full `Vec<Capsule>` (JSON) — the single registry |
| `/catalog/:id/up` | POST | throw: spawn agent / `ensure_service` / launch app |
| `/catalog/:id/down` | POST | recall: stop by id |
| MCP `catalog_list` / `catalog_up` / `catalog_down` | — | same, as fleet MCP tools |

`/catalog` **subsumes** the three-registry disagreement Hyperia's dashboard
hit: one endpoint both the dashboard and agent-config consume, so Ollama,
Ferricula (real port), Sailfish, Whisper, Kokoro, and Maximus all report from
the same source with the same honest states. It replaces the ad-hoc
`GET /api/services` the dashboard plan wanted with n8 as the authority.

MCP-spec note: catalog results are `structuredContent` **objects**
(`{"capsules": [...]}`), not bare arrays — the same wrapper rule the fleet
tools now follow (fixed `8f42a3a`); never repeat the bare-array bug.

## Telemetry: connecting n8's data to Hyperia

The Hyperia agent's diagnosis is correct in shape — "n8 has the data, the wire
doesn't exist" — but two of its three findings are **already fixed in our tree
and only look broken because it is testing a pre-`8f42a3a` gateway**:

| finding | reality | action |
|---|---|---|
| `agent_net`/`fleet_status` return a bare array → schema-validating clients reject | **FIXED** `8f42a3a`: wrapped as `{"agents": [...]}` | Hyperia rebuilds/updates n8; the **pull path works immediately** |
| container→pane token can't refresh on re-attach | **SHIPPED** `8f42a3a`: token written to `/opt/nemesis8/.n8/tokens/<container>` on every attach | agents re-read the file (BASE #9) |
| n8 → Hyperia telemetry **push** never built | true — no producer | **this plan builds it (Phase 4)** |

So the immediate unblock is a **stale gateway**, not new work: rebuilt n8 makes
every fleet/catalog MCP tool consumable today. The push path is the only genuine
new connector, and everything it needs, we shipped this week:

- **Per-pane attribution** — the pane-binding file
  (`/opt/nemesis8/.n8/panes/<container>`, rewritten on every attach) gives the
  *current* pane for each container, which is exactly the envelope key
  `/api/telemetry/event` wants. (A container in no pane → no push; in a new
  pane → correctly re-attributed. This is the container↔pane mapping the
  Hyperia agent flagged as "needed" — it exists now.)
- **Auth** — the persistent minted `hyp_agent_*` token the container already
  holds authenticates the POST.
- **Data** — the monitor already tracks `net_rx_bps`/`net_tx_bps` and the event
  stream; token counts come from the transcript tail (`tool_events.rs` already
  reads these files) rather than being invented.

### Push connector design (Phase 4)

Opt-in, fire-and-forget, **never blocks or couples**:

- A gateway-side task batches per-agent metrics on the monitor's existing tick,
  maps each container→pane via the binding file, and POSTs a per-pane envelope
  to `HYPERIA_URL/api/telemetry/event` with the minted token.
- **n8 must run fine with Hyperia absent** (the whole design rule): push is
  gated on `[integrations].hyperia` + a reachable endpoint; failure is a
  debug log, never a stall. When Hyperia is down, the pull path (fleet MCP) is
  the fallback and loses nothing.
- **Cadence + backpressure**: batch on the metrics interval (~5s), coalesce,
  drop-oldest on send failure — never queue unboundedly.
- **Producers, honestly**: net + events have producers today. Token usage does
  NOT yet flow into monitor events — the harness footers count it, but n8
  doesn't capture it. Phase 4b adds a token producer (scrape the per-turn usage
  from the transcript tail we already read) so "Token Usage" shows real numbers
  instead of the honest-but-empty "no producers reporting."
- **Cards tell the truth meanwhile**: until a producer exists for a metric,
  the surface says *no data connected* (not zeros dressed as data). The catalog
  and fleet already carry real net/state/uptime, so most cards light up on the
  rebuild alone.

## Phases

1. **Capsule + catalog engine** (`src/catalog.rs`) — the read-model over the
   three registries + running reconcile + external probes + inproc declarations.
   Unit tests over synthetic registry/probe mixes. No new surface yet.
2. **Gateway surface** — `/catalog`, `/catalog/:id/{up,down}`, the three MCP
   tools (object-wrapped results). `n8 catalog` CLI for parity.
3. **Hyperia consumes** — hand them one endpoint (`GET /catalog`) + the two
   action routes. Their install-n8-then-discover flow works against it. This
   also retires their dashboard's three-registry disagreement.
4. **Telemetry push connector** — the opt-in pusher above (4a: net/events;
   4b: token producer from the transcript tail).
5. **Later** — capsule *definitions* Hyperia can author (drop a `services/`
   TOML via the gateway); cross-host catalog federation (the control-plane
   registry already spans hosts).

## Acceptance

- `GET /catalog` returns one array covering: installable agents/apps/services,
  live containers by label, external probes with honest up/down/not-configured
  (Ferricula at its **configured** port, never a hardcoded one), and declared
  inproc modules. No duplicates, no drift between what two cards claim.
- `POST /catalog/ferricula/up` starts it; `/down` stops it — same result as
  `n8 services up ferricula`, now driveable by a remote client.
- A rebuilt n8 makes every fleet/catalog MCP tool consumable by a
  schema-validating client (the `8f42a3a` wrapper) — verified with a
  strict client, no bare arrays.
- With push enabled: a per-pane envelope reaches `/api/telemetry/event` with
  correct pane attribution and the minted token; with Hyperia **down**, n8
  launches and serves the catalog unaffected.
- Every metric card shows real data OR "no data connected" — never a zero
  pretending to be a measurement.

## Touchpoints

**new** `src/catalog.rs`, `docs/plans/capsule-catalog.md` · `src/gateway.rs`
(routes + MCP tools + push task) · `src/service_registry.rs` /
`provider_registry.rs` / apps registry (expose `available` lists) ·
`src/config.rs` (external-probe endpoints from `[integrations]`) ·
`mcp-servers/` (catalog registry def, opt-in) · `n8 catalog` CLI. Nothing
container-side; nothing that couples n8 to Hyperia's presence.
