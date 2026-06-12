# awman (Agent Workflow Manager)

**nemesis8's nearest neighbor.** Same genus — a Rust single binary that runs AI
code agents in containers from the terminal — but specialized around the
**issue → merged PR** software-development lifecycle rather than as a general
sandbox/tooling platform.

> Formerly named `amux`. Migrates `AMUX_*` env → `AWMAN_*`.

## What it is

Four pillars (from its README):
1. **Isolate agents with containers _and git worktrees_** — each agent gets its
   own worktree so parallel agents on the same repo don't collide.
2. **Run multiple agents in parallel via a TUI.**
3. **Structured, repeatable workflows** for a project's SDLC — it ships an
   `aspec` spec format and a `templates/` directory.
4. **API mode** to fan workflows out to a homelab/cluster.

Distribution mirrors n8: `curl … | sh` installer, GitHub Releases with
per-platform binaries (linux/macos/windows, amd64/arm64), `mise` backend,
`make install` from source (Rust 1.94+).

## Stack
- Rust single binary (`Cargo.toml`, Rust 1.94+), `Makefile`, `Dockerfile.dev`.
- Subdirs of note: `aspec` (workflow/spec format), `templates`, `benches`,
  `tools`, `docs`. Version examined: 0.9.1.

## Overlap with nemesis8

| Capability | awman | nemesis8 |
|---|---|---|
| Language | Rust single binary | Rust single binary |
| Agent isolation | Containers **+ git worktrees** | Containers (Docker/Podman/WSL); one mounted workspace |
| Parallel agents | Yes, TUI | Yes, control room |
| Fleet / remote | "API mode" → homelab/cluster | gateway/`serve` + planned controller/worker registry |
| Core framing | **Workflow engine** (issue→PR) | Sandbox + tooling platform |
| Provider support | Claude/Codex/etc. | codex/gemini/claude/antigravity/ollama via TOML |

## What's worth borrowing

1. **A workflow engine (highest value).** awman treats an *issue→PR pipeline* as
   a first-class, repeatable object (its `aspec` + templates). n8 has all the raw
   pieces — `run`, gateway, cron/triggers, remote — but no structured "workflow"
   abstraction stitching them into a lifecycle. This is the biggest gap awman
   exposes.
2. **Git-worktree isolation.** Give each parallel agent its own worktree so N
   agents against the *same repo* don't stomp each other's working tree. n8
   isolates by container but mounts a single workspace; worktree-per-agent would
   make same-repo parallelism clean. (n8 already has worktree plumbing available
   via the harness for subagents — the concept transfers.)
3. **Overlays.** awman's layering model for composing workflow/config on top of a
   base — relevant if/when n8 grows a workflow abstraction.

## Where n8 is ahead / deliberately different
- MCP tooling ecosystem; pokeballs (sealable/shareable images); Hyperia
  (terminal) + Ferricula (storage) integrations; BM25 session search; broader
  runtime support (the Podman/WSL detection work); provider breadth via TOML.
- n8 is a **platform**; awman is a **workflow tool**. Adopting awman's workflow
  engine would *extend* n8, not redirect it.

## Candidate n8 work items (open later)
- [ ] **Workflow engine**: structured issue→PR pipelines as first-class objects. → [#45](https://github.com/DeepBlueDynamics/nemesis8/issues/45)
- [ ] **Worktree-per-agent** isolation for same-repo parallelism. → [#46](https://github.com/DeepBlueDynamics/nemesis8/issues/46)
- [ ] (stretch) **Overlays** for workflow/config composition.

## Attribution
- **Repo:** https://github.com/prettysmartdev/awman (formerly `amux`)
- **Author:** prettysmartdev
- **License:** Apache-2.0 — *idea reuse is unrestricted; copying code requires
  preserving LICENSE/NOTICE + stating changes (see [README](README.md) §3).*
- **Version examined:** 0.9.1 · **Local clone:** `code/deepbluedynamics/research/awman`
- **Examined:** 2026-06-08
