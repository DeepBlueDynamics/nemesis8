# Nemesis 8

**Run every AI coding agent. In containers. At once.**

One binary. Eleven providers. A fleet you can actually see.

[nemesis8.nuts.services](https://nemesis8.nuts.services)

---

## What is this?

nemesis8 wraps AI coding CLIs — Codex, Claude Code, Grok, Antigravity, OpenCode, and friends — in Docker/Podman containers with persistent sessions, durable identities, a curated bench of MCP tools, and a gateway that knows what every agent is doing. Point it at a project and it handles the image, the tools, the credentials, the sessions, and the cleanup.

Your agents get superpowers. Your laptop keeps its boundaries. Everything that happens is yours to inspect.

## Install

**Windows:**
```powershell
powershell -c "irm https://nemesis8.nuts.services/install.ps1 | iex"
```

**Linux / macOS:**
```bash
curl -fsSL https://nemesis8.nuts.services/install.sh | sh
```

**From source:** `cargo install --path .`

**Prerequisites:** Docker or Podman (or a remote gateway — then you need nothing). API keys only if your provider wants them.

## Sixty seconds in

```bash
n8                          # home screen: new session or jump back into anything
n8 --danger                 # skip approvals; the container is the seatbelt
n8 --provider grok          # same workspace, different brain
n8 resume                   # centered last-10 overlay — running, suspended, saved.
                            #   ⏎ or 1–9 and you're back in. `m` for the full picker.
n8 resume last              # zero UI, straight into the newest session
n8 run "kill the TODOs"     # one-shot
n8 shell                    # drop into the container
n8 build                    # rebuild the agent image (checkbox picker: rust, gpu, ffmpeg…)
```

## Sessions that tell the truth

Every provider's sessions are tracked, listed, and resumable. Each session's workspace comes from the provider's **own record** — codex's rollout, grok's path encoding, opencode's db, antigravity's transcript — so ten agents across ten projects can't mislabel each other's history.

Resume auto-detects the provider, offers to switch you into the session's original directory, and brings the model back with the session.

Containers are detached from their terminals. Close the pane, kill the terminal, come back tomorrow — the agent kept working, and attaching is one keystroke.

## Providers

| Provider | What runs |
|---|---|
| **codex** | OpenAI Codex CLI |
| **claude** | Claude Code |
| **antigravity** | Google's Antigravity (`agy`) |
| **grok** | xAI Grok Build — with telemetry/codebase-upload **hard-disabled**; private repos stay private |
| **opencode** | OpenCode — multi-backend, first-class **local models via Ollama** (GLM, Qwen, Gemma… auto-enumerated, defaultable) |
| **hermes** | Nous Research Hermes |
| **pi** | Pi coding agent |
| **sakana** | codex driving Sakana Fugu (1M-token context) |
| **omp** | Oh My Pi — LSP, debugger, browser, subagents, 60+ backends, native MCP |
| **fx** | Vercel's fx — native (Zig) agent; Codex/Grok subscriptions or AI Gateway |
| **hax** | Minimalist C agent — first-class **local models** (Ollama, llama.cpp) |

Every provider is one TOML file: config dialect, session layout, workspace records, prompt delivery, and MCP quirks are all declared, with no per-provider Rust. To add one, write a TOML file — see [docs/adding-a-provider.md](docs/adding-a-provider.md). CLIs auto-update at container start.

**New: omp and fx.** [omp (Oh My Pi)](https://github.com/can1357/oh-my-pi) is a full-featured agent — LSP, debugger, browser control, subagents, 60+ model backends, and native MCP; hyperia wires in with no shim, and it runs Ollama models locally. [fx](https://github.com/vercel-labs/fx) is Vercel's native agent (Zig), authenticating via Codex/Grok subscriptions or an AI Gateway key. Both support container login and carry credentials across sessions.

Per-provider model defaults without cross-contamination: `OPENCODE_DEFAULT_MODEL=ollama/glm-5.2:cloud` in `[env]` and only opencode changes.

## The fleet sees everything

`n8 serve` runs the gateway (port **9801**): HTTP API, trigger scheduler, and an agent control plane reconciled against live containers every ~10s — hand-started containers included.

- **`/fleet`** — live dashboard: every agent's CPU/mem (real container stats), network, tokens/sec, tool calls as they happen, and a full-text-searchable event stream (BM25, streaming over SSE)
- **`POST /mcp`** — the same fleet as MCP tools (`fleet_status`, `agent_events`, `agent_net`, `telemetry_health`), stateless streamable-HTTP: point any MCP client at it and ask n8 what its agents are doing
- **`n8 agents`** — list / spawn / kill across the fleet from the CLI

| Route | What |
|---|---|
| `/completion` | run a prompt |
| `/sessions` · `/sessions/:id` | session list / detail |
| `/triggers` | scheduled prompts — once, daily, interval |
| `/agents` · `/agents/spawn` · `/agents/:id/kill` | control plane |
| `/expose` · `/unexpose` · `/exposed` | reverse port tunnels |
| `/fleet` · `/fleet/data.json` · `/mcp` | telemetry: dashboard / JSON / MCP |

**Reverse tunneling:** an agent starts a dev server inside its sandbox; the `expose_port` MCP tool maps it to `127.0.0.1:<port>` on your host (chisel data plane, ports 18000–18999). View and close tunnels in the TUI dashboard.

## The tool bench

Agents wake up with batteries included:

- **Built-in native servers** (always on, opt-out per workspace): `nuts-files` (transactional, Unicode-correct file ops), `shivvr` (embeddings), `ask` (second-opinion LLM), `n8gw` (gateway control)
- **Registry servers** (toggle per workspace): blender, hyperia, meridian, fleet telemetry, …
- **Python stdio tools** in `MCP/` — drop a script, it's a tool

Every agent also receives its **system prompt where its CLI actually reads it** (AGENTS.md / CLAUDE.md / GEMINI.md…), carrying shared guardrails: work in `/workspace` (host-backed — survives the container), bind `0.0.0.0`, use the file tools, and if you're missing something — *ask for a terminal instead of failing sadly*.

Terminal-integration bonus (Hyperia users): containers get **persistent identities that survive restarts** — no dead tokens stranding a running agent — and always know which pane is displaying them.

## Build the image your way

```bash
n8 build          # checkbox picker
n8 build --rust   # rustup/cargo/rustc baked in — agents compile out of the box
n8 build --native # C/C++ toolchain (node-gyp, Python C extensions, linkers)
n8 build --gpu    # CUDA runtime + cuDNN
n8 build --ffmpeg # media
```

Cargo caches persist on the shared data home — crates download once, ever.

## Configuration

`.nemesis8.toml` per workspace (local wins), `~/.nemesis8/config.toml` global:

```toml
provider = "codex"
mcp_tools = ["blender", "github.py"]
# disabled_builtins = ["ask"]

[env]
OPENCODE_DEFAULT_MODEL = "ollama/glm-5.2:cloud"
env_imports = ["SERPAPI_API_KEY"]        # forward from host env

[[mounts]]
host = "C:/data/models"
container = "/workspace/models"
```

Secrets travel by **reference and forwarding** — provider TOMLs mark which env vars matter; nothing gets baked into images.

## Remote mode

No Docker on the client at all:

```bash
export NEMESIS8_REMOTE=http://server:9801
n8 run "fix the tests"
n8 resume a4f2c
```

Auth via `--token` / `NEMESIS8_TOKEN`.

## CLI reference

```
n8                       home screen (new session / resume / control room)
n8 run <prompt>          one-shot
n8 resume [id|last]      last-10 overlay · direct by id · newest with no UI
n8 sessions [query]      list / full-text search past sessions (--json for agents)
n8 attach <name>         attach to a running agent
n8 agents <action>       fleet control plane: list / spawn / kill
n8 serve                 gateway (--background / --status / --stop)
n8 services <action>     dependency containers (start / stop / list)
n8 shell                 container bash
n8 login                 provider credentials
n8 build                 rebuild the agent image
n8 mcp test              validate every provider's generated MCP config
n8 doctor                check prerequisites
```

**Flags:** `--provider` `--model` `--danger` `--workspace` `--privileged` `--remote` `--token` `--port` `--tag`

## Project layout

| Dir | What |
|---|---|
| `providers/` | AI agents as declarative TOML specs (foreground TTY) |
| `apps/` | non-AI foreground tools (e.g. `glint` dashboard) |
| `services/` | background dependency containers |
| `MCP/` | Python stdio MCP tools |
| `mcp-servers/` | MCP server registry (TOML) |
| `mcp-bins/` | Rust source for the native MCP binaries |
| `prompts/` | the shared BASE guardrails every agent receives |

## Building & releasing

One script, two audiences — [`scripts/build.sh`](scripts/build.sh):

```bash
./scripts/build.sh                     # manual: gates, then the image picker
./scripts/build.sh --agent --rust      # agentic: non-interactive, JSON summary on stdout
./scripts/build.sh --agent --host-only # gates + host binary only, no image
```

Gates = release build, full test suite, `n8 mcp test`, and a gateway smoke (boot on a scratch port, `/health` + `/mcp` handshake). Agents get logs on stderr and a single JSON line on stdout.

Shipping has four channels (host binary, base image, container internals, installer site) — the runbook is [docs/RELEASING.md](docs/RELEASING.md).

**Working on nemesis8 itself — human or agent?** Start with **[docs/HANDOFF.md](docs/HANDOFF.md)**: current state, the port family, hard-won working rules, the open-issue map, and cross-repo contracts. Keep it updated when you change the state of the world — it's the first thing the next session reads.

## License

[BSD 3-Clause](LICENSE)

---

[Website](https://nemesis8.nuts.services) · [GitHub](https://github.com/DeepBlueDynamics/nemesis8) · [Deep Blue Dynamics](https://github.com/DeepBlueDynamics)
