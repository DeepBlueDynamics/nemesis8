# nemesis8 v0.29.7 — Ollama cloud models in opencode, working agent telemetry ☁️

opencode can use Ollama's cloud models, and agent telemetry is stored again. To get it: `n8 update`, `n8 build`, then recreate your containers.

## What changed

- **Ollama cloud models in opencode** (#183). Cloud models such as `glm-5.2:cloud` now appear in opencode's model list, and a configured `OPENCODE_DEFAULT_MODEL` works instead of falling back to opencode's free model. If Ollama can't serve the configured model, n8 warns before opencode starts. `NEMESIS8_OLLAMA_CLOUD=0` lists pulled models only.
- **Agent telemetry is stored** (#181). lume refused the telemetry containers sent, so agents' telemetry stores stayed empty and the errors could bury the exit menu. Containers now send only new events and drop a batch lume refuses instead of retrying it forever. `NEMESIS8_LUME_TI=0` turns telemetry off.
- **Agents can search their own tool calls** (#177, #178, #179). Every tool call goes into the agent's lume store. Works for claude, codex, grok, antigravity, omp, pi and opencode.
- **Agents check their own telemetry** (#175). The base prompt points agents at `ti_query` for their CPU, memory and other numbers.
- **opencode 2 provider** (#180). The opt-in `opencode2` provider (alias `oc2`) installs opencode 2 next to 1.x. Add it to `INSTALL_PROVIDERS` to include it. Provider TOMLs can pin a version with `install.version`.

These changes are in the container image. A container that's already running keeps the old behavior until you rebuild and recreate it.

Previous release: [v0.29.5](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.29.5).
