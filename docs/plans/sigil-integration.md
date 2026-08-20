# Sigil integration

"Pokeball / capsule" was the working name for abstracting the containers. That
idea is now **Sigil**. Sigil is the smart container/tool layer.

Repo: `deepbluedynamics/sigil`. Rust agent framework. Three binaries:
- `sigil` — the agent (builds/edits code, cargo-gated, workspace-confined).
- `sigil_mcp` — an MCP **server** that exposes Sigil to MCP clients.
- `sigilctl` — fleet launcher (isolated builders, no docker socket).

Facts that matter:
- Sigil is also an MCP **client**: it connects to remote MCP tool servers and
  absorbs their tools live — adds, schema changes, removals, no reconnect.
- Model is env-config: `SIGIL_LM_ENDPOINT`, `SIGIL_LM_MODEL`,
  `SIGIL_LM_API_KEY` (or `OPENAI_API_KEY`). Any model — Claude, OpenAI
  gpt-5.6-sol, local Ollama.
- Workspace-confined by design (temp git worktrees, traversal-rejecting patch
  policy). Wants no docker socket.

## The choice we're adding

Per workspace, picked in the TUI MCP tools picker, one of:

- **Stock MCP** (today): the agent forks the built-in MCP servers itself —
  nuts-files, shivvr, ask, n8gw. One process per tool, per container, per init.
- **Sigil**: the agent talks to **one** Sigil container. Sigil absorbs those
  same tools and runs them, driven by its own agent (configurable model —
  default gpt-5.6-sol). Sigil also manages the containers it builds.

## The wire

```
n8 agent ── MCP ──► sigil_mcp  (Sigil container)
                       │  Sigil is an MCP CLIENT of the same tools:
                       ├─► nuts-files   file read/edit
                       ├─► shivvr       embeddings
                       ├─► ask          second opinion
                       └─► n8gw         gateway control
```

- The agent sees ONE MCP server (`sigil`). Sigil has folded the four in.
- Example: file reads/edits currently done by nuts-files (gnosis-files) — Sigil
  absorbs them, runs them from its own container, and can gate on top (cargo
  check/test before it claims an edit is good). One Sigil container instead of
  four stdio servers per agent.
- Sigil sees `n8gw`, so it knows the gateway and can spawn/manage containers
  through it — `sigilctl fleet` → n8's `agent_spawn` — never the docker socket.

## Pieces to build

1. **Sigil as a capsule** — `services/sigil.toml` (or `providers/sigil.toml`).
   Build from source (cargo, bookworm-compatible) or its own Dockerfile.
   Opt-in, off by default.
2. **Sigil config** — model + token in workspace config / `[env]`. Default:
   gpt-5.6-sol via `OPENAI_API_KEY` (or point `SIGIL_LM_ENDPOINT` at local
   Ollama). This is Sigil's "agent configuration" — whose brain runs the tools.
3. **TUI picker toggle** — add "Sigil" to the tools picker. On writes
   `sigil.enabled = true` into `.nemesis8.toml`.
4. **Config gate** — when Sigil is on, the generated agent config drops the 4
   built-in MCP registrations and writes ONE `sigil` registration pointing at
   the Sigil container's `sigil_mcp` endpoint.
5. **Sigil's downstream config** — tell Sigil which MCP servers to absorb
   (the nuts-files / shivvr / ask / n8gw endpoints). n8 writes this when it
   launches the Sigil container.

## Reuse the existing branch

`feat/sigil-toolhost` already built step 4: a config flag that replaces the 4
built-ins with one `sigil` registration, honors `disabled_builtins`, defaults
off, gated + tested. That mechanism is correct — I earlier called the branch
dead-premise, but the user's design makes it the right shape. The only change:
the registration points at Sigil's **real** `sigil_mcp` endpoint, and Sigil
does the absorbing (there is no separate router to invent). Un-park it, repoint
it, drop the five "open contract" questions — the env contract is published in
Sigil's `sigil_mcp` header.

## Telemetry — important, but NOT the focus here

n8 collects the data; the wire to Hyperia is what's missing.

- **Fix the producers.** File-change events and network: n8 has these (the
  monitor). Tokens: no producer yet — scrape per-turn usage from the transcript
  tail (`tool_events.rs` already reads those files).
- **Expose it. Easiest = a stream.** n8 already serves `/fleet/events/stream`
  (SSE). Point Hyperia at that URL; it connects and reads. No push, no polling
  to build. (Push into `/api/telemetry/event` is the alternative — more work,
  cleaner pane attribution — but the stream is the fast path.)
- The pull MCP tools (`fleet_status`, `agent_net`) are **already schema-fixed**
  (`8f42a3a`). If Hyperia sees bare arrays, it's on a stale gateway — rebuild.

## Order

1. Sigil capsule + config (launch a Sigil container, model configurable).
2. Un-park `feat/sigil-toolhost`, repoint the registration at `sigil_mcp`.
3. TUI picker toggle: stock vs Sigil.
4. Sigil absorbs the built-in tool endpoints; verify one Sigil container serves
   file ops end to end from the MCP endpoint.
5. (Separate track) telemetry stream to Hyperia.
