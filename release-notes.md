# nemesis8 v0.26.1 — Speak as yourself 🪪

A container could end up talking to Hyperia as the pane that launched it instead of as its own agent. This release makes each container claim a Hyperia identity it can actually hold, says so plainly when it can't, and stops configuring two Hyperia clients that fight over one token. To get it: `n8 update`; the one-client rule reaches containers after `n8 build`.

## What went wrong

n8 mints a Hyperia identity per container, named after the container (`nemesis8/n8-proud-otter`). Container names come from a list of 2,304 combinations, and Hyperia keeps every identity name forever. Since Hyperia stopped re-issuing an existing identity's token by name, a launch whose random name had been used before got "Identity already exists" back. n8 read that as "no token", kept the launching pane's token, and started the container with it. On one host 26 of 262 stored container tokens were pane tokens from that fallback, and the odds of hitting a used name were already about one in eight per launch.

Separately, a workspace config that lists both `hyperia` (the HTTP server) and `hyperia-mcp.py` (the stdio shim) gave the agent two Hyperia clients with the same token. Hyperia allows one live session per token, so the second client was refused and died.

## What changed

- **Names and identities are chosen together.** When a drawn name is already registered, n8 first looks for that identity's credential in the per-container token file on this host and reuses it, which is what a relaunch of the same name should do. If there is no credential, it draws another name and tries again, up to twelve times.
- **No silent pane fallback.** If an identity cannot be claimed while Hyperia is running, the launch prints a warning saying the container will speak as this shell's pane, and why.
- **The mint reports what happened**: minted, name taken, or sidecar unreachable, instead of reading a token out of prose and turning every failure into "nothing".
- **One Hyperia client per agent.** When a config names both the HTTP server and the shim, the container keeps the shim (it re-reads the token file after a rotation) and logs that it dropped the other.
- Stale comments claiming Hyperia returns the same token for the same name are gone.

## Session ids announced to the terminal

Hyperia's Save Tab restores a pane with `n8 resume <id>`, using the session id n8 announces when the agent starts. For grok that id was a state file's name (`1790270046-43`), because the entry scanned grok's whole config dir and took the first new file; for opencode nothing was announced at all, because its sessions are rows in a database, not files. Now the entry scans only the provider's declared session directories, applies grok's uuid-directory rule, polls the opencode and hermes databases for a new row belonging to this container's workspace, and never announces anything that does not look like a session id.

Coming from further back? [v0.26.0](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.26.0) was the previous published build.
