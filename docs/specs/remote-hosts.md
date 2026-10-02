# Remote hosts: another machine's gateway in the control room

Status: shipped in the `feat/remote-hosts` branch (n8 0.26.7).

## What it does

A second machine running `n8 serve` (say, `nemesis`) shows up in the local
control room next to this machine:

- **Running tab** and **Sessions tab** gain a `HOST` column (only once a remote
  is configured). Local rows come first, then each remote's rows grouped by
  host. The filter (`/`) matches host names too.
- **Top bar** gets one badge per remote: `● nemesis v0.26.7` when it answers,
  `○ nemesis unreachable` when it does not, `◌ nemesis …` while the first poll
  is in flight.
- **Session menu** gains `New session on <host>` for every remote. It opens the
  usual New modal (provider, model, danger); confirming starts an
  **interactive** agent on that machine and attaches this terminal to it, so
  the agent's TUI streams into the local pane (a Hyperia pane included).
- `a`/Enter on a remote row attaches over the gateway's PTY WebSocket; Enter on
  a remote session resumes it there (a new interactive agent with
  `session_id`). `k` kills through the gateway. Delete/logs are local-only for
  now.
- `n8 attach nemesis/n8-merry-lemur` and `n8 shell nemesis/…` route by the host
  prefix (the id form `n8 agents list` prints against a remote). An unknown
  prefix is a clear error instead of Docker's "No such container".
- `n8 interactive --remote http://host:9801` starts an interactive agent there
  and attaches.

## Configuration

Global config (`~/.nemesis8/config.toml`), managed by `n8 remotes`:

```toml
[[remotes]]
name = "nemesis"
url = "http://nemesis.local:9801"
token_env = "NEMESIS8_TOKEN_NEMESIS"   # keychain / env name; default NEMESIS8_TOKEN_<NAME>
# token = "…"                           # inline only when there is no OS keychain
```

```
n8 remotes                       # table: NAME URL STATUS TOKEN AGENTS
n8 remotes add nemesis http://nemesis.local:9801        # hidden token prompt
ssh nemesis cat ~/.nemesis8/gateway-token | n8 remotes add nemesis http://nemesis.local:9801 --token-stdin
n8 remotes add open-box http://10.0.0.5:9801 --no-token # gateway without auth
n8 remotes rm nemesis                                   # keychain token is kept
```

The single `remote = "…"` / `remote_token` pair still works and is listed as
one host named after its URL's host.

Tokens are resolved on the host side (`RemoteGateway::resolve_token`: inline →
keychain → process env) before the control room starts; the TUI never touches
the keychain.

## Gateway: interactive spawn

`POST /agents/spawn` accepts two new fields:

```json
{ "interactive": true, "provider": "claude", "model": null, "danger": false,
  "workspace": null, "session_id": null, "env": {}, "labels": {}, "identity": null }
```

- `interactive: true` starts a TTY container running `nemesis8-entry
  --interactive` (the provider's own UI) and answers at once with
  `{"status":"running","agent_id":"n8-…","message":"…"}`. No prompt, no
  timeout, no slot taken from `max_concurrent`.
- `session_id` (with `interactive`) resumes that provider session.
- `prompt` together with `interactive` is a 400 ("takes no prompt").
- `workspace` is a path on the gateway's machine; omitted → the gateway's own
  (the directory `n8 serve` runs in).
- Config resolution matches a headless run: the workspace's layered config,
  then provider/model/danger overrides, else the gateway's.
- Name clash (`409`), charon enabled (`500`, interactive needs a plain Docker
  container).

The client then attaches with `GET /agents/{id}/pty?mode=attach`
(`pty_client::run(.., PtyMode::Attach)`). When the attach ends and the
container has exited, the gateway honours the exit choice the entry recorded
(`exit_choice::take_choice`): Remove → container deleted, Stop → kept for a
later `n8 attach`, none recorded (older image, plain detach) → left alone.

## Registry fix that came with it

`Registry::reconcile` treated every container in `docker ps -a` as live, so an
agent whose container had stopped stayed `running` for ever (ten phantom agents
on one real gateway). A container now counts as live only when its Docker
state is `running`/`restarting`/`paused`; a stopped one marks its record
`Exited` (and a never-registered stopped one is listed as `Exited`). A running
container flips an exited record back.

## Control-room row filter

A remote's rows come from its `GET /agents`. Containers the gateway merely
discovered on its Docker (a memory agent, a database) are not n8 agents and are
skipped: a row is kept when it has a provider or its id starts with `n8-`.

## Not in this change

- Workspace choice for a remote new session (the gateway's default is used).
- Remote delete / logs / tools in the control room.
- Discovering remotes (mDNS); hosts are configured explicitly.
