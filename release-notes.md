# nemesis8 v0.29.7 — Ollama cloud models in opencode, telemetry that reaches lume ☁️

opencode can now use Ollama's cloud models, and each container agent's lume telemetry store finally fills up. Get it: `n8 update`, then `n8 build`.

v0.29.6 was tagged but never published. GitHub retired the `macos-14` runner, so the Intel Mac build couldn't start and the release step never ran. That build now runs on `macos-latest` (#184). Everything listed below was meant for 0.29.6 and ships here.

## What changed

- **Ollama cloud models show up in opencode** (#183). Ollama runs cloud models like `glm-5.2:cloud` and `deepseek-v4-pro:cloud` without a pull, so they never appeared in the list n8 gave opencode. opencode rejected them (`Model ollama/glm-5.2:cloud is not valid`) and fell back to its free model. n8 now adds Ollama's cloud catalog to opencode's model list, and always includes your configured `OPENCODE_DEFAULT_MODEL` when Ollama can serve it. If it can't, you get a warning before opencode starts. Set `NEMESIS8_OLLAMA_CLOUD=0` to list pulled models only.
- **Telemetry stops getting stuck** (#181). The in-container pusher sent the agent's whole event history to lume as one batch. lume rejects batches that reach too far back, so in containers that reused an old name nothing was ever stored, and the retry errors flooded the exit menu. Now the pusher starts with new events only, skips a batch lume refuses, backs off when lume doesn't answer, and stops when the agent exits. If telemetry was lost during the session, you get one note before the exit menu saying how to turn it off (`NEMESIS8_LUME_TI=0`).
- **Tool calls go into the agent's own store** (#177, #178, #179). Each tool call the agent makes is written to its lume store, so the agent can search what it has already done, e.g. `match(body, 'cargo test')`. Covers claude, codex, grok, antigravity (now reading the 1.3.5 transcript format), omp, pi, and opencode.
- **Agents are told to check their own telemetry** (#175). The base prompt now points the agent at `ti_query` for its own CPU, memory, and other numbers, alongside `lume_search` for text.
- **opencode 2.x as a separate provider** (#180). opencode 2 ships as its own npm package (`@opencode/cli`), so it's a new opt-in provider, `opencode2` (alias `oc2`), that installs next to 1.x. It isn't in the default image; add `opencode2` to `INSTALL_PROVIDERS` to build with it. Provider TOMLs can now pin an install version with `install.version`.

The opencode, telemetry, and tool-call changes are in the container image. Run `n8 build` to pick them up in new sessions; containers that are already running keep the old behavior.

Coming from further back? [v0.29.5](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.29.5) was the previous published build (lume search guidance, quieter lume logs).
