# Plan: service orchestration — n8 spawns its dependency services

> **Status (2026-06):** M0 (service_def + service_registry + `services/*.toml` +
> build.rs embed) and M1 (`ensure_service`/`list_services`/`stop_service` + the
> `n8 services` CLI) are **shipped**. M2–M4 (auto-ensure-on-launch, health, the
> credentials/broker seam) remain.

**Goal:** nemesis8 starts and supervises the **remote services** agents depend on
(Ferricula, Hyperia sidecar, shivvr, grub/wraith, opensearch, instructor, …) as
separate containers, from **declarative templates**, so a user with only Docker +
the `n8` binary gets a working stack without hand-running compose files.

This is the "how do we fire these up" half. The **credentials** half (scoped
tokens, broker, not-raw-env) is a separate concern — see
[mcp-planes-and-secrets](mcp-planes-and-secrets.md) *(to be written)*; this plan
leaves a clean seam for it (§M4) and does **not** solve it.

## Why

Today the agent container talks to remote MCP/services over HTTP
(`FERRICULA_URL=http://ferricula:8765`, `HYPERIA_URL=…:9800`, grub/wraith,
the n8 gateway, shivvr). Those services must already be running — started by
hand (`docker compose up` in `services/transcription`, the desktop Hyperia app,
etc.). There's no n8-native way to declare "these are my services" and have n8
ensure they're up on the shared network. Result: tools silently fail to connect
(the exact `ConnectError`/`No identity` class we just debugged) when a service
isn't running.

## Current state (verified)

n8 already has the engine — this plan adds *what to start and when*, not *how*:

- `DockerOps::create_container` + `start_container` (bollard) — used 3× already
  (agent run / run_capture / pokeball). Pull via bollard `create_image`.
- `DockerOps::ensure_network()` — creates the shared **`gnosis-network`** bridge;
  agents already join it (`--network=gnosis-network`), so services are reachable
  by **container name** (`ferricula:8765`).
- Label discovery — `LABEL_AGENT`/`agent_id`/… stamped on containers and matched
  in `list_containers()`. Reuse the pattern with a `nemesis8.service` label.
- `PokeballSpec` — an existing declarative container-template (image, runtime,
  ports, env_vars, volumes, build, health). The template precedent to mirror.
- `services/transcription/` — a real service with `docker-compose.yml` (CPU+GPU)
  and a Dockerfile; `config.integrations.{hyperia,ferricula}` already exist.
- Published images on Docker Hub: `deepbluedynamics/ferricula:0.10.3`,
  `…/nemesis8-base`, hyperia-sidecar (per its deploy spec), etc.

## Architecture

**Templates, not compose, as the source of truth.** A `services/*.toml`
registry (mirrors `providers/*.toml`), spawned via **native bollard** so it's
single-runtime (docker *and* podman over one socket), labeled (shows up in the
control room / registry), health-gated, and no compose-plugin dependency. The
per-service `docker-compose.yml` files stay as the human "do it by hand" path.

```
[service]
name = "ferricula"                          # container name + gnosis-network DNS
image = "deepbluedynamics/ferricula:0.10.3" # registry ref…
# build = { context = "../ferricula" }      # …or build for dev (mutually exclusive)
network = "gnosis-network"
restart = "unless-stopped"

ports   = ["127.0.0.1:8765:8765"]           # publish (loopback by default)
volumes = ["ferricula-data:/data"]          # named, persistent
env     = ["PORT=8765"]                     # plain env; secret REFS come in M4

[service.health]
test = "http://localhost:8765/status"       # in-container probe; gate "up" on it
interval_secs = 10
retries = 6

# Optional ordering; reconciled depends-first.
depends_on = []

# Which config keys does a running instance populate for agents?
# e.g. exports FERRICULA_URL=http://ferricula:8765 into agent build_env.
[service.exposes]
FERRICULA_URL = "http://ferricula:8765"
```

**Orchestrator = nemesis (`n8 serve` + a `n8 services` command).** `serve`
already owns the Docker socket and `ensure_network()`. It gains an
`ensure_services()` reconcile: for each enabled template, is a
`nemesis8.service=<name>` container running+healthy? If not → pull/build →
create+start on the network with the template's ports/env/volumes/labels → wait
healthy. Idempotent. This is the **same reconcile-against-`docker ps` pattern**
the control-plane plan uses for agents — services and agents share the lifecycle
machinery and the control-room surface.

## Milestones (incremental, each shippable + testable)

