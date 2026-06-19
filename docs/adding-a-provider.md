# Adding a provider

Every AI CLI nemesis8 can drive — `codex`, `gemini`, `claude`, `antigravity`,
`grok`, `ollama`, `pi` — is described entirely by **one TOML file** in
[`providers/`](../providers). There is no per-provider Rust: `entry.rs` reads the
spec and generates the CLI's config, wires sessions, injects MCP tools, handles
danger mode, and runs login. Adding a provider means writing a TOML file and
adding one word to the Dockerfile.

> If you find yourself wanting to write `if provider == "x"` in Rust, stop — add
> a field to the spec instead so every provider benefits. That's the whole point.

## TL;DR — three steps

1. **Write `providers/<name>.toml`** (schema below; copy the closest existing
   provider).
2. **Add `<name>` to `INSTALL_PROVIDERS`** in the [`Dockerfile`](../Dockerfile)
   (line ~54) so the CLI gets installed into the image.
3. **Rebuild and run:** `n8 build` then `n8 --provider <name> interactive`.

The TOML is embedded into the binary at build time by `build.rs`, and can be
overridden at runtime by dropping a file at `/opt/defaults/providers/<name>.toml`
or `~/.nemesis8/providers/<name>.toml` (no rebuild needed for overrides).

## How install works

`[provider.install]` is consumed by [`scripts/install-providers.py`](../scripts/install-providers.py):

```toml
[provider.install]
kind = "npm"                       # "npm", "curl", "host", or "none"
package = "@scope/the-cli"         # for kind = "npm"
ignore_scripts = true              # optional: npm install --ignore-scripts
npm_flags = ["--no-audit"]         # optional: extra npm flags (list or string)
# for kind = "curl":
#   url = "https://…/install.sh"   # piped to sh
#   binary_name = "grok"           # the binary the installer drops on PATH
# for kind = "host":  nothing is installed — the provider reuses another CLI
#   (ollama reuses `codex`) and talks to a service running on the host.
```

`INSTALL_PROVIDERS` (Dockerfile build arg) selects which of these run during the
image build. A provider not in the list still works if its `binary` happens to be
present, but normally you add it to the list.

## Schema reference

All sections except `[provider]` and `[provider.config_dir]` are optional and
default sensibly. Field names map 1:1 to
[`src/provider_def.rs`](../src/provider_def.rs).

### `[provider]` — identity

