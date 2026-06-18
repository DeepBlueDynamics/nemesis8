# ACP-based Container Harnesses

**A minimalist approach to AI coding agents.** The concept centers on running barebones coding agent CLIs in container isolation, communicating with clients (harnesses, UIs, or editors) via the standardized **Agent Client Protocol (ACP)**.

## What it is

Instead of creating monolithic agents with large default skillsets, telemetry, or complex memory systems, this architectural pattern focuses on:
1. **Separation of Concerns:** Keep the core agent CLI as thin and simple as possible.
2. **Standardized Protocol:** Use ACP (LSP-like JSON-RPC protocol) for all communication between the editor/UI and the agent core.
3. **Gateway Architecture:** Connect gateways or lightweight wrappers (harnesses) to this containerized core.

This prevents the agent context from being bloated with unused skills (e.g., the 10k+ tokens context overhead in default Hermes loads) and provides clean, pluggable editor support.

## Stack
- Standardized ACP endpoints (JSON-RPC over stdio or WebSockets).
- Lightweight local containers (Docker/Podman) running the agent CLIs.

## Overlap with nemesis8

| Design Pattern | ACP Harness / Gateways | nemesis8 |
|---|---|---|
| **Core Architecture** | Thin agent core + external gateways | Containerized CLI runtime + HTTP gateway/scheduler |
| **Protocol** | ACP (standard JSON-RPC) | HTTP REST API / TUI terminal streams |
| **Agent Support** | Supports any agent exposing ACP | Supports Codex, Claude Code, Gemini, Grok, etc., via spec configurations |

## What's worth borrowing / implementing

1. **Host→container ACP bridge (`nemesis8 acp`) — chosen, this is the build.**
   A `nemesis8 acp --provider <p>` command pipes the editor's `stdin`/`stdout` to
   the containerized agent's **native** ACP process, so host IDEs (Zed et al.)
   connect straight to the sandboxed agent. ACP is stdio, so no port/tunnel is
   needed. Full plan: [`docs/plans/acp-integration.md`](../plans/acp-integration.md).
   Data-driven hook: an `acp_subcommand` field in the provider TOML.
2. **ACP gateway server (`nemesis8 serve --acp`) — deferred.** Making n8 *itself*
   an ACP server that translates to any provider (even non-ACP ones) is a large
   reimplementation that competes with providers' own native ACP — and the trend
   is everyone going ACP-native. Revisit only if we hit an agent that will never
   speak ACP. (Approach 2 in the plan.)

## What nemesis8 brings (the brief)

nemesis8 already *is* "thin agent cores + gateways pointing at them" — the agent
CLIs are the cores; n8 owns no agent logic. What it adds on top of a few-hour ACP
harness:

- **Sandboxed, per-agent Docker containers** — isolation, workspace mounts,
  danger mode — not just a subprocess.
- **Data-driven, any-CLI registry** (`providers/*.toml`) + a **socket/stdio MCP
  registry** (`mcp-servers/*.toml`): add an agent CLI or a tool by dropping a
  TOML, no rebuild, no per-provider code.
- **A connectivity broker** (`serve`): reverse tunnel so the host can reach a
  server the agent built; the **`nemesis8 acp`** editor gateway (this); and
  container-to-container over a shared network — plus **Hyperia** for
  *consent-gated* agent↔agent over the shared TTY. (See
  [`serve-port-tunnel.md`](../plans/serve-port-tunnel.md).)
- **A fleet control room** — sessions, running agents, a tools picker.

So `nemesis8 acp` lands andy_xor_andrew's idea — *ACP gateway to a thin
containerized core* — with isolation, multi-agent, and pluggable tooling around
it. **Repo: https://github.com/DeepBlueDynamics/nemesis8**

## Attribution
- **Source:** Hacker News Thread: [Migrate from OpenClaw](https://news.ycombinator.com/item?id=48586005#48586519)
- **Author/Idea:** Comment by `andy_xor_andrew` (discussing a custom ACP harness wrapping containerized CLIs like Copilot CLI and OpenCode).
- **Date:** 2026-06-18
