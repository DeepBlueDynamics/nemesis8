# Plan: fully self-contained provider files

**Goal:** adding/removing a provider touches exactly ONE file — `providers/<name>.toml`
(or a user drop-in at `~/.nemesis8/providers/<name>.toml`). Everything needed to get a
provider running — install, config, login, auth sync, model/danger flags, sessions,
cosmetics — is data in that file. Zero provider names in `src/` outside tests.

The TOML schema (`provider_def.rs`) already covers ~90%: install (npm/curl/host),
config_dir (format/filename/mcp_key), prompt flags, danger, model, api_keys,
env_overrides, hooks (sessions/resume/auth_files_sync), validation. What remains is
**wiring the parsed-but-unread parts and deleting the hardcoded leftovers.**

## Current hardcode inventory (verified 2026-06-12)

| # | Site | What's hardcoded | Replacement |
|---|------|------------------|-------------|
| 1 | `docker.rs:1035` `login_cmd = match provider` | login commands for gemini / codex / claude only; antigravity + grok TOML `[provider.login]` is parsed into `LoginSpec` **but never read** — their login silently no-ops | read `spec.login.command` / `.env_vars` / `.ports`; keep one generic "no login required" fallback |
| 2 | `main.rs:304` | gemini-only OAuth preflight (`~/.gemini/oauth_creds.json` else bail) | new TOML `[provider.login.preflight]` { file, env_fallback, hint } — generic check before interactive |
| 3 | `docker.rs:1017` | gemini cred-sync (`~/.gemini/*` → volume) hardcoded in `into_login_args` | drive from existing `hooks.auth_files_sync` (gemini.toml already lists the files); generalize: sync any provider's listed auth files host→volume on every launch |
| 4 | `config.rs:34-38` `Provider::parse` | alias map (openai→codex, google→gemini, anthropic→claude) duplicated alongside TOML `aliases` | resolve aliases via `ProviderRegistry` (single source: the TOMLs) |
| 5 | `config.rs default_providers()` | hand-maintained list | derive from registry: all embedded providers (or add `default_install = true/false` per TOML for opt-outs) |
| 6 | `entry.rs:528-533 provider_emoji` | emoji per provider | TOML `emoji = "📜"` (fallback 🐙) |
| 7 | `provider_registry.rs:9-16 EMBEDDED` | manual `include_str!` list — easy to forget (nearly bit us adding grok) | **build.rs scans providers/*.toml and generates it** (= issue #34) |
| 8 | `docker.rs build_env` API-key list | OPENAI/ANTHROPIC/GEMINI/XAI/GROK keys hand-listed | forward the union of every registry provider's `api_keys.chain` + a small generic extras list (SERPAPI etc.) |
| 9 | `controlroom.rs:171` fallback `["codex","gemini","claude"]`, `:661` default `"codex"` | UI fallbacks | registry names; default = configured provider, else first registry entry |
| 10 | `provider_def.rs:113 default_model_env = CODEX_DEFAULT_MODEL` (+ `CODEX_SESSION_ID`, `CODEX_HOME`) | legacy env names cross the host↔container contract | rename with send-both/read-both window — already tracked as **#39 phase 2**; sequence LAST |

(`provider_def.rs` / `provider_registry.rs` test mentions are fine — tests may name providers.)

## Phases (each independently shippable, tested, releasable)

### P1 — wire `[provider.login]` (the real gap)
- `into_login_args` reads `spec.login.command`; absent → generic "no login required".
- `spec.login.env_vars` appended to env; `spec.login.ports` published (gemini 8766,
  codex's socat 1455 bridge moves INTO codex.toml's command, antigravity 8766).
- Fixes antigravity + grok login as a side effect.
- Acceptance: delete the `match` at docker.rs:1035; `n8 --provider antigravity login` runs `agy /login`.

### P2 — auth preflight + auth-file sync as data
- `[provider.login.preflight]` `{ file = "~/.gemini/oauth_creds.json", env_fallback = "GEMINI_API_KEY", hint = "run 'gemini auth login' on the host" }`; interactive path checks it generically (main.rs:304 deleted).
- `hooks.auth_files_sync` becomes the only cred-sync mechanism, applied per-provider at launch (docker.rs:1017 deleted).

### P3 — cosmetics + registry-derived defaults
- `emoji` field; aliases from registry; `default_providers` from registry (`default_install` flag); controlroom fallbacks from registry.

### P4 — build.rs embed + key forwarding (closes #34)
- build.rs generates the EMBEDDED array from `providers/*.toml` at compile time.
- `build_env` forwards keys from the union of registry `api_keys.chain`s.

### P5 — legacy env renames (#39 phase 2)
- `CODEX_DEFAULT_MODEL` → `NEMESIS8_MODEL` etc., sent+read both ways for ≥2 releases.

## Definition of done
```
grep -rE '"(codex|gemini|claude|antigravity|grok|ollama)"' src/ --include='*.rs' \
  | grep -v test
```
returns **nothing**. New-provider checklist = "write the TOML, done" (registry picks it
up from disk at runtime; build.rs embeds it at compile time; install/login/config/keys
all flow from the file).

## Risks / notes
- P1/P2 change login behavior for gemini/codex — port the EXACT current commands into
  the TOMLs first, diff `docker run` args before/after (golden test).
- P5 is the only phase with cross-version compat concerns; it ships alone.
- User drop-ins (`~/.nemesis8/providers/*.toml`) already override builtins — that
  mechanism is the escape hatch while phases land.
