# nemesis8 — working-state handoff

> For any agent (or human) picking up work. Snapshot: **2026-08-19, v0.19.3.**
>
> **This file is part of the work.** When you finish an arc that changes
> state — a release, a fix train, a new surface, a new rule from the owner —
> update this file in the same push. An incoming session reads CLAUDE.md,
> then this, then the runbooks it points at. If you leave it stale, the next
> agent relearns your week the hard way.

## Current state

- **Released**: v0.19.5 (signed, Latest; 0.19.4 minutes before it) — persistent Hyperia container
  identities, session self-truth (workspaces from provider records, not the
  racing index), quick-resume overlay + `resume last`, per-provider model
  defaults, local-model enumeration fix, `--rust` image option, system-prompt
  delivery to every CLI's real instructions file, declarative provider layouts.
- 0.19.4: sessions --json, logs→stderr, build.sh gates, MCP structuredContent
  spec fix, token-file rebind, containers-tab search. 0.19.5: the tunnel-port
  collision fix — chisel sidecar moved to 9803 (gateway+1 landed on the
  trainer when the port family moved; tunnels were silently dead since July —
  mappings stayed "pending" while expose_port reported success).
- **Dev box**: host `n8` + gateway daemon = HEAD build; local image current
  through the glm enumeration fix once the owner's latest `n8 build` ran.

## The port family (fixed convention, do not invent ports)

| port | what | bind |
|---|---|---|
| 9800 | Hyperia sidecar (theirs) | loopback |
| **9801** | n8 gateway — REST + `POST /mcp` (fleet tools) + `/fleet` dashboard + SSE | 0.0.0.0 |
| **9802** | trainer API (Sailfish tool-run data) | 127.0.0.1 ONLY (private transcripts) |
| **9803** | chisel reverse-server (tunnel data plane; exposed ports live in 18000-18999) | 0.0.0.0 |
| 9124 | Meridian sidecar (theirs) | loopback |
| 18000-18999 | chisel exposure range — do not squat | — |

One definition each: `gateway::DEFAULT_PORT` / `trainer_api::TRAINER_PORT`.

## Observability surfaces (all shipped)

- **MCP** `:9801/mcp`: `fleet_status · agent_events · agent_net ·
  event_facets · telemetry_health` (results carry `structuredContent`).
- **Dashboard** `:9801/fleet` (source: `web/fleet.html`, embedded via
  `telemetry_web.rs`) + `/fleet/data.json` + SSE stream.
- **Search**: `q=` routes to the lume store (`src/event_store.rs`); tool_call
  events synthesized from transcripts (`src/tool_events.rs`; agy is protobuf,
  #90). cpu/mem/net from the RUNTIME stats API, never in-container /proc.
- **CLI for agents**: `n8 sessions <query> --json` (BM25 over transcript
  content, pure JSON on stdout; all logs go to stderr).

## Working rules (hard-won, do not relearn)

1. **No provider/model specifics in shared code.** Dialects and layouts are
   DECLARED in `providers/*.toml` — headers key (`mcp_headers_key`), env-ref
   auth, session layouts (`session_canonical_file`, `session_db_*`),
   workspace truth (`workspace_probes`), prompt delivery (`write_to_file`),
   model env. Shared Rust holds only generic engines. This rule has been
   violated and corrected twice (grok headers; antigravity brain paths) —
   don't be the third.
2. Gates before any commit: `scripts/build.sh --agent --host-only`
   (release build · full tests · `n8 mcp test` · gateway smoke). Bump via
   `scripts/bump.sh` BEFORE building; PATCH by default, MINOR only when the
   owner says so. Runbooks: `docs/RELEASING.md`, `docs/PROVIDER-TESTING.md`.
3. Owner rulings in force: **never mock data** · **timestamps Zulu+date,
   first column, newest-first** · **search surfaces everything a container
   does** · **runtime stats over /proc** · **agents missing a tool ask for a
   terminal instead of failing** (BASE guardrail #8).
4. **Session truth**: a session's workspace comes from the provider's own
   record. The recorder index is a last-resort fallback and must never claim
   sessions that self-resolve (the one-instance-stamps-everything epidemic,
   fixed 2026-08-19). Hyperia tokens: `hyp_pane_*` die on pane close AND
   sidecar restart; containers get minted `hyp_agent_*` identities
   (`nemesis8/<workspace>`) at launch — see #104 for the small print.
5. Multi-agent work: strict file ownership, private `CARGO_TARGET_DIR`,
   no Cargo.toml edits by pane agents. Kill n8 processes by exact PID only.
   Launch hangs: check `docker image inspect nemesis8:latest` entrypoint
   FIRST (tag-clobber incident, #92).
6. Swapping the installed binary on Windows: RENAME the running exe, copy the
   new one in — running panes survive, new launches get the new build.
   Restart the gateway daemon afterward or it keeps serving the old code.

## In-flight / parked

- `mcp-bins/hyperia-cli.js` working-tree diff — #75, not n8-agent-owned.
- **`feat/klaussy`** — another agent's MCP-server feature, parked verbatim.
- **`feat/sigil-toolhost`** (worktree `../nemesis8-sigil`) — Phase-0 Sigil
  router seam, implemented + gated, uncommitted by design; blocked on Sigil
  contract decisions (see `docs/plans/sigil-integration.md`).

## Open issue map (2026-08-19)

- **Architecture follow-ups**: #103 per-container session-id correlation
  (write-side race fully mitigated; this is the completing design) · #104
  per-container Hyperia identity small print (rotation caveats recorded).
- **Fresh follow-ups**: #92 image identity check · #93 launch-time dependency
  probe · #94 container-path records · #96 trainer zip · #97 model column.
- **Planned, briefs ready**: #85 podman 6 · #86 Hyperia observability wiring ·
  #87 LOGPANE net panel · #52 secrets store (all in `docs/plans/`).
- **Owner decisions**: #88 ACP keep-or-kill · TUI v3 train (#40-46) ·
  April tail (#19-33) sweep.
- **Analytics arc**: #77 epic → #81/#82/#83 (lume store is the seed).

## Cross-repo contracts

- **Hyperia**: consumes `:9801/mcp`; containers hold persistent minted
  identities and read their host pane from
  `/opt/nemesis8/.n8/panes/$NEMESIS8_AGENT_ID` (rewritten on every attach).
  Optional Hyperia-side assist (token→pane API) offered, not required.
- **Sigil**: evaluation seam lives on `feat/sigil-toolhost`; five open
  contract decisions listed in `docs/plans/sigil-integration.md`.
- **Meridian**: shim `MCP/meridian-mcp.py` + `mcp-servers/meridian.toml` (:9124).
- **Sailfish**: trainer API :9802 (Part 1 of their spec).
- **Secrets interop**: shared OS-keychain namespace `deepbluedynamics`,
  convention over connection — NEITHER product calls the other (#52 plan).
