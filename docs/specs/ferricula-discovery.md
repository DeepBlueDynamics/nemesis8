# Ferricula identity discovery

n8 finds every Ferricula identity running on the host and offers it to agents
as an MCP server. Nothing to configure per identity, no image rebuild: the
identity's container carries Docker labels, n8 reads them at launch.

Offering is not giving. An identity's bearer may carry real power (Steve's
operator token writes his memory and drives his life loop), so an agent only
gets an identity when its workspace opts in.

## Label contract

Set on the identity's container (compose `labels:` or `docker run --label`):

| label | meaning | default |
|---|---|---|
| `ferricula.identity` | short name; becomes the MCP server name (`steve`) | required |
| `ferricula.mcp_port` | host port the identity is published on | first published port |
| `ferricula.mcp_path` | Streamable-HTTP MCP endpoint path | `/mcp` |
| `ferricula.bridge` | stdio bridge script (Python, MCP over stdio, REST to the identity); relative paths are under the compose project dir | none |
| `ferricula.token_env` | env var name of the bearer token, stored with `n8 secrets set <NAME>` | `FERRICULA_OPERATOR_TOKEN` |
| `ferricula.health_path` | unauthenticated route returning `{agent_id, mode}` | `/health` |

Steve's redeploy script sets `ferricula.mcp_port=18875`, `ferricula.mcp_path=/mcp`,
`ferricula.health_path=/health` and `ferricula.token_env=NUTS_AHP_TOKEN`: agents
present a nuts-auth token for a `reader` actor, never the operator token.

## What n8 does at every launch

1. Lists containers carrying `ferricula.identity` through the Docker API (no
   `docker` CLI; no container value reaches a command line).
2. For each running identity: `GET /health` (agent id, mode) and an
   unauthenticated `POST` to the MCP path to learn whether the running image
   has an MCP route.
3. Keeps its definition current, one of two ways:
   - **http** (the MCP route exists): writes
     `~/.nemesis8/home/.nemesis8/mcp/ferricula-<name>.toml`, the user MCP dir
     every container reads, with the endpoint as a container sees it
     (`http://host.docker.internal:<port>/mcp`), `bearer_token_env`, and
     `enabled_by_default = false`.
   - **bridge** (the route is 404 and a bridge is labelled): copies the script
     to `~/.nemesis8/home/mcp/ferricula-<name>.py` (where `n8 mcp add` puts
     tools) with a header that `setdefault`s `FERRICULA_BASE_URL`,
     `OLLAMA_BASE_URL` (both via `host.docker.internal`) and
     `<TOKEN_ENV>_FILE`.
4. Writes the token from the keychain to
   `~/.nemesis8/home/.n8/ferricula/<name>.token` (container path
   `/opt/nemesis8/.n8/ferricula/<name>.token`) when the secret is set.
5. **Enables it only if the workspace opted in**: `mcp_tools` lists the
   identity's name (`steve`), its alias (`ferricula-steve`) or its bridge
   file. The launch config then carries the right entry for the transport
   (`steve` for http, `ferricula-steve.py` for bridge) and the token env on
   `env_imports`, so the bearer is forwarded like any imported secret. A
   missing token is warned about only for an enabled identity.
6. Removes generated files (first line marks them) for identities that are
   gone or changed transport. Hand-written `ferricula-*` files are never touched.

When a container is recreated with an MCP route, the next launch switches it
from bridge to http and cleans the wrapper up; an opted-in name follows.

## Opting in

Per workspace, in `.nemesis8.toml`:

```toml
mcp_tools = ["steve"]
```

or toggle the identity in the tools picker (bare `n8`, Tools), where it is
listed by name with the tag `ferricula`. To give every identity to every agent:

```toml
[integrations]
ferricula_auto_enable = true
```

## Seeing it

- `n8 mcp list` prints the identities, state, mode, whether this workspace has
  them (`enabled` / `opt-in`), the transport the agent would get, and whether
  each token env is set.
- Launch logs: `integration: ferricula identity steve → MCP server `steve` via …`
  when enabled, or `… available via …; not given to this agent` when not.

## Off switch

```toml
[integrations]
ferricula_discovery = false
```

## Authorisation (Ferricula side)

Ferricula v3 validates non-operator bearers against nuts-auth and matches the
token's `actor` exactly against `[[auth.agents]]` in the identity's config,
with role `operator` or `reader`. The agreed shape: one `ahp_` token with actor
`nemesis8` and role `reader`, stored as `NUTS_AHP_TOKEN`, shared by every n8
agent that opts in. Per-agent actors come later with deterministic agent
identities (`nemesis8/n8-<workspace>-<provider>-<N>`), each listed.

## Known limits

- One token per identity for all n8 agents until per-agent minting exists.
- Only the first published port is considered when `ferricula.mcp_port` is absent.
- n8 does not deploy identities. Steve is deployed by Ferricula's own script;
  moving that to `n8 services` is a separate, one-time cutover.
