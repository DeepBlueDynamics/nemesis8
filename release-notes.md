# nemesis8 v0.29.0 — Lume search in every container 🔎

Every container can now search its own code and its own telemetry with Lume. To get it: `n8 update`, then `n8 build`, then recreate your containers.

## LUME search

`n8 build` has a new option, **LUME search**, checked on by default (uncheck it in the build screen, or pass `--no-lume`, to skip it). When it's on, the image bundles the Lume v0.15.2 CLI and each container runs `lume serve` as an MCP tool, so the agent gets:

- **Code and doc search** over its workspace. The workspace is indexed in the background at start-up, so search is ready shortly after launch and updates incrementally.
- **SQL over its own telemetry.** The container's n8 activity — file edits, filesystem changes, and resource metrics — streams into a separate Lume TI store over OTLP and is queryable with `ti_query`, kept apart from the code index.

The binary is pinned to Lume v0.15.2 and verified against its published checksums at build time. A container built without the option simply doesn't register the tool.

Coming from further back? [v0.28.1](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.28.1) was the previous published build.
