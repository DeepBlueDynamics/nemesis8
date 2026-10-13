# nemesis8 v0.29.3 — per-container telemetry, clean Windows output 🧼

Three fixes. Get them with `n8 update`, then `n8 build`, then recreate your containers.

## What changed

- **Each container gets its own Lume TI telemetry store** (#169). Every container's `lume serve` pointed at the same telemetry WAL on the shared `/opt/nemesis8` bind-mount, which is single-vessel: the first agent stamped it with its vessel URN, and every later container failed ingest with `WAL vessel URN mismatch` — spraying the error into the agent's TUI, even on a brand-new, fresh-start container. The store is now keyed per-agent, so each container has its own WAL. Fleet aggregation is unaffected — it flows through the OTLP tailer push, not this container-local store.
- **UTF-8 output renders correctly on Windows consoles** (#168). `n8 --help` and logs showed `ΓÇö` for `—` and `Γèò` for `⊕`, because Windows decoded n8's UTF-8 in the legacy OEM codepage. n8 now sets the console to UTF-8 (code page 65001) at startup. No-op on macOS/Linux.
- **Registry stdio MCP servers get the gateway identity + token** (#167). The stdio path dropped `MCP_FORWARD_ENV` / `bearer_token_env`, so the n8gw registry server came up with no auth env. Both are forwarded now.

Coming from further back? [v0.29.2](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.29.2) was the previous published build (Lume search + telemetry working end to end).
