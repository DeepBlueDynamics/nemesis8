# Scheduled and spawned runs — gateway API contract

How a host app (Hyperia's sticky runs, a script, `n8 schedules`) starts a
headless agent run through the gateway and learns how it went. All routes are
on the gateway (`http://127.0.0.1:9801`), bearer-gated with
`NEMESIS8_AUTH_TOKEN` (keychain service `nemesis8`, user `NEMESIS8_AUTH_TOKEN`;
Windows Credential Manager target `NEMESIS8_AUTH_TOKEN.nemesis8`).

## Run options (shared by triggers and spawn)

| field | type | meaning |
|---|---|---|
| `workspace` | string | host directory mounted as the workspace; its layered `.nemesis8.toml` (mcp_tools, env) drives the run. Default: the gateway's workspace. |
| `provider` | string | `codex`, `claude`, `grok`, … Default: the workspace/gateway config. |
| `model` | string | model override. |
| `danger` | bool | skip approvals and sandboxing. Default: the gateway's `--danger`. |
| `env` | object of string → string | extra environment for the run's container. n8's own variables win on a clash; `NEMESIS8_AGENT_ID`, `NEMESIS8_AUTH_TOKEN`, `NEMESIS8_CONFIG_JSON`, `NEMESIS8_PROVIDER`, `NEMESIS8_WORKSPACE`, `NEMESIS8_HOST_WORKSPACE`, `GATEWAY_URL`, `HYPERIA_AGENT_TOKEN`, `HOME`, `PATH` are refused with 400. Keys must be valid variable names. |
| `labels` | object of string → string | extra Docker labels on the container. `nemesis8.*` keys are refused. Values ≤ 4096 bytes. |
| `identity` | string | requested agent name: the container name, the agent id and the Hyperia identity `nemesis8/<identity>` in one. 2–63 chars of `[A-Za-z0-9_.-]`, starting alphanumeric. Held by an existing container (running or exited) or by a Hyperia identity this host cannot claim → the run fails (`last_error` on a trigger, 409 on spawn); it is never redrawn. Omit for a random `n8-<adjective>-<animal>` name. |
| `timeout_secs` | integer | stop the run after this long. 10–86400; default 900. The container is stopped and the run reports `error: container timed out after Ns`. |

The run's container is one-shot (`nemesis8-entry --prompt …`), removed when it
ends. While it runs it is an agent: `GET /agents/{agent_id}` shows it, with
`state` `Starting` → `Running` → `Exited`, and `last_prompt`.

## POST /triggers → TriggerRecord

Body: `title`, `description?`, `prompt_text`, `schedule`, `tags?[]`, plus the
run options above at top level (`workspace`, `provider`, `model`, `danger`,
`env`, `labels`, `identity`, `timeout_secs`).

`schedule` is one of

```json
{"type": "once", "at": "2026-09-26T20:00:00Z"}
{"type": "daily", "time": "09:30", "timezone": "America/Los_Angeles"}
{"type": "interval", "minutes": 15}
```

`once` with `at` in the past fires on the next scheduler tick (30 s): that is
"run now". `daily` time is wall-clock in the IANA `timezone` (default `UTC`;
unknown zones are 400). A daily trigger created after today's time has passed
first fires tomorrow. `interval` starts with an immediate fire.

Validation failures are `400 {"error": "…"}`.

## TriggerRecord

```json
{
  "id": "0f3a9c2b1d4e5f60",
  "title": "sticky 42", "description": "", "prompt_text": "…",
  "schedule": {"type": "once", "at": "…"},
  "created_by": "", "created_at": "…", "enabled": true, "tags": [],
  "run": {"workspace": "…", "provider": "codex", "danger": true,
          "env": {"HYPERIA_STICKY_ID": "42"}, "labels": {"hyperia.sticky": "42"},
          "identity": "sticky-42", "timeout_secs": 600},
  "last_fired": "…",         // when the last run was LAUNCHED
  "last_status": "running",  // "running" | "ok" | "error"; absent before the first fire
  "last_error": null,        // set with "error"
  "last_finished_at": null,  // set when the run ends, either way
  "last_agent_id": "sticky-42",   // set the moment the container is named
  "last_session_id": null    // set when the container reports its provider session
}
```

Empty `env`/`labels` and unset options are omitted from `run`.

Follow a run by polling `GET /triggers/{id}`:

1. `last_status` becomes `"running"` and `last_fired` is set when the scheduler
   launches it. `last_agent_id` appears within a second or two; from then on
   `GET /agents/{last_agent_id}` is the live container.
2. `last_session_id` appears once the provider has written its session (the
   container's entry reports it). It stays null for providers that write no
   session file.
3. `last_status` becomes `"ok"` or `"error"` (with `last_error`) and
   `last_finished_at` is set. A `once` trigger is also disabled.

A trigger never overlaps itself: an `interval` or `daily` fire while the
previous run is still in flight is skipped until it ends. The gateway runs at
most `max_concurrent` (2) runs at once; a due trigger that finds no slot stays
due for the next tick. Deleting a trigger while its run is in flight is honoured:
the run finishes, the record stays deleted.

## Other trigger routes

- `GET /triggers` — all records. `GET /triggers/{id}` — one.
- `PUT /triggers/{id}` — any of `title`, `description`, `prompt_text`,
  `schedule`, `enabled`, `tags`, and the run options. `workspace`, `provider`,
  `model`, `identity` set to `""` clear the override; `timeout_secs: 0`
  restores the default; `env` / `labels` replace the whole map.
- `DELETE /triggers/{id}` — 204.
- `GET /status` — `scheduler: {trigger_count, enabled_count, next_fire}`.

## POST /agents/spawn → 200 SpawnAck

Body: `prompt` plus the run options. Same launch path as a trigger fire, no
record. Returns as soon as the container is named:

```json
{"status": "spawning", "agent_id": "n8-fun-lark",
 "message": "agent n8-fun-lark launching; poll GET /agents/n8-fun-lark (Starting → Running → Exited)"}
```

- `400` bad prompt or run options, `409` the requested identity is taken,
  `429` the gateway is at its concurrency limit, `500` the run could not start.
- Follow it with `GET /agents/{agent_id}`; `session_id` on that record is the
  provider session once reported. The run's output is not kept; the container
  is removed when it exits.

## POST /agents/{id}/register

The container's entry calls this on boot (`provider`, `workspace`, `pid`,
`container_name`) and again with `session_id` when the provider session is
known. Every field is optional and merges into the existing record.

## CLI parity

`n8 schedules create --title … --prompt … (--once … | --daily HH:MM [--timezone Z] | --every N)
[--env K=V]… [--identity NAME] [--timeout SECS]` posts the same body with the
current directory as `workspace` and the global `--provider/--model/--danger`.
