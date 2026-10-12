# nemesis8 v0.29.2 — Lume search + telemetry, working 🔎

Lume now works end to end in every container: code/doc search and the TI telemetry store both come up, and indexing skips build output and junk files. To get it: `n8 update`, then `n8 build`, then recreate your containers.

## What changed

- **Lume bumped to v0.15.3**, which fixes the serve crash that took `lume serve` down at open — and poisoned its store so every later container crashed too — on 0.29.0/0.29.1. v0.15.3 collapses the duplicate telemetry points that tripped it (on capture, commit, and WAL replay), so an already-poisoned store opens clean. Verified against the real failing store before shipping.
- **Telemetry is back on by default** (`lume serve --ti-store … --otlp`); set `NEMESIS8_LUME_TI=0` for search-only serve.
- **Antigravity** reaches Lume through a stdio shim — its MCP client can't speak HTTP, so it was getting a broken entry before; codex/claude/opencode use Lume over native HTTP.
- **Indexing is .gitignore-aware** and skips `._*`, `.DS_Store`, and `node_modules`, so it no longer churns on build output or macOS AppleDouble sidecars.
- Plus a fleet-telemetry ring recency fix (#164).

Coming from further back? [v0.29.1](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.29.1) was the previous published build (Lume's serve was broken at runtime there; this release makes it actually work).
