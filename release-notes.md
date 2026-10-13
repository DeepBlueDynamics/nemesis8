# nemesis8 v0.29.5 — quieter containers, smarter search 🔎

Two changes to the in-container agent: it's pointed at `lume_search` for finding things, and lume's startup/index noise no longer bleeds into the agent's terminal. Get it: `n8 update`.

## What changed

- **Agents are told to reach for `lume_search`** (#173). A new guardrail in the base system prompt steers the container agent to lume's semantic + full-text search when it needs to *find* something by meaning — where a symbol lives, how an API is used, a relevant doc or past decision — while keeping `nuts_find` / `nuts_search` for exact-string and filename lookups.
- **lume's stderr goes to a log, not the TUI** (#174). `lume serve` and `lume index` now write their stderr to `/opt/nemesis8/lume/lume.log` instead of inheriting the agent's terminal, so extraction warnings and indexing chatter stop scrolling through the agent's view. The log is there when you need to look.

Both land in the container image — run `n8 build` to pick them up in new sessions.

Coming from further back? [v0.29.4](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.29.4) was the previous published build (sessions grouped by workspace).
