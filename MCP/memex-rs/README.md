# memex-rs (MCP)

Rust MCP memory server implementing the `memory.md` contract surface:

- `remember(event, owner_agent_id, role, tags, quality_hint)`
- `recall(query, owner_agent_id, scope, k, recency_bias)`
- `offer(memory_id, from_agent, to_agent)`
- `dream(owner_agent_id, budget)`
- `status(owner_agent_id)`

## What this version includes

- SQLite-backed records + append-only audit log
- Deterministic local embeddings (384d hashed vectors; no external model needed)
- Hybrid recall scoring (lexical + semantic + fidelity/importance/recency/centrality/keystone)
- Thermodynamic lifecycle handling in `dream`:
  - decay updates
  - ACTIVE -> FORGIVEN -> ARCHIVED transitions
  - keystone promotion
  - near-duplicate consolidation clusters

## Build

```bash
cd /workspace/codex-container/memex-rs
cargo build --release
```

## Run manually (stdio MCP)

```bash
MEMEX_DB_PATH=./data/memex.sqlite \
./target/release/memex-mcp
```

## Install with Codex MCP (`mcp add`)

```bash
codex mcp add memex-rs -- /workspace/codex-container/memex-rs/target/release/memex-mcp
```

If your launcher is `gnosis-container.ps1`, run this inside that container shell after build.

## Persisted data

- Default DB path: `./data/memex.sqlite` (relative to working dir)
- Override with env: `MEMEX_DB_PATH`

## Notes

- This is a practical v1. Graph extraction is represented via centrality + consolidation behavior.
- For v2, swap `embed_text` with ONNX embedding model and add explicit graph edge tables + traversal.
