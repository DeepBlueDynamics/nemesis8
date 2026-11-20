# nemesis8 v0.29.6 — agent telemetry that reaches lume 📈

Each container agent has its own lume telemetry store. This release fixes the pipe that feeds it, adds the agent's tool calls to it, and tells the agent it can query it. Get it: `n8 update`, then `n8 build`.

## What changed

- **Telemetry stops getting stuck** (#181). The in-container pusher sent the agent's whole event history to lume as one batch. lume rejects batches that reach too far back, so in containers that reused an old name nothing was ever stored, and the retry errors flooded the exit menu. Now the pusher starts with new events only, skips a batch lume refuses, backs off when lume doesn't answer, and stops when the agent exits. If telemetry was lost during the session, you get one note before the exit menu saying how to turn it off (`NEMESIS8_LUME_TI=0`).
- **Tool calls go into the agent's own store** (#177). Each tool call the agent makes is written to its lume store, so the agent can search what it has already done, e.g. `match(body, 'cargo test')`. Works for providers with JSONL transcripts (claude, codex, grok, antigravity).
- **Agents are told to check their own telemetry** (#175). The base prompt now points the agent at `ti_query` for its own CPU, memory, and other numbers, alongside `lume_search` for text.

All three are in the container image. Run `n8 build` to pick them up in new sessions; containers that are already running keep the old behavior.

Coming from further back? [v0.29.5](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.29.5) was the previous published build (lume search guidance, quieter lume logs).
