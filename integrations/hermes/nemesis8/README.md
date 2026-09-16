# Nemesis8 Native Plugin for Hermes Agent

This plugin integrates Hermes Agent with the Nemesis8 control plane gateway (`n8 serve`), enabling Hermes agents to inspect the fleet, spawn containerized sub-agents, and manage active workloads.

## Directory Structure

```
/workspace/nemesis8/integrations/hermes/nemesis8/
├── plugin.yaml                    # Hermes plugin manifest
├── __init__.py                    # Plugin entry point: register(ctx)
├── schemas.py                     # Tool JSON schema definitions
├── tools.py                       # REST handlers communicating with n8gw
├── skills/
│   └── nemesis8-fleet/
│       └── SKILL.md               # Fleet orchestration & model selection skill
├── tests/
│   ├── __init__.py
│   └── test_plugin.py             # Comprehensive test suite (fake ctx, mocks)
└── README.md
```

## Tools Provided

All tools are namespaced to avoid collision with Hermes built-in tools:

- `nemesis8_gateway_status`: Check gateway health, active runs, agent count, uptime. Returns friendly offline notice if gateway is down.
- `nemesis8_agent_list`: List all agents known to Nemesis8 (running, idle, exited).
- `nemesis8_agent_spawn`: Spawn a new containerized agent given a `prompt` (required non-empty string) and optional `provider`, `model`, and `workspace` (validated as non-empty strings).
- `nemesis8_agent_stop`: Terminate a running agent container by `id` (required non-empty string, URL-encoded).
- `nemesis8_agent_kill`: Alias for `nemesis8_agent_stop`.

## Automatic startup

The n8 image packages this directory under `/opt/defaults/integrations/hermes/nemesis8`.
The Hermes provider copies it to `$HERMES_HOME/plugins/nemesis8` and adds `nemesis8`
to `plugins.enabled` at startup. Existing plugin enablement and user settings are
preserved; an explicit `plugins.disabled: [nemesis8]` remains authoritative.
The plugin's fleet guide is available through `skill_view` as
`nemesis8:nemesis8-fleet` (plugin skills require an explicit load).
The optional spawn `workspace` is an absolute host path; omit it unless known.

## Hermes SDK Signatures Verified

- Entry point: `register(ctx)`
- Tool registration: `ctx.register_tool(name=..., toolset='nemesis8', schema=..., handler=...)`
- Skill registration: `ctx.register_skill(name='nemesis8-fleet', path=Path(__file__).parent / 'skills' / 'nemesis8-fleet' / 'SKILL.md', description='Operate the Nemesis8 gateway and agent fleet')`
- Tool handlers accept `(args: dict, **kwargs)` and always return a JSON string (`json.dumps(...)`).
- Zero external runtime dependencies (uses Python standard library: `urllib.request`, `urllib.error`, `urllib.parse`, `json`, `socket`, `os`, `logging`, `pathlib`).

## Error Handling Guarantees

1. **Gateway Offline**: If the gateway is unreachable (transport/network error, connection refused, or timeout), tools return a calm structured JSON payload (`status: "gateway_not_running"`) detailing how to start the gateway without raising.
2. **Invalid `GATEWAY_URL`**: If `GATEWAY_URL` is malformed or lacks scheme/host, returns a structured error JSON payload (`status: "error"`).
3. **HTTP Responses**: Non-JSON HTML/text produces a structured error. A successful empty response returns `{"ok": true}`, matching n8gw.
4. **Input Type Validation**: Missing required parameters or wrong types for optional parameters (`provider`, `model`, `workspace`) are rejected with clear error messages before dispatching network calls.

## Environment Variables & Configuration

- `GATEWAY_URL`: Base URL of the Nemesis8 control plane (defaults to runtime alias `http://host.docker.internal:9801` / `http://host.containers.internal:9801`).
- `NEMESIS8_AUTH_TOKEN`: Optional Bearer token forwarded automatically in `Authorization: Bearer <token>` when set.

## Running Tests

Run the test suite using Python's standard `unittest`:

```bash
python3 -m unittest discover -s integrations/hermes/nemesis8/tests
```

All 19 unit tests pass, validating tool and skill registration, argument validation, URL encoding, HTTP headers/payloads, non-JSON error handling, invalid gateway URL validation, and graceful offline degradation.
