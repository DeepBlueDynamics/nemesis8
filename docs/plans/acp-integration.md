# Plan: ACP integration — `nemesis8 acp` (editor → containerized agent)

Let a host editor (Zed, VS Code, any ACP client) drive a coding agent that runs
**inside an n8 container**, by speaking **ACP** (Agent Client Protocol —
JSON-RPC over stdio) through a thin `nemesis8 acp` proxy. The provider's *native*
ACP does the work; n8 is a transparent pipe plus the sandbox + config layer.

## Why this shape (credit)
Borrowed from a [HN comment by **andy_xor_andrew**](../competitive/acp-harness.md)
on the "Migrate from OpenClaw" thread: *the closer you can get to a barebones
"coding-agent core" plus "gateways that point to it," the better* — and ACP is
the universal adapter, so one thin gateway supports any CLI that exposes ACP
(opencode, copilot cli, …). See `docs/competitive/acp-harness.md` for the full
brief. **ACP is stdio, not a port**, so unlike the reverse-tunnel work this needs
no port negotiation at all.

## Decision (locked in scoping)
Approach **1** — a stdio proxy to the provider's own ACP — **not** Approach 2
(n8 itself becoming an ACP *server* that translates to non-ACP agents). See the
connectivity plan, Part 2, for why 2 is deferred.

## How ACP works (the relevant bit)
An ACP client (editor) launches the agent as a **subprocess** and speaks JSON-RPC
over its **stdin/stdout** (`initialize`, prompt/turn, file read/write, cancel, …).
`nemesis8 acp` *is* that subprocess from the editor's point of view — internally
it runs the agent in a container and pipes the stream through.

## Components
1. **Provider spec** — add `acp_subcommand: Option<String>` to `PromptSpec`
   (`provider_def.rs`). A provider that speaks ACP declares it; e.g.
   `providers/opencode.toml`:
   ```toml
   [provider.prompt]
   exec_subcommand = "run"
   acp_subcommand  = "acp"     # `opencode acp` starts its ACP server on stdio
   ```
   No per-provider Rust. `nemesis8 acp` errors clearly when a provider has none.

2. **`nemesis8 acp` command** (`cli.rs` + `main.rs`)
   - Args: `--provider <name>` (else config default), `--workspace` (default cwd),
     `--danger`, optional `--session <id>`.
   - `ensure_image`; reuse `build_env` + `build_host_config` (same workspace mount,
     env, MCP wiring as a normal run — so the agent has its tools).
   - Launch with **`-i` and NO TTY** (`--rm`), entrypoint `nemesis8-entry --acp`.
     `-it` is wrong here — this is a JSON-RPC pipe, not a terminal.
   - Wire **host stdin → container stdin** and **container stdout → host stdout**
     1:1, transparently. **stderr → a log file**, never the editor's stdout.

3. **`nemesis8-entry --acp` mode** (`entry.rs`)
   - Run the normal `write_provider_config` setup (MCP servers, danger merge, …)
     so the agent starts configured, **then `exec` `<binary> <acp_subcommand>`**
     with stdio inherited. entry's own logs already go to stderr — keep it that
     way so they never corrupt the JSON-RPC stdout stream.

4. **Editor config** (docs) — e.g. Zed `settings.json` registers a custom ACP
   agent whose command is `n8 acp --provider opencode`. Document Zed (the ACP
   reference client) + the generic shape for other ACP clients.

## The one real risk: stdout hygiene
The JSON-RPC stream **is** stdout. Nothing in n8 / entry / docker may write to the
editor's stdout except the agent's ACP output:
- entry diagnostics → stderr (already true).
- `docker run` must not emit pull/progress to stdout in acp mode (suppress or
  route to stderr).
- No banner / resume-hint / prompt printing in acp mode.
A single stray `println!` corrupts the protocol — gate acp mode tightly.

## Lifecycle
Editor connects → `n8 acp` starts a fresh `-i --rm` container + agent → ACP
session runs → editor closes stdin (EOF) → agent exits → container exits. One
container per ACP session. (Recording the session like interactive runs is
optional follow-up.)

## Phasing & effort (~1 day)
- **P1** `acp_subcommand` field + opencode.toml. (~30 min)
- **P2** `nemesis8 acp` command + `entry --acp` + the stdio pipe. (~half day)
- **P3** Zed/VS Code config docs + stdout-hygiene test (drive opencode from Zed).
  (~couple hours)

## Open questions
- First-class editor: **Zed** (ACP's reference client) — confirm.
- `opencode acp` must be stdio-only (no TTY needs) — verify on first wire-up.
- Fresh container per session (simplest) vs attach to an existing session's
  container — start with fresh.

## Out of scope
- Approach 2 (n8 as an ACP server for non-ACP providers) — deferred; see
  `serve-port-tunnel.md` Part 2.
- This is standalone: it does **not** depend on the reverse tunnel or the service
  registry, so it can ship first.
