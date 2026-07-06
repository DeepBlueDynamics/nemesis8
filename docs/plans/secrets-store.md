# `n8 secrets` — OS-keychain secret store + machine bootstrap (issue #52)

Hyperia keeps tokens in the OS keychain; n8 keeps them in **plaintext TOML,
host env vars, and loose files in the data home**. This plan gives n8 a
keychain-backed store with masked TUI/CLI management, keychain-first injection
at launch, and — rolled in from the field — a **new-machine bootstrap** for the
provider-login state that today silently makes a second box "not work."

Written against **v0.18.11**. Verify line numbers before editing.

## Motivating incidents (why now)

1. An agent wrote `ANTHROPIC_API_KEY` into codex `config.toml` in plaintext
   (the original #52 trigger).
2. **2026-07-03**: glm-5.2 "missing" in opencode on the laptop. Root cause: the
   Z.ai coding-plan key lives in
   `~/.nemesis8/home/.local/share/opencode/auth.json` on the desktop's
   data-home volume — invisible, unmanaged, and absent on any new machine.
   Diagnosis took a code-dive; it should have been
   `n8 secrets list` showing `zai-coding-plan  (opencode login)  desktop:set laptop:—`.

## Inventory — every secret flow n8 touches today

| # | Flow | Where | At rest |
|---|---|---|---|
| 1 | `[env]` table | `.nemesis8.toml` / `~/.nemesis8/config.toml` → `config.container_env()` | **plaintext TOML** |
| 2 | `env_imports` | host env → container | host env |
| 3 | Integration list (`SERPAPI_API_KEY`, `ELEVENLABS_API_KEY`, `HYPERIA_URL/_AGENT_TOKEN`, `FERRICULA_URL`, …) | `docker.rs::build_env` ~L1656 | host env |
| 4 | Provider key chains — union of every `providers/*.toml` `[provider.api_keys].chain/.target` | `docker.rs` ~L1698 | host env |
| 5 | GitHub token | `docker.rs::resolve_github_token` ~L457 (gh CLI/env) | gh's store |
| 6 | Registry MCP bearer tokens (`bearer_token_env`: `HYPERIA_AGENT_TOKEN`, `MERIDIAN_AGENT_TOKEN`) | `mcp-servers/*.toml` → `docker.rs` ~L1714 + `config.rs::socket_headers` | env (Hyperia injects per-pane) |
| 7 | `.serpapi.env` file | workspace root, read by `entry.rs` ~L1181 | **plaintext file in the repo dir** |
| 8 | `SAILFISH_N8_TOKEN` | `trainer_api.rs` bearer gate | host env |
| 9 | **Provider-CLI login state** — opencode `auth.json` (zai/anthropic OAuth), codex OAuth, gemini FileKeychain, claude creds | `~/.nemesis8/home/...` (data-home volume) | files in data home, per-machine |

Rows 1–8 are env-var-shaped → the keychain store covers them directly.
Row 9 is file-shaped agent state → covered by the bootstrap (Phase 4), not by
moving it into the keychain.

## Design (unchanged from #52, confirmed against code)

- **Crate**: `keyring` v3 (wincred / macOS Keychain / libsecret). Service
  `"nemesis8"`, entry key = env-var name.
- **`src/secrets.rs`**: `get / set / delete / list_known()` + `mask()`
  (prefix + last 4: `sk-ant-…A9wA`). Known-secret discovery = the exact
  build_env union (rows 3+4+6+8) so the UI shows a checklist of names that
  *matter on this install*, each set/unset, plus custom names.
- **Injection precedence** in `build_env`: **keychain > host env > `[env]` toml**.
  One lookup helper `secrets::resolve(name)` replaces the bare
  `std::env::var()` calls at the ~L1656-1731 cluster.
- **Headless Linux**: keyring error → fall back to an age-encrypted file store
  (`~/.nemesis8/secrets.age`, passphrase via `N8_SECRETS_KEY` or prompt) with a
  clear one-line warning. Never silently plaintext.

## Phases

### Phase 1 — store + CLI (S)
`Cargo.toml` + `src/secrets.rs` + `cli.rs`/`main.rs`:
```
n8 secrets set <NAME>       # hidden prompt (no value in argv/history)
n8 secrets list             # masked, with source column: keychain/env/toml/—
n8 secrets rm <NAME>
```
`list` prints the discovery checklist (known names from providers/registry/
integrations) even when unset — that alone answers "what does this box need."
Unit tests: mask(), discovery union, precedence resolution (env-var fakes).

### Phase 2 — launch injection (S)
`docker.rs::build_env`: route rows 1–4, 6, 8 through `secrets::resolve`
(keychain-first). No behavior change when keychain is empty — acceptance says
existing env/toml paths keep working. Also: `trainer_api.rs` reads
`SAILFISH_N8_TOKEN` via the same resolve.

### Phase 3 — control-room Secrets screen (M)
Masked list (name · masked value · source · used-by), set/update/clear with
hidden input. Placement: the System view tab (#41) if landed, else a
standalone screen off the home menu like Tools. Never renders full values;
clipboard-paste only for input.

### Phase 4 — provider-login inventory + machine bootstrap (M) — NEW (the laptop incident)
Row 9. Two parts:
- **Inventory**: a data-driven manifest of known login-state files, per
  provider TOML (`[provider.auth_state] files = ["opencode:.local/share/opencode/auth.json", …]`
  or a small hardcoded map to start): opencode auth.json, codex auth, gemini
  keychain files, claude creds. The Secrets screen/`n8 secrets list` shows a
  "provider logins" section: present/absent per file — making the laptop
  failure a one-glance diagnosis.
- **Bootstrap**: `n8 secrets export [--out bundle.age]` — age-encrypted bundle
  of keychain entries + login-state files; `n8 secrets import bundle.age` on
  the new box (writes keychain entries + drops files into the data home).
  Passphrase prompted; bundle is safe to move over Syncthing/USB. This is the
  new-machine one-liner.

### Phase 5 — cleanups (S)
- Deprecate `.serpapi.env` (entry warning: "move to n8 secrets"), remove a
  release later.
- Docs: README secrets section; PROVIDER-TESTING.md note (auth-dependent
  providers need Phase-4 bootstrap on fresh boxes).
- Optional: `n8 doctor` flags plaintext keys found in `[env]` toml with a
  migrate hint (`n8 secrets set X` + delete the toml line).

## Acceptance (from #52 + additions)
- `n8 secrets set XAI_API_KEY` → keychain; `list` masked; launched container
  receives it with **no host env and no plaintext toml**.
- Windows + macOS native; keyring-less Linux degrades to the age file with a
  clear message.
- Existing env/toml flows unchanged when the store is empty.
- **Laptop scenario**: `n8 secrets export` on box A + `import` on box B →
  opencode shows glm-5.2 on B with zero manual file spelunking.

## Out of scope (explicitly)
- **Scoping/brokering** (short-lived, per-tool tokens instead of raw keys in
  every container) — step 2, the mcp-planes-and-secrets plan; this ticket is
  the host-side store only.
- Rotating/expiring secrets; CI secrets (Azure signing lives in repo settings).
- In-container encryption (the agent must read the value; the win is at-rest
  on the host).

## Touchpoints
`Cargo.toml` (keyring, age), **new** `src/secrets.rs`, `src/docker.rs`
(build_env), `src/trainer_api.rs` (bearer), `src/cli.rs` + `src/main.rs`
(subcommand), `src/controlroom.rs` (screen), `providers/*.toml`
(auth_state manifests), docs.
