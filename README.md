# Nemisis 8

Rust orchestrator for AI CLI container workloads. Multi-provider. Distributer of fortune.

What you do with it is up to you.

**Website:** [nemesis8.nuts.services](https://nemesis8.nuts.services)

## Quick Start

```bash
# Build the Docker image
nemisis8 build

# Run a one-shot prompt (Codex, default)
nemisis8 run "list markdown files and summarize"

# Run with Gemini instead
nemisis8 --provider gemini run "hello"

# Interactive session
nemisis8 interactive

# Start the HTTP gateway
nemisis8 serve --port 4000

# Drop into a container shell
nemisis8 shell

# List and resume sessions
nemisis8 sessions
nemisis8 resume <session-id-or-last-5>
```

## Providers

nemisis8 supports multiple AI CLI backends:

| Provider | CLI | Auth |
|----------|-----|------|
| **Codex** (default) | `@openai/codex` | `OPENAI_API_KEY` or `nemisis8 login` |
| **Gemini** | `@google/gemini-cli` | `GEMINI_API_KEY` or `nemisis8 --provider gemini login` |

Set provider in config (`provider = "gemini"`) or via CLI flag (`--provider gemini`).

### Known Issues

- **Gemini interactive mode** has TTY rendering issues in some terminal environments. One-shot `run` works. See [#1](https://github.com/DeepBlueDynamics/nemisis8/issues/1).

## CLI Reference

```
nemisis8 build              Build Docker image
nemisis8 run <prompt>       One-shot exec
nemisis8 interactive        Interactive session
nemisis8 serve              HTTP gateway (default port 4000)
nemisis8 shell              Drop into container bash
nemisis8 login              Refresh auth credentials
nemisis8 sessions           List sessions
nemisis8 resume <id>        Resume session (full UUID or last 5 chars)
nemisis8 init               Scaffold .codex-container.toml
nemisis8 doctor             Check prerequisites
nemisis8 pokeball <action>  Sealed project environments
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

## HTTP Gateway

`nemisis8 serve` starts an axum HTTP gateway:

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Liveness check |
| `/status` | GET | Concurrency info |
| `/completion` | POST | Run a prompt |
| `/sessions` | GET | List sessions |
| `/sessions/:id` | GET | Session details |
| `/sessions/:id/prompt` | POST | Continue session |

Concurrency is limited to 2 simultaneous runs with an 8-second spawn throttle.

## Pokeball System

Capture, seal, and run projects in isolated containers:

```bash
nemisis8 pokeball capture ./my-project   # scan and generate spec
nemisis8 pokeball seal ./my-project      # capture + build image
nemisis8 pokeball run myapp --prompt "fix the tests"
nemisis8 pokeball list                   # list registered pokeballs
```

Workers run with `network=none`, read-only rootfs, all caps dropped, 4GB memory limit, 256 PID limit.

## MCP Tools

69 Python MCP tools in `MCP/`, installed automatically at container startup. See `.codex-container.toml` for the active tool list.

## Architecture

Two Rust binaries:
- **`nemisis8`** -- host CLI (build, run, serve, sessions, pokeball)
- **`nemisis8-entry`** -- runs inside Docker (MCP install, provider config gen, CLI launch)

```
src/
├── main.rs          CLI entry point
├── lib.rs           Library root
├── cli.rs           clap subcommands + global flags
├── config.rs        .codex-container.toml parser + provider abstraction
├── docker.rs        bollard Docker lifecycle + docker CLI for TTY
├── gateway.rs       axum HTTP gateway
├── session.rs       session list/resume
├── scheduler.rs     trigger scheduling
├── entry.rs         container entry-point binary
└── pokeball/        sealed environment system
```

## Building

```bash
cargo build -p nemisis8 --release
```

## License

[Gnosis AI-Sovereign License v1.3](LICENSE.md) | [BSD 3-Clause alternative](BSD-LICENSE)
