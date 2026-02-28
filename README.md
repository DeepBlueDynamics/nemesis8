# Nemisis 8

Rust orchestrator for Codex container workloads. Distributer of fortune.

What you do with it is up to you.

## Quick Start

```bash
# Build the Docker image
nemisis8 build

# Run a one-shot prompt
nemisis8 run "list markdown files and summarize"

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

## CLI Reference

```
nemisis8 build              Build Docker image
nemisis8 run <prompt>       One-shot exec
nemisis8 interactive        Interactive Codex session
nemisis8 serve              HTTP gateway (default port 4000)
nemisis8 shell              Drop into container bash
nemisis8 login              Refresh Codex auth
nemisis8 sessions           List sessions
nemisis8 resume <id>        Resume session (full UUID or last 5 chars)
```

### Global Flags

```
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
workspace_mount_mode = "named"
mcp_tools = ["agent-chat.py", "gnosis-crawl.py", "gnosis-files-basic.py"]

[env]
SOME_SERVICE_URL = "https://api.example.com"

env_imports = ["SERVICE_ENGINE_URL", "MOLTBOOK_API_KEY"]

[[mounts]]
host = "C:/Users/kord/Code/gnosis/myoo"
container = "/workspace/myoo"
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

## MCP Tools

69 Python MCP tools in `MCP/`, installed automatically at container startup. See `.codex-container.toml` for the active tool list.

## Architecture

Two Rust binaries:
- **`nemisis8`** — host CLI (build, run, serve, sessions)
- **`nemisis8-entry`** — runs inside Docker (MCP install, config gen, exec)

```
src/
├── main.rs          CLI entry point
├── lib.rs           Library root
├── cli.rs           clap subcommands + global flags
├── config.rs        .codex-container.toml parser
├── docker.rs        bollard Docker lifecycle
├── gateway.rs       axum HTTP gateway
├── session.rs       session list/resume
├── scheduler.rs     trigger scheduling
└── entry.rs         container entry-point binary
```

## Building

```bash
# From gnosis workspace root
cargo build -p nemisis8

# Release build
cargo build -p nemisis8 --release
```

## License

[Gnosis AI-Sovereign License v1.3](LICENSE.md) | [BSD 3-Clause alternative](BSD-LICENSE)
