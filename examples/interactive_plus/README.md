# codex-keyboard

Python wrapper for programmatic control of [OpenAI Codex CLI](https://github.com/openai/codex).

## How It Works

This package uses the official `codex exec --json` interface for programmatic control. This is the same approach used by:
- The official [TypeScript SDK](https://github.com/openai/codex/tree/main/sdk/typescript) (`@openai/codex-sdk`)
- [codex-container](https://github.com/DeepBlueDynamics/codex-container)'s HTTP gateway

When you run `codex exec --json "your prompt"`, Codex outputs **JSONL (JSON Lines)** events to stdout:
- `thread.started` - New thread initialized with `thread_id`
- `turn.started`, `turn.completed`, `turn.failed` - Turn lifecycle events
- `item.*` - Agent messages, reasoning, command executions, file changes, MCP tool calls, web searches, plan updates
- `error` - Non-fatal errors

This package wraps this interface with a clean Python API.

## Installation

```bash
pip install codex-keyboard
```

**Prerequisites:**
- OpenAI Codex CLI installed: `npm install -g @openai/codex`
- Codex authenticated: `codex login`

## Quick Start

### Simple Prompt Execution

```python
from codex_keyboard import run_prompt

# Run a single prompt
result = run_prompt("List all Python files in the current directory")
print(result["output"])
print(f"Success: {result['success']}")
```

### Streaming Events

```python
from codex_keyboard import run_prompt_streaming

# Stream events as they arrive
for event in run_prompt_streaming("Explain this codebase"):
    if event.type == "item.agent_message":
        print(event.data.get("text", ""))
    elif event.type == "item.command_execution":
        print(f"Running: {event.data}")
```

### Multi-Turn Conversations

```python
from codex_keyboard import CodexThread

# Maintain conversation context across turns
thread = CodexThread()

# First turn
result = thread.run("Create a hello world Python script")
print(result["output"])

# Continue the conversation (automatically resumes session)
result = thread.run("Now add command-line argument parsing")
print(result["output"])

# Access session info
print(f"Session ID: {thread.session_id}")
```

### Full Control with CodexExecController

```python
from codex_keyboard import CodexConfig, CodexExecController

# Configure Codex
config = CodexConfig(
    model="gpt-4",
    approval_mode="full-auto",  # No approval prompts
    sandbox_mode="workspace-write",
    working_directory="/path/to/project",
)

controller = CodexExecController(config)

# Run with event callback
def on_event(event):
    print(f"[{event.type}] {event.data}")

result = controller.run(
    "Refactor the main.py file",
    on_event=on_event
)

# Resume the session later
result = controller.resume(
    "Also update the tests",
    session_id=result["session_id"]
)
```

### Async Support

```python
import asyncio
from codex_keyboard import run_prompt_async, stream_prompt_async

async def main():
    # Async execution
    result = await run_prompt_async("Generate a README")
    
    # Async streaming
    async for event in stream_prompt_async("Analyze the code"):
        print(event.type)

asyncio.run(main())
```

## Event Types

Events received from `--json` mode:

| Event Type | Description |
|------------|-------------|
| `thread.started` | Thread initialized, contains `thread_id` |
| `turn.started` | Turn began processing |
| `turn.completed` | Turn finished successfully |
| `turn.failed` | Turn failed |
| `item.agent_message` | Final response text |
| `item.reasoning` | Model's reasoning process |
| `item.command_execution` | Shell command executed |
| `item.file_change` | File was modified |
| `item.mcp_tool_call` | MCP tool invoked |
| `item.web_search` | Web search performed |
| `item.todo_list` | Plan/todo updates |
| `error` | Non-fatal error |

## Configuration Options

```python
from codex_keyboard import CodexConfig

config = CodexConfig(
    # Path to codex binary (auto-detected if not set)
    codex_path=None,
    
    # Working directory for Codex
    working_directory="/path/to/project",
    
    # Model to use
    model="gpt-5-codex",
    
    # Approval mode: "suggest", "auto-edit", "full-auto"
    approval_mode="suggest",
    
    # Sandbox: "read-only", "workspace-write", "danger-full-access"
    sandbox_mode="workspace-write",
    
    # API key (uses CODEX_API_KEY env var if not set)
    api_key=None,
    
    # Extra CLI arguments
    extra_args=["--skip-git-repo-check"],
)
```

## PTY Mode (Advanced)

For cases where you need direct TUI control:

```python
from codex_keyboard import interactive_session
import time

with interactive_session() as codex:
    # Send a prompt
    codex.send_prompt("Create a test file")
    
    # Wait for processing
    time.sleep(5)
    
    # Approve an action
    codex.approve_action()
    
    # Send custom keys
    codex.send_key("ctrl+c")  # Cancel
```

**Note:** PTY mode requires `pexpect` (Unix) or `wexpect` (Windows). The `exec` mode is recommended for most use cases.

## Similar Projects

- **[codex-container](https://github.com/DeepBlueDynamics/codex-container)** - Full Docker-based solution with HTTP API, file watchers, scheduling, and 272+ MCP tools
- **[@openai/codex-sdk](https://github.com/openai/codex/tree/main/sdk/typescript)** - Official TypeScript SDK
- **[codex_sdk (Elixir)](https://github.com/nshkrdotcom/codex_sdk)** - Elixir SDK

## License

MIT License
