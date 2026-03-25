# Nemesis 8

Run AI agents in Docker. Four providers. One binary. Switch with a flag.

[nemesis8.nuts.services](https://nemesis8.nuts.services)

---

## What is this?

nemesis8 wraps AI CLI tools in Docker containers with persistent sessions, 69 MCP tools, and an HTTP gateway with a built-in scheduler. Four providers — Codex, Gemini, Claude Code, and OpenClaw — all sharing the same tools and config. Point it at a project directory and it handles image building, tool installation, credential forwarding, and session management.

Works locally with Docker, or remotely against a gateway — no Docker needed on the client.

## Install

```bash
# From source
cargo install --path .

# Or grab a release binary
# https://github.com/DeepBlueDynamics/nemesis8/releases
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
nemesis8 --provider openclaw interactive

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

The same 69 MCP tools work across all four providers — file ops, web crawling, search, TTS, vision, and more.

## Configuration

Create a `.codex-container.toml` in your project root (or run `nemesis8 init`):

```toml
provider = "codex"               # codex, gemini, claude, or openclaw
workspace_mount_mode = "named"
codex_cli_version = "latest"     # pin a version or use "latest"

# MCP tools to activate
mcp_tools = ["serpapi-search.py", "gnosis-crawl.py", "pdf-reader.py"]

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
| **OpenClaw** | `openclaw` | `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` |

All providers auto-update to the latest CLI version at container startup.

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

## Gateway + Scheduler

`nemesis8 serve` runs an HTTP gateway with an integrated trigger scheduler:

```bash
nemesis8 serve              # port 4000
nemesis8 serve --port 8080
```

| Route | Method | What it does |
|-------|--------|--------------|
| `/health` | GET | Liveness check |
| `/status` | GET | Active runs, scheduler status, uptime |
| `/completion` | POST | Run a prompt |
| `/sessions` | GET | List sessions |
| `/sessions/:id` | GET | Session details |
| `/triggers` | GET/POST | List or create scheduled triggers |
| `/triggers/:id` | GET/PUT/DELETE | Manage a trigger |

Triggers run prompts on a schedule — once, daily, or on an interval. Auth middleware available via `NEMESIS8_AUTH_TOKEN`.

### MCP Integration

`nemesis-mcp.py` connects any Claude Code session to the gateway. Add it to `.mcp.json` for full control: prompts, triggers, sessions — all from within Claude.

## Pokeball System

Capture a project, seal it into a hardened image, run AI against it in isolation:

```bash
nemesis8 pokeball capture ./my-project    # detect deps, generate spec
nemesis8 pokeball seal ./my-project       # build sealed image
nemesis8 pokeball run myapp --prompt "fix the tests"
nemesis8 pokeball list                    # list stored pokeballs
```

Workers: `network=none`, read-only rootfs, all caps dropped, 4GB RAM, 256 PIDs. AI talks through a broker — the container never touches the network.

## CLI Reference

```
nemesis8 run <prompt>       Run a prompt (one-shot)
nemesis8 interactive        Full TUI session
nemesis8 serve              HTTP gateway + scheduler
nemesis8 shell              Container bash shell
nemesis8 login              Store API credentials
nemesis8 sessions           List past sessions
nemesis8 resume <id>        Resume a session
nemesis8 build              Rebuild the Docker image
nemesis8 init               Create a config file
nemesis8 doctor             Check prerequisites
nemesis8 pokeball <action>  Sealed environments
```

**Flags:** `--provider`, `--danger`, `--model`, `--workspace`, `--port`, `--tag`, `--privileged`, `--remote`, `--token`

## Building from source

```bash
cargo build --release
```

## License

[Gnosis AI-Sovereign License v1.3](LICENSE.md) | [BSD 3-Clause alternative](BSD-LICENSE)

---

[Website](https://nemesis8.nuts.services) | [GitHub](https://github.com/DeepBlueDynamics/nemesis8) | [Deep Blue Dynamics](https://github.com/DeepBlueDynamics)