| Field | Req | Notes |
|---|---|---|
| `name` | ✓ | Canonical name; what `--provider <name>` matches. |
| `aliases` | | Alternate names (e.g. `["google"]` for gemini). |
| `emoji` | | Glyph in the entry banner / TUI lists. |
| `binary` | ✓ | Executable name on `PATH` inside the container. |
| `script` | | Path to a launch script instead of a bare binary. |
| `install_package` | | Informational; the real install is `[provider.install]`. |
| `workspace_flag` | | Flag passed as `<flag> <workspace_root>` for CLIs that otherwise sandbox writes to their session dir (e.g. antigravity's `--add-dir`). Omit if the CLI uses cwd as the workspace. |

### `[provider.config_dir]` — where its config lives + MCP wiring

```toml
[provider.config_dir]
path = ".gemini"            # relative to the data home (~/.nemesis8/home → /opt/nemesis8)
format = "json"             # "json", "toml", or "none" (CLI manages its own config)
filename = "settings.json"
mcp_key = "mcpServers"      # the key under which MCP servers are written
merge = false               # merge MCP table into an existing file instead of overwrite
```

- **`format = "none"`** → nemesis8 writes no config; the CLI handles everything.
- **`mcp_key = ""`** → the provider has **no MCP** (e.g. `pi`). nemesis8 generates
  a config with no servers, skips Hyperia injection, and skips schema-cache
  pruning — all generically. This is the supported way to onboard an MCP-less
  agent; do not special-case it in code.
- **`merge = true`** → only for CLIs that keep their *own* state in the same file
  (grok's `[cli]`/`[marketplace]`). Default `false` regenerates a clean config
  each session, which avoids the CLI persisting a value a future version can't
  parse (the codex `model_availability_nux` breakage).

### `[provider.prompt]` — how a one-shot prompt is passed

```toml
[provider.prompt]
flag = "-p"                       # <bin> -p "prompt"
# or, for subcommand-style CLIs:
exec_subcommand = "exec"          # <bin> exec ...
exec_prompt_flag = "--prompt"
interactive_subcommand = "chat"   # subcommand for interactive mode
```

### `[provider.system_prompt]` — the agent's system prompt

```toml
[provider.system_prompt]
persona = "You are Codex, OpenAI's coding agent."  # one-line identity, prepended
env_var = "CODEX_INSTRUCTIONS"  # deliver via env var (codex/gemini), OR…
write_to_file = "SYSTEM.md"     # …write into the provider's config dir (pi)
```

n8 owns the system prompt. The **body is embedded in the binary** — `prompts/BASE.md`
via `include_str!` (the shared guardrails: use `nuts-files`, edit only `/workspace`,
etc.) — so every provider gets the same baseline with no workspace or image
dependency. `compose_system_prompt` prepends this provider's **`persona`** line to
that base, giving the right identity per agent (`codex` → "You are Codex…",
`pi` → "You are Pi…").

A provider needs **one delivery mechanism** for the composed text:
- **`env_var`** — set it as an env var the CLI reads (codex `CODEX_INSTRUCTIONS`,
  gemini `GEMINI_INSTRUCTIONS`).
- **`write_to_file`** — write it into the provider's config dir under this name
  (pi → `SYSTEM.md`).

(`source_file` is a legacy field — the prompt body comes from the embedded
`prompts/BASE.md`, not a per-workspace file. Edit `prompts/BASE.md` to change the
shared guardrails for all providers.)

### `[provider.danger]` — skip-approvals mode

```toml
[provider.danger]
flag = "--approve"                              # flag added in danger mode
env_vars = ["SOME_YOLO=1"]                      # env vars set in danger mode
config_merge = { defaultProjectTrust = "always" }  # JSON merged into the config in danger mode
```

`config_merge` is merged into the generated config (JSON or TOML) **only when
`--danger` is set**. Pi uses it to write `defaultProjectTrust:"always"` so
non-interactive runs don't stall on a trust prompt.

### `[provider.model]` — model selection

```toml
[provider.model]
flag = "--model"
env_source = "CODEX_DEFAULT_MODEL"   # env var holding the default model id
default = "gpt-5.5"                  # informational
context_window = 200000              # ollama/codex-custom-endpoint: written to config
max_output_tokens = 64000            # ditto
```

### `[provider.api_keys]` — key resolution

```toml
[provider.api_keys]
target = "GEMINI_API_KEY"            # env var the CLI actually reads
chain = ["GEMINI_API_KEY", "GOOGLE_API_KEY"]  # first match wins → target
optional = true                      # don't error if no key (subscription/OAuth/local)
write_to_config = false              # write the key into the config file instead of env
```

Multi-backend agents (like `pi`, which reads `ANTHROPIC_API_KEY`/`OPENAI_API_KEY`/…
directly) typically just set `optional = true` and rely on nemesis8 forwarding the
usual key env vars.

### `[provider.hooks]` — sessions, resume, file sync

```toml
[provider.hooks]
requires_git_init = false
supports_sessions = true
resume_flag = "--resume"             # <bin> --resume <id>; omit → `<bin> resume <id>` (codex-style)
session_dirs = [".codex/sessions"]   # relative to data home; one `*` allowed MID-pattern only
auth_files_sync = ["auth.json"]      # files synced host↔volume so login persists
extra_config_files = ["projects"]    # extra files/dirs to scaffold in the config dir
```

> **`session_dirs` gotcha:** the `*` wildcard is only expanded *mid-pattern*
> (`.gemini/tmp/*/chats`). A **trailing** `*` (`.pi/agent/sessions/*`) is **not**
> expanded and will match nothing. The session scanner already recurses into
> subdirectories, so point at the root (`.pi/agent/sessions`) and let it walk down.

### `[provider.login]` — interactive auth

```toml
[provider.login]
command = "gemini -d auth login"     # shell run for `n8 --provider <name> login`
env_vars = ["OAUTH_CALLBACK_PORT=8766"]
ports = ["8766:8766"]                # ports published during login (OAuth callback)

[provider.login.preflight]           # host-side check before interactive sessions
file = ".gemini/oauth_creds.json"    # relative to host home; if missing…
env_fallback = "GEMINI_API_KEY"      # …and this env isn't set…
hint = "Run 'gemini auth login' first, or set GEMINI_API_KEY."  # …bail with this hint
```

### `[provider.validation]` — flag self-check

```toml
[provider.validation]
flags = ["--model", "-p"]            # flags that must exist for normal runs
danger_flags = ["-y"]                # flags that must exist in danger mode
```

## Worked example: an MCP-less, multi-backend agent (`pi`)

[`providers/pi.toml`](../providers/pi.toml) shows the full pattern for a modern
agent that has **no MCP**, brings **its own** model providers, and gates on
**project trust** instead of a danger flag:

```toml
[provider]
name = "pi"
aliases = ["earendil"]
binary = "pi"

[provider.install]
kind = "npm"
package = "@earendil-works/pi-coding-agent"
ignore_scripts = true

[provider.config_dir]
path = ".pi/agent"
format = "json"
filename = "settings.json"
mcp_key = ""                         # ← no MCP: generic no-op path

[provider.system_prompt]
persona = "You are Pi, the coding agent."   # ← prepended to the embedded BASE.md
write_to_file = "SYSTEM.md"                 # ← composed prompt written to the config dir

[provider.danger]
flag = "--approve"
config_merge = { defaultProjectTrust = "always" }   # ← trust, not a yolo flag

[provider.hooks]
supports_sessions = true
resume_flag = "--session"
session_dirs = [".pi/agent/sessions"]   # ← root, scanner recurses
auth_files_sync = ["auth.json", "trust.json"]
```

## Test checklist

- `cargo test --lib` — `test_all_providers_parse` and the registry test pick up
  your TOML automatically. Add a `test_parse_<name>_provider` for any non-obvious
  fields.
- `n8 build` builds the image with your CLI installed.
- `n8 --provider <name> interactive` launches it; `--danger` applies your
  danger spec.
- `n8 resume` lists and resumes the provider's sessions (verify `session_dirs`).
- `cargo build --release` is clean (the `entry` binary is what reads the spec).