### M0 — Template format + registry
- New `src/service_def.rs` (`ServiceDef`/`ServiceSpec`, serde) mirroring
  `provider_def.rs`, and `src/service_registry.rs` mirroring `provider_registry`
  (embedded via build.rs include, + `~/.nemesis8/services/*.toml` user overrides).
- Ship `services/ferricula.toml` first (it's already imaged).
- Tests: parse all `services/*.toml`; round-trip the schema.

### M1 — `ensure_service` (native bollard) + `n8 services` CLI
- `DockerOps::ensure_service(spec) -> ServiceStatus`: label-check running →
  if absent, `create_image` (pull) or build → create_container (ports/env/
  volumes/labels/network/restart) → start → poll health.
- `DockerOps::list_services()` / `stop_service()` (label-filtered, mirrors
  `list_containers`).
- CLI: `n8 services up [name] | status | down [name] | logs <name>`.
- Verify: `n8 services up ferricula` pulls + starts it healthy; re-run is a
  no-op; `status` shows it; `down` stops it.

### M2 — Wire into `serve` + agent env
- `n8 serve` calls `ensure_network()` then `ensure_services()` for enabled
  services on startup (config decides which — extend `[integrations]` or a new
  `[services] enabled = [...]`).
- `build_env` derives the service URLs (FERRICULA_URL, etc.) from
  `[service.exposes]` of *running* services, instead of today's hardcoded/probed
  values — so the agent only gets URLs for services that are actually up.
- Verify: `n8 serve` brings up the declared stack; an agent session reaches
  `ferricula:8765` with no manual compose.

### M3 — Reconcile loop + control-room surface
- Periodic `ensure_services()` tick (restart unhealthy; honor `restart` policy).
- Services render in the control room beside agents (state/health glyphs from the
  theme taxonomy), sharing the registry/reconcile machinery (control-plane M2).
- Verify: kill a service container → next tick restarts it; control room shows
  health flapping then green.

### M4 — Secrets seam (depends on the broker plan)
- `env` entries become secret **refs** (`{ secret = "ferricula-token" }`) resolved
  by the token broker at spawn, not raw values in the template or the agent env.
- Verify: a service that needs a token starts with it injected, and the token is
  not present in the agent container's environment.

### M5 — Full service set
- Templates for `hyperia-sidecar`, `shivvr`, `grub`/`wraith`, `opensearch`,
  `gnosis-instructor`, with `depends_on` ordering (e.g. sidecar depends_on
  ferricula). Map each agent MCP tool's URL/token to its service.
- Note: the *desktop* Hyperia is a host app on `host.docker.internal`, NOT a
  managed container — templates cover managed-container services only; host
  services stay a separate, already-handled case.

## Critical files
- New: `src/service_def.rs`, `src/service_registry.rs`, `services/*.toml`,
  `src/cli.rs` (`Services` subcommand).
- Modify: `src/docker.rs` (`ensure_service`/`list_services`/`stop_service`,
  reuse `ensure_network` + labels), `src/gateway.rs`/`serve` (ensure on startup +
  reconcile tick), `src/main.rs` (`build_env` URL derivation; `Services` dispatch),
  `build.rs` (embed `services/*.toml`), `src/controlroom.rs` (M3 surface).

## Verification (per-milestone above) + overall
- `n8 services up` on a clean machine with only Docker pulls + starts Ferricula
  healthy; an agent session then reaches it with zero manual steps.
- Re-runs are idempotent; `down` cleans up; killed services self-heal under serve.
- `cargo test --lib` green; works under both Docker and Podman (one socket path).

## Risks / decisions
- **bollard vs compose** — decided: native bollard (single runtime, labels,
  registry integration). Compose files remain the manual escape hatch.
- **Image source** — registry ref (deploy) vs build context (dev). Template
  supports both; default to the published image, fall back to build if `build` set
  and the image isn't pullable.
- **Volumes/ports collisions** — a host-run service already on a port (desktop
  Hyperia on 9800) must not be stomped: `ensure_service` should detect an
  existing healthy listener and skip spawning a duplicate (treat as "external,
  already up").
- **Lifecycle ownership** — services started by `n8 serve` outlive a single
  agent; `n8 services down` / serve stop is the cleanup. Don't tie service
  lifetime to one agent container.
- **Secrets** — explicitly deferred to M4 + the broker plan; M0–M3 use plain env
  so they're shippable without blocking on the secrets design.
