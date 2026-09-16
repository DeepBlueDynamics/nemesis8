# nemesis8 v0.25.0 — Hermes on the tunnel 🪽

nemesis8 can now run a provider's backend server in a container and hand you the URL. The first one is Hermes: `n8 --provider hermes serve-backend` starts `hermes serve` inside an n8 container, and Hermes One Desktop connects to it at `http://127.0.0.1:<port>` — no login prompt, no hand-written `docker run`. To get it: `n8 update`, then `n8 build`.

## A Hermes backend, in a container

`n8 --provider hermes serve-backend --serve-port 9119` launches the container detached (it comes back with Docker) and prints the Server URL for the desktop. It reuses everything n8 already sets up for a Hermes session: the config that points Hermes at your local Ollama through `host.docker.internal`, your enabled MCP tools, the bundled `nemesis8` plugin, and the workspace mount. The desktop is talking to the same Hermes an `n8 interactive` session would run.

## Why there's no login

Hermes now refuses to serve on a non-loopback address without an auth provider (a password or Nous Portal OAuth). Publishing the port the usual way (`-p`) makes Hermes bind `0.0.0.0`, so it stops and asks for one. nemesis8 sidesteps that: Hermes binds `127.0.0.1` inside the container, and the port reaches your machine over nemesis8's reverse tunnel — the same one that carries OAuth callbacks. Loopback means Hermes asks for nothing.

This needs the gateway running (`n8 serve --background`). If it isn't, `serve-backend` falls back to a direct publish and tells you Hermes will want auth; `--no-tunnel` forces that path on purpose.

## Any provider, no code

The server is described in the provider's TOML, not in Rust. A `[provider.serve]` block names the subcommand, the host and port flags, and a default port. Hermes ships with one; any provider that adds the block gets `serve-backend` with no code change.

## Second launches behave

Bringing this up on a real Docker Desktop machine surfaced the ways a second `serve-backend` could go wrong, and each is handled. If a backend already owns the port and is running, the command says so and launches nothing. If the old container is gone, its stale tunnel mapping is released instead of blocking the port, and the gateway now drops the mapping of any exited or removed container on its own. The tunnel hookup retries the transient failure that can happen right after a container starts. A random container name that collides with an old exited container is re-rolled. And "Backend up" prints only once Hermes actually answers.

## Also

- `--serve-port` was silently overwriting the global `--port` (a clap arg-id collision); fixed.
- Release names and notes now come from `release-notes.md` in the repo, and the release workflow posts a status embed to Discord once the bot secrets are set.

Coming from further back? [v0.24.2](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.24.2) was the previous published build.
