# MCP Build Guide (Gnosis)

Purpose: standardize how we build MCP servers/tools in this repo so tools are easy for agents to pick, call, and recover from.

This guide is based on a scan of current tools in `MCP/` (64 tool files, 398 `@mcp.tool()` functions).

## 1) Current inventory snapshot

Primary comms/email surfaces we currently have:
- `google-gmail.py` (`gmail_*` read/send/reply/search/labels)
- `agentmail.py` (`agentmail_*` inbox/message APIs)
- `agent-chat.py` relay (`relay_*`) + agent-to-agent helpers
- `slackbot.py` (`slack_*`)
- `twilio-sms.py` (`twilio_*`)
- `github_discussions.py` discussion-thread comms

Design implication:
- Prefer one tool family per channel (email/chat/sms) with consistent naming and response shapes.

## 2) Server skeleton (required)

Use this structure in every MCP server:

```python
#!/usr/bin/env python3
from __future__ import annotations

from typing import Any, Dict
from mcp.server.fastmcp import FastMCP

mcp = FastMCP("tool-name")

# helpers

def _result(success: bool, **kwargs: Any) -> Dict[str, Any]:
    payload = {"success": success}
    payload.update(kwargs)
    return payload

@mcp.tool()
async def tool_fn(arg: str) -> Dict[str, Any]:
    """Short action sentence.

    Use when:
    - ...

    Do not use when:
    - ...

    Args:
        arg: ...

    Returns:
        Dict with `success`, payload fields, and optional recovery fields.
    """
    ...

if __name__ == "__main__":
    mcp.run(transport="stdio")
```

Rules:
- Keep `if __name__ == "__main__"` at end of file.
- Always type annotate inputs/outputs.
- Keep helper functions private (`_name`).

## 3) Naming conventions

- Server name: kebab-case in `FastMCP("...")`.
- Tool functions: snake_case verbs (`gmail_send`, `file_read`, `service_engine_restart`).
- Keep families grouped by prefix:
  - `gmail_*`, `agentmail_*`, `relay_*`, `file_*`, `weather_*`, etc.
- Avoid Python keyword collisions by suffix underscore (`from_`).

## 4) Input contract best practices

- Keep first-call inputs simple primitives: `str`, `int`, `bool`, `Optional[str]`.
- Validate aggressively at entry point.
- Return actionable validation failures; do not throw raw tracebacks.
- Use bounded ranges for numerics (`limit`, `timeout`, `forecast_days`, etc.).
- For nested/complex input, support both:
  - structured `dict` parameter, and/or
  - `*_json: str` fallback for callers that only pass strings.

Validation pattern:

```python
if not inbox_id:
    return {"success": False, "error": "Missing inbox_id", "next_steps": ["Call agentmail_list_inboxes first"]}
if limit < 1 or limit > 200:
    return {"success": False, "error": "limit must be 1..200", "provided": limit}
```

## 5) Output contract (standard response shape)

Every tool should return JSON-like dict with predictable keys.

Minimum:
- `success: bool`

On success:
- `data` and/or domain fields (`messages`, `file_path`, `services`, etc.)
- optional `warnings: list[str]`
- optional `meta: {}` for extra diagnostics

On failure (positive, recovery-oriented):
- `success: false`
- `error: str` (short diagnosis)
- `detail: str` (optional context)
- `likely_causes: list[str]` (optional)
- `try_instead: list[str]` (optional alternates)
- `next_steps: list[str]` (concrete follow-up actions)

Preferred helper:

```python
def _ok(**kwargs):
    return {"success": True, **kwargs}

def _fail(error: str, **kwargs):
    return {"success": False, "error": error, **kwargs}
```

## 6) Docstring design for agent selection (important)

Docstrings are part of tool selection quality. Keep them precise and decision-oriented.

Use this template:

```python
"""Do one clear action in one sentence.

Use when:
- Condition A
- Condition B

Do not use when:
- Condition C (use `other_tool`)

Args:
    x: format, units, bounds, examples.

Returns:
    success shape and key fields.

Failure behavior:
- What errors mean.
- What caller should do next.

Examples:
    tool_name(x="...")
"""
```

Prompt-string optimization rules:
- Lead with decision boundary (`Use when` / `Do not use when`).
- Include canonical units and formats (`E.164`, `YYYY-MM-DD`, RFC3339).
- Include one realistic example call.
- Avoid vague wording like "helper", "stuff", "various".
- State side effects explicitly (`writes file`, `sends message`, `deletes path`).

