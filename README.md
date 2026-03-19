# Nemesis 8

Rust orchestrator for AI CLI container workloads. Multi-provider. Distributer of fortune.

What you do with it is up to you.

**Website:** [nemesis8.nuts.services](https://nemesis8.nuts.services)

## Quick Start

```bash
# Build the Docker image
nemesis8 build

# Run a one-shot prompt (Codex, default)
nemesis8 run "list markdown files and summarize"

# Run with Gemini instead
nemesis8 --provider gemini run "hello"

# Interactive session
nemesis8 interactive

# Start the HTTP gateway + scheduler
nemesis8 serve --port 4000

# Drop into a container shell
nemesis8 shell

# List and resume sessions
nemesis8 sessions
nemesis8 resume <session-id-or-last-5>
```

## Providers

nemesis8 supports multiple AI CLI backends:

| Provider | CLI | Auth |
|----------|-----|------|
| **Codex** (default) | `@openai/codex` | `OPENAI_API_KEY` or `nemesis8 login` |
| **Gemini** | `@google/gemini-cli` | `GEMINI_API_KEY` or `nemesis8 --provider gemini login` |

Set provider in config (`provider = "gemini"`) or via CLI flag (`--provider gemini`).

### Known Issues

- **Gemini interactive mode** has TTY rendering issues in some terminal environments. One-shot `run` works. See [#1](https://github.com/DeepBlueDynamics/nemesis8/issues/1).

## CLI Reference

```
nemesis8 build              Build Docker image
nemesis8 run <prompt>       One-shot exec
nemesis8 interactive        Interactive session
nemesis8 serve              HTTP gateway + scheduler (default port 4000)
nemesis8 shell              Drop into container bash
nemesis8 login              Refresh auth credentials
nemesis8 sessions           List sessions
nemesis8 resume <id>        Resume session (full UUID or last 5 chars)
nemesis8 init               Scaffold .codex-container.toml
nemesis8 doctor             Check prerequisites
nemesis8 pokeball <action>  Sealed project environments
```

### Global Flags

```
--provider <name>     AI provider: codex (default) or gemini
--danger              Bypass sandbox (danger mode)
--privileged          Docker privileged mode
--model <name>        Model override
--workspace <path>    Custom workspace mount
--port <N>            Gateway port (default 4000)
--tag <image>         Custom image tag
```

## Configuration

Config lives in `.codex-container.toml` at the workspace root:

```toml
provider = "codex"  # or "gemini"
workspace_mount_mode = "named"
mcp_tools = ["agent-chat.py", "gnosis-crawl.py", "gnosis-files-basic.py"]

[env]
SOME_SERVICE_URL = "https://api.example.com"

env_imports = ["SERVICE_ENGINE_URL", "MOLTBOOK_API_KEY"]

[[mounts]]
host = "C:/Users/you/data"
container = "/workspace/data"
```

## HTTP Gateway + Scheduler

`nemesis8 serve` starts an axum HTTP gateway with an integrated scheduler:

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Liveness check |
| `/status` | GET | Concurrency + scheduler info |
| `/completion` | POST | Run a prompt |
| `/sessions` | GET | List sessions |
| `/sessions/:id` | GET | Session details |
| `/sessions/:id/prompt` | POST | Continue session |
| `/triggers` | GET | List scheduled triggers |
| `/triggers` | POST | Create a trigger |
| `/triggers/:id` | GET | Get trigger details |
| `/triggers/:id` | PUT | Update a trigger |
| `/triggers/:id` | DELETE | Delete a trigger |

Concurrency is limited to 2 simultaneous runs with an 8-second spawn throttle.

### Trigger Schedules

Triggers support three schedule types:

- **Once** — fire at a specific timestamp
- **Daily** — fire at HH:MM in a given timezone
- **Interval** — fire every N minutes

### MCP Integration

The `nemesis-mcp.py` tool provides a Model Context Protocol interface to the gateway, giving any Claude Code session full control over nemesis8: status, prompt execution, trigger CRUD, session management, and time utilities.

## Pokeball System

Capture, seal, and run projects in isolated containers:

```bash
nemesis8 pokeball capture ./my-project   # scan and generate spec
nemesis8 pokeball seal ./my-project      # capture + build image
nemesis8 pokeball run myapp --prompt "fix the tests"
nemesis8 pokeball list                   # list registered pokeballs
```

Workers run with `network=none`, read-only rootfs, all caps dropped, 4GB memory limit, 256 PID limit.

## MCP Tools

69 Python MCP tools in `MCP/`, installed automatically at container startup. See `.codex-container.toml` for the active tool list.

## Architecture

Two Rust binaries:
- **`nemesis8`** -- host CLI (build, run, serve, sessions, pokeball)
- **`nemesis8-entry`** -- runs inside Docker (MCP install, provider config gen, CLI launch)

```
src/
├── main.rs          CLI entry point
├── lib.rs           Library root
├── cli.rs           clap subcommands + global flags
├── config.rs        .codex-container.toml parser + provider abstraction
├── docker.rs        bollard Docker lifecycle + docker CLI for TTY
├── gateway.rs       axum HTTP gateway + scheduler
├── session.rs       session list/resume
├── scheduler.rs     trigger scheduling
├── entry.rs         container entry-point binary
└── pokeball/        sealed environment system
```

## Building

```bash
cargo build --release
```

## License

[Gnosis AI-Sovereign License v1.3](LICENSE.md) | [BSD 3-Clause alternative](BSD-LICENSE)
