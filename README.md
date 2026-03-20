# Nemesis 8

Run AI agents in Docker. Switch providers with a flag. Keep your sessions, tools, and sanity.

[nemesis8.nuts.services](https://nemesis8.nuts.services)

---

## What is this?

nemesis8 is a single Rust binary that wraps AI CLI tools (OpenAI Codex, Google Gemini) in Docker containers with persistent sessions, 69 MCP tools, and an HTTP gateway with a built-in scheduler. You point it at a project directory and it handles the rest — image building, tool installation, credential forwarding, session management.

## Install

```bash
# From source
cargo install --path .

# Or grab a release binary
# https://github.com/DeepBlueDynamics/nemesis8/releases
```

**Prerequisites:** Docker and at least one API key (`OPENAI_API_KEY` or `GEMINI_API_KEY`).

## Usage

```bash
# Run a prompt — builds the image automatically on first run
nemesis8 run "list all TODO comments and summarize"

# Interactive session (full TUI)
nemesis8 interactive

# Use Gemini instead of Codex
nemesis8 --provider gemini interactive

# Resume a previous session (last 5 chars of UUID)
nemesis8 sessions
nemesis8 resume a4f2c

# Drop into the container shell
nemesis8 shell

# Store API credentials in the container
nemesis8 login
```

The same 69 MCP tools work across both providers — file ops, web crawling, search, TTS, vision, and more. They're installed automatically at container startup.

## Configuration

Create a `.codex-container.toml` in your project root (or run `nemesis8 init` to scaffold one):

```toml
provider = "codex"               # or "gemini"
workspace_mount_mode = "named"
codex_cli_version = "latest"     # pin a version or use "latest" to auto-update

# Which MCP tools to activate (from the MCP/ directory)
mcp_tools = ["serpapi-search.py", "gnosis-crawl.py", "pdf-reader.py"]

[env]
MY_API_URL = "https://api.example.com"
env_imports = ["SERVICE_URL", "API_KEY"]   # forward these from the host

[[mounts]]
host = "C:/Users/you/data"
container = "/workspace/data"
```

## Providers

| Provider | CLI | How to authenticate |
|----------|-----|---------------------|
| **Codex** (default) | `@openai/codex` | Set `OPENAI_API_KEY` or run `nemesis8 login` |
| **Gemini** | `@google/gemini-cli` | Set `GEMINI_API_KEY` or run `nemesis8 --provider gemini login` |

Switch at any time with `--provider gemini` or set `provider = "gemini"` in your config.

## Gateway + Scheduler

`nemesis8 serve` starts an HTTP API with an integrated trigger scheduler:

```bash
nemesis8 serve              # listens on port 4000
nemesis8 serve --port 8080  # custom port
```

**Endpoints:**

| Route | Method | What it does |
|-------|--------|--------------|
| `/health` | GET | Liveness check |
| `/status` | GET | Active runs, scheduler status, uptime |
| `/completion` | POST | Run a prompt and get the result |
| `/sessions` | GET | List all sessions |
| `/sessions/:id` | GET | Session details |
| `/triggers` | GET/POST | List or create scheduled triggers |
| `/triggers/:id` | GET/PUT/DELETE | Manage a trigger |

**Triggers** run prompts on a schedule — once at a timestamp, daily at a time, or on an interval. The scheduler fires them through Docker automatically.

### MCP Integration

`nemesis-mcp.py` connects any Claude Code session to the gateway over HTTP. Add it to `.mcp.json` and you get full control: run prompts, manage triggers, list sessions — all from within Claude.

## Pokeball System

Capture a project, seal it into a hardened image, and run AI against it in an isolated container:

```bash
nemesis8 pokeball capture ./my-project    # detect deps, generate spec
nemesis8 pokeball seal ./my-project       # build sealed image
nemesis8 pokeball run myapp --prompt "fix the tests"
```

Workers are locked down: `network=none`, read-only root filesystem, all capabilities dropped, 4GB memory limit, 256 PID limit. AI model access goes through a broker.

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

**Flags:** `--provider`, `--danger`, `--model`, `--workspace`, `--port`, `--tag`, `--privileged`

## Building from source

```bash
cargo build --release
```

## License

[Gnosis AI-Sovereign License v1.3](LICENSE.md) | [BSD 3-Clause alternative](BSD-LICENSE)

---

[Website](https://nemesis8.nuts.services) | [GitHub](https://github.com/DeepBlueDynamics/nemesis8) | [Deep Blue Dynamics](https://github.com/DeepBlueDynamics)
