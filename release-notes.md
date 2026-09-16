# nemesis8 v0.25.3 — The desktop's token 🎟️

Hermes Desktop asks for a token when you connect it to a backend n8 is running. n8 now provides one: it generates a session token once, keeps it, injects it into the backend, and prints it when the backend starts. Paste it into the desktop a single time. To get it: `n8 update`.

## Why the desktop asked

`hermes serve` guards its API with a session token. When Hermes Desktop launches its own local agent it mints that token and hands it over, so the two just agree. A backend started by n8 got no such token, so Hermes minted a random one that nothing else could know — and the desktop, which stores a token per saved server, had to ask you for it. There was no way to answer.

## What n8 does now

When a provider's server takes a client token from an environment variable (`[provider.serve] session_token_env`; Hermes: `HERMES_DASHBOARD_SESSION_TOKEN`), `serve-backend` loads a token from `~/.nemesis8/home/serve-tokens/<provider>.token` — creating one the first time — injects it, and prints it at launch along with the file path:

```
Desktop token for hermes — paste it when the desktop asks for the server's token:
  <token>
  saved at C:\Users\you\.nemesis8\home\serve-tokens\hermes.token — same token on every restart; delete the file to rotate it
```

Because it's persisted, restarting the backend doesn't invalidate the desktop's saved connection. This applies on the default loopback-plus-tunnel path, where the token is the only auth; the `--no-tunnel` (`0.0.0.0`) path uses Hermes's own password/OAuth gate instead, which ignores it.

## Also

- The "Backend up" line no longer says "auth-free" when a token applies; it says the desktop just needs the token printed above.

Coming from further back? [v0.25.2](https://github.com/DeepBlueDynamics/nemesis8/releases/tag/v0.25.2) was the previous published build.
