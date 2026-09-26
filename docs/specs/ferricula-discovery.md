# Ferricula identity discovery

n8 registers every Ferricula identity running on the host as an MCP server for
every agent it launches. Nothing to configure per identity, no image rebuild:
the identity's container carries Docker labels, n8 reads them at launch.

## Label contract

Set on the identity's container (compose `labels:`):

| label | meaning | default |
|---|---|---|
| `ferricula.identity` | short name; becomes the MCP server name (`steve`) | required |
| `ferricula.mcp_port` | host port the identity is published on | first published port |
| `ferricula.mcp_path` | Streamable-HTTP MCP endpoint path | `/mcp` |
| `ferricula.bridge` | stdio bridge script (Python, MCP over stdio, REST to the identity); relative paths are under the compose project dir | none |
| `ferricula.token_env` | env var name of the bearer token, stored with `n8 secrets set <NAME>` | `FERRICULA_OPERATOR_TOKEN` |
| `ferricula.health_path` | unauthenticated route returning `{agent_id, mode}` | `/health` |

Example (Steve):

```yaml
services:
  steve:
    ports:
      - "127.0.0.1:18875:8875"
    labels:
      - "ferricula.identity=steve"
      - "ferricula.mcp_port=18875"
      - "ferricula.mcp_path=/mcp"
      - "ferricula.bridge=scripts/steve_mcp_bridge.py"
      - "ferricula.token_env=FERRICULA_OPERATOR_TOKEN"
```

## What n8 does at every launch

1. `docker ps --filter label=ferricula.identity` and `docker inspect` for name,
   state, labels and published ports.
2. For each running identity: `GET /health` (agent id, mode) and an
   unauthenticated `POST` to the MCP path to learn whether the running image
   has an MCP route.
3. Register it, one of two ways:
   - **http** (the MCP route exists): writes
     `~/.nemesis8/home/.nemesis8/mcp/ferricula-<name>.toml` — the user MCP
     dir every container reads — with the endpoint as a container sees it
     (`http://host.docker.internal:<port>/mcp`) and `bearer_token_env`.
     `mcp_tools` gets `<name>`.
   - **bridge** (the route is 404 and a bridge is labelled): copies the script
     to `~/.nemesis8/home/mcp/ferricula-<name>.py` (the same dir `n8 mcp add`
     uses) with a header that `setdefault`s `FERRICULA_BASE_URL`,
     `OLLAMA_BASE_URL` (both via `host.docker.internal`) and
     `<TOKEN_ENV>_FILE`. `mcp_tools` gets `ferricula-<name>.py`; the container
     runs it with its own Python as a stdio server.
4. Writes the token from the keychain to
   `~/.nemesis8/home/.n8/ferricula/<name>.token` (container path
   `/opt/nemesis8/.n8/ferricula/<name>.token`) and adds the token env to
   `env_imports`, so it is forwarded like any imported secret. Missing token:
   the identity is still registered and a warning names the secret to store.
5. Removes generated files (first line marks them) for identities that are
   gone or changed transport. Hand-written `ferricula-*` files are never touched.

When a container is recreated with an MCP route, the next launch switches it
from bridge to http and cleans the wrapper up.

## Seeing it

- `n8 mcp list` prints the identities, their state and mode, which transport
  the agent will get, and whether each token env is set.
- Launch logs: `integration: ferricula identity steve → MCP server `steve` via …`.

## Off switch

```toml
[integrations]
ferricula_discovery = false
```

## Known limits

- One token per identity, shared by every agent (the operator token). Per-agent
  scoping needs Ferricula to mint per-caller tokens, as Hyperia does.
- Discovered identities are enabled for every agent. Per-workspace opt-in can
  be added on top of `mcp_tools` if that becomes noisy.
- Only the first published port is considered when `ferricula.mcp_port` is absent.
