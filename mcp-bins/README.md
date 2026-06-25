# mcp-bins

Rust **source** for nemesis8's in-house native MCP-server binaries. Each crate
here compiles to a `/usr/local/bin/*` binary inside the agent image; a matching
`mcp-servers/*.toml` registers that binary as an MCP server the agent can use.

This directory is *implementation*, not a registry — to add/remove which servers
an agent gets, edit `mcp-servers/` (config), not this dir.

| Crate | Binary (`/usr/local/bin/`) | Registered by | Purpose |
|---|---|---|---|
| `ask-rs`     | `ask`        | `mcp-servers/ask.toml`       | Second-opinion MCP (Claude/Gemini/OpenAI) |
| `n8gw`       | `n8gw`       | `mcp-servers/nemesis8.toml`  | Client for the nemesis8 gateway / control-plane |
| `nuts-files` | `nuts-files` | `mcp-servers/nuts-files.toml`| File MCP (read/write/edit/search/diff); path-deps `../../aegis-edit` |
| `shivvr`     | `shivvr`     | `mcp-servers/shivvr.toml`    | Embeddings client (embed / similarity / status) |

Other `mcp-servers/*.toml` entries have **no** crate here: `hyperia` is a remote
HTTP endpoint, `blender` runs via `uvx`.

## Build
Each crate is self-contained (its own `Cargo.lock`), built standalone in the
image — see the `cd /opt/nemesis8-build/mcp-bins/<crate> && cargo build --release`
steps in the repo `Dockerfile`. They are **not** members of the top-level cargo
workspace.

## Related taxonomy
- `MCP/` — Python stdio MCP **tools** (agent capabilities)
- `mcp-servers/` — MCP **server** config TOMLs (point at these binaries, or remote)
- `mcp-bins/` — *(here)* Rust source for the native MCP-server binaries
- `services/` — background sidecar containers · `apps/` — foreground TTY tools · `providers/` — AI agents
