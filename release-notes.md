# nemesis8 v0.26.5 — Scheduled runs that report back, and Steve on the tool list 🧠

Two things in this release: the gateway's scheduler can now run a headless agent the way a host app needs it to, and n8 finds Ferricula memory identities (Steve) running on the machine and offers them to agents as MCP servers. To get it: `n8 update`, then `n8 build` so the container entry can report session ids, then recreate containers.

## Scheduled and spawned runs

- `POST /triggers`, `PUT /triggers/{id}` and `POST /agents/spawn` take `env`, `labels`, `identity` and `timeout_secs`. Env never overrides n8's own variables; `identity` names the container, the agent id and the Hyperia identity in one, and a taken name fails the run instead of being redrawn; the default timeout for agent runs is 900 s (it was the gateway's 120 s).
- A trigger records its last run: `last_status` is `running` from launch, `last_agent_id` the moment the container is named, `last_session_id` once the container reports its provider session, `last_finished_at` and `ok`/`error` at the end.
- The scheduler runs each fire as its own task. A long run no longer blocks the tick or other triggers, a trigger never overlaps itself, and deleting a trigger while its run is in flight stays deleted.
- `POST /agents/spawn` honours workspace, model and danger, runs through the same path as a trigger, and returns the agent id at once.
- Daily triggers fire. They never did, and they ignored their timezone; both fixed, with real IANA zones and DST handling.
- `n8 schedules` sends the gateway bearer, so it works against an authenticated gateway. `create` grew `--env`, `--identity` and `--timeout`.
- Contract: `docs/specs/scheduled-runs-api.md`.

## Ferricula identity discovery

- Any container carrying the `ferricula.identity` label is found at every launch through the Docker API and offered as an MCP server, over HTTP when it serves `/mcp` or through its stdio bridge when it does not.
- Opt-in per workspace: list the identity's name in `mcp_tools` (`steve`) or toggle it in the tools picker. `[integrations] ferricula_auto_enable = true` gives every identity to every agent; `ferricula_discovery = false` turns discovery off.
- `n8 mcp list` shows identities, state, mode, whether this workspace has them, the transport and whether the token is stored. The picker shows them by name instead of as stale files.
- Contract: `docs/specs/ferricula-discovery.md`.

Coming from further back? [v0.26.4](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.26.4) was the previous version on main; the last published build was [v0.26.3](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.26.3).
