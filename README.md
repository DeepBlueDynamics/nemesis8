# Nemesis 8

Run AI agents in Docker. One binary. Multiple providers. Switch with a flag.

[nemesis8.nuts.services](https://nemesis8.nuts.services)

---

## What is this?

nemesis8 wraps AI coding CLIs (Codex, Gemini, Claude Code, Antigravity, others) in Docker containers with persistent sessions, a curated bench of MCP tools, and an HTTP gateway with a built-in scheduler. Point it at a project directory and it handles image building, tool installation, credential forwarding, and session management.

Works locally with Docker, or remotely against a gateway — no Docker needed on the client.

## Install

**Windows:**
```powershell
powershell -c "irm https://nemesis8.nuts.services/install.ps1 | iex"
```

**Linux / macOS:**
```bash
curl -fsSL https://nemesis8.nuts.services/install.sh | sh
```

**From source:**
```bash
cargo install --path .
```

**Prerequisites:** Docker (or a remote gateway). API keys optional — set them if your provider needs them.

## Usage

```bash
# Run a prompt — builds the image automatically on first run
nemesis8 run "list all TODO comments and summarize"

# Interactive session (full TUI)
nemesis8 interactive

# Switch providers
nemesis8 --provider gemini interactive
nemesis8 --provider claude interactive

# Danger mode — skip all approvals and sandboxing
nemesis8 --danger interactive

# Resume a previous session
nemesis8 sessions
nemesis8 resume a4f2c

# Remote mode — no local Docker needed
nemesis8 --remote http://server:4000 run "analyze this codebase"

# Drop into the container shell
nemesis8 shell
```

The same tool bench works across every provider — file ops, web crawling, search, TTS, vision, and more.

## Configuration

Create a `.nemesis8.toml` in your project root (or run `nemesis8 init`):

Config is two layers: global defaults in `~/.nemesis8/config.toml` ⊕ this
per-workspace `.nemesis8.toml` (local keys win). Your project's `.nemesis8.toml`
is bind-mounted at `/workspace/<name>`; `/workspace` itself is a per-session
scratch parent (so an agent's `cd ..` stays sandboxed). Built-in binary servers
(nuts-files, shivvr, ask, nemesis8) are always on — disable per-workspace with
`disabled_builtins`.

```toml
provider = "codex"               # codex, gemini, or claude
codex_cli_version = "latest"     # pin a version or use "latest"

# MCP tools to activate (Python tools + registry servers like "blender")
mcp_tools = ["serpapi-search.py", "pdf-reader.py", "blender"]

# Turn an always-on built-in off for this workspace:
# disabled_builtins = ["ask"]

# Commands to run inside the container before the CLI launches
setup_commands = [
    "cd /workspace && npm install",
    "pip install -e ."
]

# Remote gateway (skip local Docker entirely)
# remote = "http://server:4000"

[env]
MY_API_URL = "https://api.example.com"
env_imports = ["SERVICE_URL", "SERPAPI_API_KEY"]

[[mounts]]
host = "C:/Users/you/data"
container = "/workspace/data"
```

### Environment Files

nemesis8 loads env files on startup (later wins):

1. `~/.nemesis8/env` — global keys shared across projects
2. `.env` — project-level
3. `.*.env` — named files like `.serpapi.env`, `.openai.env`

```
# .serpapi.env
SERPAPI_API_KEY=abc123
```

## Providers

| Provider | CLI | Auth |
|----------|-----|------|
| **Codex** (default) | `@openai/codex` | `OPENAI_API_KEY` or `nemesis8 login` |
| **Gemini** | `@google/gemini-cli` | `GEMINI_API_KEY` or `nemesis8 --provider gemini login` |
| **Claude** | `@anthropic-ai/claude-code` | `ANTHROPIC_API_KEY` or `nemesis8 --provider claude login` |
| **Antigravity** | `agy` (curl installer) | OAuth via `nemesis8 --provider antigravity login` |
| **Grok** | `grok` (x.ai installer) | `XAI_API_KEY` or `GROK_API_KEY` |
| **Ollama** | `codex` against a local Ollama endpoint | none (local models) |
| **Pi** | `@earendil-works/pi-coding-agent` | any backend key (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …) or `/login` |
| **Sakana** (alias `fugu`) | `codex` against [Sakana Fugu](https://console.sakana.ai) (`api.sakana.ai`) | `SAKANA_API_KEY` |

All providers auto-update to the latest CLI version at container startup. Each ships as a single TOML spec in [`providers/`](providers/) — no per-provider Rust — and you invoke it by name with `--provider <name>`.

**Custom OpenAI-compatible endpoints (Ollama, Sakana):** a provider can drive the `codex` CLI against any OpenAI-compatible API instead of OpenAI's. **Sakana Fugu** is one such provider — `--provider sakana` (or `fugu`) runs codex against Sakana's Fugu API (models `fugu` and `fugu-ultra`, 1M-token context). It's fully isolated from your `codex` setup: its own config dir (`.codex-sakana` via `CODEX_HOME`) and session history, so it never touches `~/.codex`. Set `SAKANA_API_KEY` in your env; pick the model with `--model fugu-ultra` for the deep-reasoning variant.

**Adding your own provider:** drop a TOML file in `providers/` and add the name to `INSTALL_PROVIDERS`. See **[docs/adding-a-provider.md](docs/adding-a-provider.md)** for the full schema and a worked example.

## Remote Mode

Run prompts against a remote gateway without Docker installed locally:

```bash
# Set once
export NEMESIS8_REMOTE=http://server:4000

# Then use normally
nemesis8 run "fix the tests"
nemesis8 sessions
nemesis8 resume a4f2c
nemesis8 doctor    # shows remote health
```

Or set `remote = "http://server:4000"` in your config. Auth token supported via `--token` or `NEMESIS8_TOKEN`.

## Gateway + control plane

`nemesis8 serve` runs an HTTP gateway with an integrated trigger scheduler **and an
agent control plane** — a registry of running agents reconciled against live
containers, so you can list, spawn, and kill agents across the fleet.

```bash
nemesis8 serve              # foreground, port 4000
nemesis8 serve --port 8080
nemesis8 serve --background # detached daemon (writes a PID + log)
nemesis8 serve --status     # is the daemon up?
nemesis8 serve --stop       # stop the daemon
```

Start/stop/status are also in the TUI: the control room's **Gateway** menu, with a
live status badge in the top bar.

| Route | Method | What it does |
|-------|--------|--------------|
| `/health` `/status` | GET | Liveness; active runs, scheduler, uptime |
| `/completion` | POST | Run a prompt |
| `/sessions` · `/sessions/:id` | GET | List sessions / details |
| `/triggers` · `/triggers/:id` | GET/POST/PUT/DELETE | Scheduled triggers |
| `/agents` · `/agents/:id` | GET | List agents / detail (control plane) |
| `/agents/spawn` · `/agents/:id/kill` | POST | Spawn / kill an agent |
| `/agents/:id/register` · `/deregister` | POST | Agents self-register on boot/exit |
| `/daemons` · `/daemons/register` | GET/POST | Worker daemons in a multi-host fleet |
| `/expose` | POST | Expose a container-local TCP port back to the host |
| `/unexpose` | POST | Release a previously exposed port mapping |
| `/exposed` | GET | List all active port mappings |

Agents are discovered by Docker **label** and reconciled every ~10s, so even
hand-started containers appear. Drive it from the CLI with **`nemesis8 agents`**
(list / spawn / kill). Triggers run prompts on a schedule — once, daily, or on an
interval. Auth via `NEMESIS8_AUTH_TOKEN`.

### Reverse Port Exposure (Tunneling)

For local development where you need to access a container-local port (such as a web application or database started by an agent inside the sandbox) from your host machine, nemesis8 provides automated reverse port tunneling using a `chisel` data plane.

* **Tools (`n8gw` MCP):**
  * `expose_port` — maps a container port to an ephemeral host port (bind-tested in the `18000-18999` range) reachable at `127.0.0.1:<host_port>`.
  * `unexpose_port` — stops and releases the specified tunnel by mapping ID.
* **TUI Integration:** View active port exposures, their allocated host ports, and control/close tunnels directly inside the control room Dashboard.

### MCP Integration

`nemesis-mcp.py` connects any Claude Code session to the gateway. Add it to `.mcp.json` for full control: prompts, triggers, sessions — all from within Claude.

## CLI Reference

```
nemesis8 run <prompt>       Run a prompt (one-shot)
nemesis8 interactive        Full TUI session (control room)
nemesis8 serve              HTTP gateway + scheduler + control plane
                            (--background / --status / --stop for daemon mode)
nemesis8 agents <action>    List / spawn / kill agents (control plane)
nemesis8 services <action>  Start / stop / list dependency services
nemesis8 attach <name>      Attach to a running agent
nemesis8 shell              Container bash shell
nemesis8 login              Store API credentials
nemesis8 sessions           List past sessions
nemesis8 resume <id>        Resume a session
nemesis8 build              Rebuild the Docker image (--glint installs the glint app)
nemesis8 init               Create a config file
nemesis8 doctor             Check prerequisites
```

**Flags:** `--provider`, `--danger`, `--model`, `--workspace`, `--port`, `--tag`, `--privileged`, `--remote`, `--token`

## Project layout

nemesis8 has two kinds of thing: **capabilities** an agent uses, and **things
nemesis8 launches**.

**Things nemesis8 launches** (on the launch axis — *foreground* vs *background*, *AI* vs not):

| Dir | What | Runs |
|---|---|---|
| `providers/` | AI coding agents (Codex, Claude, …) — TOML specs | foreground TTY |
| `apps/` | foreground **non-AI** tools (e.g. `glint` dashboard) — TOML specs | foreground TTY |
| `services/` | background dependency containers (ferricula, transcription, chisel) — TOML specs | background, no TTY |

In the home screen's **New** modal, the **Type** field switches between *Agent*
(providers) and *App* (`apps/`). Apps install opt-in at build time, e.g.
`n8 build --glint`.

**Capabilities an agent uses** (MCP):

| Dir | What |
|---|---|
| `MCP/` | Python stdio MCP **tools** (calculate, github, weather, …) |
| `mcp-servers/` | MCP **server** registry — TOML configs (native binary / remote HTTP / `uvx`) |
| `mcp-bins/` | Rust **source** for the native MCP-server binaries `mcp-servers/` points at (ask, n8gw, nuts-files, shivvr) — see [`mcp-bins/README.md`](mcp-bins/README.md) |

## Building from source

```bash
cargo build --release
```

## Releasing / deploying

See **[docs/RELEASING.md](docs/RELEASING.md)** — the runbook for all four ship
channels (host binary → GitHub Releases, base image → Docker Hub, container
internals → `n8 build`, installer/site → Cloud Run) and which one each kind of
change needs.

## License

[BSD 3-Clause](LICENSE)

---

[Website](https://nemesis8.nuts.services) | [GitHub](https://github.com/DeepBlueDynamics/nemesis8) | [Deep Blue Dynamics](https://github.com/DeepBlueDynamics)