## 7) Error messaging standard (positive + alternate paths)

For common errors, always include recovery hints.

Examples:

```python
return {
  "success": False,
  "error": "AGENTMAIL_API_KEY not set",
  "likely_causes": ["Missing .agentmail.env", "Env not loaded after restart"],
  "try_instead": ["set_agentmail_key(text=..., persist=True)", "agentmail_status()"],
  "next_steps": ["Create .agentmail.env", "Restart MCP session", "Re-run agentmail_status"]
}
```

```python
return {
  "success": False,
  "error": "Page content quality is minimal",
  "detail": "Likely SPA shell or blocked page",
  "try_instead": ["Retry with JS rendering", "Search cached crawl", "Use alternate source URL"]
}
```

## 8) Side-effect safety

Any destructive or external side-effect tool must state this in docstring and enforce guardrails:
- delete/move/overwrite
- outbound messaging/email/SMS
- service restart/stop
- remote API writes

Guardrail examples:
- require explicit boolean flags (`confirm`, `recursive`, `force`)
- include dry-run mode where feasible
- cap batch sizes and timeouts

## 9) Performance + reliability

- Prefer async I/O for network tools (`aiohttp`).
- Set explicit per-request timeout.
- Normalize/trim inputs before network calls.
- Return status codes/URLs when helpful for debugging.
- Avoid huge payloads in response body unless requested.
- For large binaries/text, return file path + metadata rather than inline body.

## 10) Logging and observability

- Log to `.mcp-logs/<tool>.log` for operational servers.
- Never log secrets.
- Include request identifiers when possible.
- Add `status`, `url`, `duration_ms` in responses for remote calls.

## 11) Security checklist

- Credentials only from env or explicit setup tool.
- Never return secrets (only masked `last4`).
- Validate URLs/paths and constrain write locations where appropriate.
- Treat user-provided text as untrusted.
- Keep dependency surface minimal.

## 12) Testing expectations

Minimum per new tool file:
- parse/import test
- one success-path test
- one validation-error test
- one dependency/auth-missing test

Smoke commands:

```bash
python3 -m py_compile MCP/<tool>.py
python3 - <<'PY'
import importlib.util
spec=importlib.util.spec_from_file_location('x','MCP/<tool>.py')
mod=importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)
print('ok')
PY
```

## 13) Build checklist before merge

- [ ] Function names follow family prefix.
- [ ] Docstrings include `Use when`, `Do not use when`, examples.
- [ ] Inputs are validated with bounded ranges/formats.
- [ ] Outputs follow standard response contract.
- [ ] Errors include `try_instead`/`next_steps` for common failures.
- [ ] Secrets never returned/logged.
- [ ] `mcp.run(transport="stdio")` at file end.
- [ ] Compile/import smoke checks pass.

## 14) Recommended default response schema (copy/paste)

```json
{
  "success": true,
  "data": {},
  "warnings": [],
  "meta": {
    "source": "tool-name",
    "version": "1"
  }
}
```

```json
{
  "success": false,
  "error": "short diagnosis",
  "detail": "optional context",
  "likely_causes": ["cause1"],
  "try_instead": ["other_tool(...)"],
  "next_steps": ["step 1", "step 2"]
}
```

## 15) Quick examples from this repo to emulate

- Rich argument/return docstrings: `gnosis-files-basic.py`
- Recovery-focused orchestration errors: `gnosis-orchestrator.py`
- Key setup + status patterns: `agentmail.py`, `google-gmail.py`, `serpapi-search.py`
- Compact service health + action tools: `service-engine.py`, `twilio-sms.py`

## 16) Agent-Chat Provider Matrix

`MCP/agent-chat.py` now supports multi-provider routing and model discovery.

Tools:
- `list_models(provider="all")`
- `check_with_agent(..., provider=..., model=...)`
- `chat_with_context(..., provider=..., model=...)`
- `agent_to_agent(..., provider=..., model=...)`
- `run_code_request(request=..., target=...)`

Provider/env matrix:
- `anthropic` -> `ANTHROPIC_API_KEY`
- `openai` -> `OPENAI_API_KEY`
- `gemini` -> `GOOGLE_API_KEY`
- `ollama` -> `OLLAMA_BASE_URL` (default `http://localhost:11434`)

Context-window handling (current):
- Input/output budget split with safety margin.
- Prompt clipping is deterministic and preserves system instructions first.
- Use `run_code_request` for code tasks to keep prompt structure stable.
