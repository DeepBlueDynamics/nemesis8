# nemesis8 v0.29.4 — sessions grouped by workspace 🗂️

The session views now organize around the workspace, and the control room's gateway status moved out of the top menu onto its own bar. Get it: `n8 update`.

## What changed

- **The control room's Sessions tab groups by workspace** (#171). Sessions sit under bold `/workspace/…` headers (sorted, unknown bucket last); the repeated per-row workspace column is gone and the id is shortened to its first 8 chars, indented under its header. `/` search still filters id / provider / workspace / time.
- **Gateway badges moved to their own bottom bar** (#171). The local `gw ●/○ …` badge and the per-remote gateway badges left the top menu for a dedicated status row at the very bottom, so the top row stays just navigation.
- **The resume/attach picker groups by workspace too** (#170). `n8 resume` / `n8 attach` show the same workspace headers and short 8-char ids, with `/` search intact.

Coming from further back? [v0.29.3](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.29.3) was the previous published build (per-container telemetry, clean Windows output).
