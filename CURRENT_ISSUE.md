# Current Issue: MCP Tools Not Loading in Codex

## Status: FIXED — needs image rebuild

## Root Cause

The Codex config writer in `src/config.rs` was generating `[mcpServers.tool-name]` (camelCase) but Codex CLI reads `[mcp_servers.tool-name]` (underscore). The old codex-container system (`update_mcp_config.py`) used `mcp_servers` correctly. When we rewrote the config generation in Rust, we used the wrong key name.

Result: config.toml had the right tools registered under the wrong key. Codex ignored them all. Zero MCP tools in every session.

## Fix

`src/config.rs` line 320: changed `mcpServers` → `mcp_servers`. Committed in `f66a6f8`.

## What Still Needs to Happen

1. **Rebuild the Docker image** — `nemesis8 build` (the entry binary inside the image has the old code)
2. **Delete stale host config** — `del %USERPROFILE%\.codex-service\.codex\config.toml` (the old camelCase config persists on the host volume)
3. **Neutralize .mcp.json in workspaces** — any workspace with a `.mcp.json` containing Windows paths (Hyperia sidecar, ferricula) will also break MCP inside the Linux container. The entry binary now writes `{"mcpServers":{}}` over it, but only if the image has the latest entry binary.

## Contributing Factors

- `.mcp.json` from host workspaces gets mounted into the container. If it has Windows paths, Codex tries to start those MCP servers on Linux, they fail, and this may suppress all MCP.
- The codex-home volume (`~/.codex-service`) persists config across container restarts. Stale configs with wrong key names or dead tools survive image rebuilds.
- Gemini's `projects.json` had the wrong format — needed `{"projects": {"/path": "shortId"}}` not `{"/path": {"shortId": "x"}}`.
- Docker layer caching can serve stale entry binaries even after source changes. Use `docker build --no-cache` or the `CACHE_BUST` ARG to force fresh builds.

## Timeline

- **Original bug**: introduced when nemesis8 was first written (the Rust config generator always used camelCase)
- **Discovered**: 2026-03-27 by inspecting the old codex-container's `update_mcp_config.py`
- **Fixed**: 2026-03-28 commit `f66a6f8`
