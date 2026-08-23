# Plan: OAuth login for containerized providers (fx / omp / codex / …)

Status: proposed. Owner: TBD. Supersedes the socat login bridges in
`providers/{fx,omp,codex}.toml`.

## Problem

OAuth CLIs run **inside** a container. The provider hardcodes
`redirect_uri = http://localhost:<PORT>/...` (fixed per provider), and the CLI
binds its callback server on `127.0.0.1:<PORT>` inside the container. The host
browser must reach that in-container loopback at exactly `localhost:<PORT>`. It
can't, so login never completes.

### Verified facts (live, 2026-08-23, image nemesis8:latest)

- `fx login codex` binds `127.0.0.1:1455`, advertises
  `redirect_uri=http://localhost:1455/auth/callback`.
- If `1455` is already taken, fx **silently fails over to `1457`** (unpublished)
  — any bridge pinned to 1455 then feeds a *stale* fx, so the browser shows
  "authorization complete" while the live fx never gets its code. This is what
  broke the last attempt.
- Normal `n8 --provider X` **session** containers publish **no ports**
  (`docker ps` → `PORTS=[]`). Only the dedicated `n8 --provider X login` path
  publishes `login.ports` (`docker.rs into_login_args`, ~line 1850).
- `chisel` is in the image (`/usr/local/bin/chisel`); containers resolve
  `host.docker.internal`.
- A chisel **reverse** tunnel `host:1455 → container 127.0.0.1:1455` works on an
  already-running session (verified: host `curl localhost:1455/auth/callback`
  returned HTTP 400 straight from fx's callback server). It's outbound from the
  container, so it needs no `-p` on the session container.
- The chisel server was **not** running (no host `:9803`, no sidecar) — the
  tunnel plane was down.

### Fixed per-provider callback ports

| Provider route | Port |
|---|---|
| fx / codex (ChatGPT OAuth) | 1455 |
| omp anthropic | 54545 |
| omp / gemini | 8085 |
| antigravity | 51121 |
| gitlab-duo | 8080 |

## Why the current approaches are wrong

- **socat in a TOML `command`** (fx/omp/codex today): binding socat to
  `0.0.0.0:1455` swallows `127.0.0.1:1455`, so the CLI fails over to 1457. The
  eth0-bind workaround now committed makes the *dedicated login container* work,
  but it's fragile, shell-in-a-string, and only covers `n8 … login`, never an
  in-session login.
- **`-p 1455:1455` on the session** can't be retrofitted to a running container,
  and two sessions can't both publish the same host port.
- The **chisel plane already solves this shape** (reach a container's loopback
  from the host) and is the right tool — it just isn't wired into login.

## The fix: in-session reverse tunnel via the chisel plane

When a session's provider needs OAuth, open a chisel **reverse** tunnel
`host:<PORT> → container 127.0.0.1:<PORT>` so the CLI's own callback server
receives the browser redirect. No socat, no `-p` on the session, no bind
collision (the CLI owns `127.0.0.1:<PORT>`; chisel *connects out* to it).

### Flow

1. Provider TOML declares the fixed callback ports (and an auth-marker file used
   to detect "already logged in").
2. n8 ensures the host chisel server is up (reuse `tunnel::ensure_chisel_server`;
   today it only starts under `n8 serve` — the login/session path must ensure it
   too).
3. On session start, if the provider declares callback ports **and** the auth
   marker is absent:
   - For each port, check the host port is free (`tunnel::port_accepts`). If
     busy, log "login callback :<PORT> is in use — finish the other login first"
     and skip (do **not** fail the session).
   - After the container is up, start a chisel **client** inside it
     (`docker exec -d … chisel client <host-chisel> R:<PORT>:127.0.0.1:<PORT>`).
4. The CLI binds `127.0.0.1:<PORT>` when the user runs its login; the browser
   callback flows host → chisel server → chisel client → CLI. Login completes.
5. Tear the client down on login success or session exit (dies with the
   container regardless).

### Why reverse-client, not socat/-p

- Works on an already-running container (outbound connect).
- No `127.0.0.1` vs `0.0.0.0` collision — the CLI keeps its default port, so no
  1457 failover.
- Reuses the existing Rust tunnel infra instead of a second mechanism.

## Concurrency

`host:<PORT>` is a singleton — OAuth loopback is inherently single-flight per
port. One session logs in on a given port at a time; a second concurrent attempt
gets a clear refusal, not a silent stale-fx success. Acceptable: simultaneous
logins of the same provider are rare.

## Implementation

- `providers/*.toml`: add `[provider.login].callback_ports = [ … ]` and
  `auth_marker = "<path under HOME>"` (fx: `.fx/chatgpt-auth.json`, omp:
  `.omp/agent/agent.db`, codex: `.codex/auth.json`). **Remove** the socat
  `command` bridges from fx/omp/codex.
- `src/provider_def.rs`: parse `callback_ports: Vec<u16>` and `auth_marker`.
- `src/tunnel.rs`: allow a reverse tunnel on a **fixed** port (not just the
  18000–18999 allocation range); add a helper to open
  `R:<port>:127.0.0.1:<port>` into a named container.
- `src/docker.rs`: in the session run path, after the container starts, open the
  login tunnel(s) when the provider declares ports and the marker is absent;
  ensure the chisel server is up first. Keep `into_login_args` working as the
  explicit fallback, but switch it to the tunnel too (drop the `-p`+socat).
- `src/gateway.rs` / `src/main.rs`: ensure the chisel server can be started for
  the login path without a full `n8 serve`.
- No `entry.rs` change is required if the chisel client is `docker exec`'d from
  the host — so **no image rebuild** for the core fix. (If the client is baked
  into the entry binary instead, that's Channel C + a rebuild.)

## Edge cases / risks

- **Host/container reachability**: on Docker Desktop the client reaches the host
  chisel server via `host.docker.internal`; on Linux bridge a `127.0.0.1`-bound
  host server isn't reachable — `ensure_chisel_server_container` already handles
  this by running chisel in a container binding `0.0.0.0`. Reuse that path and
  make sure the callback port is among what it publishes/binds.
- **Timing**: start the chisel client at session start; it retries per
  connection, so it's fine that the CLI binds `1455` only later.
- **Port failover**: since nothing else occupies `127.0.0.1:1455`, the CLI binds
  its default and the 1457 failover never triggers. Optionally verify by parsing
  the CLI's printed `redirect_uri`.

## Test plan

1. Unit: `provider_def` parses `callback_ports` + `auth_marker`.
2. Integration: start an fx session with no creds → assert host `:1455` forwards
   to the container's fx (curl → HTTP 400 / a real fx response).
3. Concurrency: second fx session mid-login → clear refusal, first unaffected.
4. E2E (manual): `n8 --provider fx`, `/login`, browser completes, creds written
   to `~/.fx`, restart session → authed with no second login.
5. Regression: codex + omp login still complete.

## Cosmetic (not a bug)

The browser's "Authorization complete, you can return to fx" is fx's own static
success page; n8 doesn't control that text.
