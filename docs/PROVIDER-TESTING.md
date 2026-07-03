# Provider config testing — the "did I do everything?" runbook

Every provider (codex, antigravity, opencode, claude, grok, sakana…) loads an
MCP config that **nemesis8-entry generates at container launch**. Each has its
own schema dialect, and a config one provider loads happily can **brick
another's startup** (real examples below). This doc is the checklist for
changing anything in that pipeline — and the harness that catches breakage
before an agent does.

## The harness: `n8 mcp test`

```bash
n8 mcp test                    # every provider, this workspace's tools
n8 mcp test --provider opencode
```

For each provider it runs the REAL pipeline — the same lib functions
`nemesis8-entry` executes in the container:

1. **tools** — this workspace's `mcp_tools` (⊕ global), stage-1 resolved
   (URL / `.py` present / registry name)
2. **capability adaptation** — `config::adapt_tools_http_unsupported`
   (antigravity: native-HTTP servers → their `<name>-mcp.py` stdio shims)
3. **generation** — `generate_codex_config_disabled` /
   `generate_json_config_styled_disabled` per the provider's format+style
4. **hyperia injection** — `config::inject_hyperia_server` under the entry's
   exact gating (never for `http_mcp_unsupported`, never when already wired)
5. **validation** — `config::validate_provider_config` against the provider's
   schema expectations → **PASS/FAIL with the exact problem**

Exit code is non-zero on any FAIL, so it belongs in CI and in your fingers.

## When to run it

- After editing **`mcp-servers/*.toml`** (registry defs) or **`MCP/*.py`**
- After editing **`providers/*.toml`** (esp. `mcp_http_style`, `mcp_key`,
  `http_mcp_unsupported`, `format`)
- After touching **`src/config.rs` generators / inject / adapt** or
  **`src/entry.rs` write_provider_config**
- After changing a workspace's tools in the TUI picker, if an agent then
  misbehaves — run it in that workspace

## The schema dialects (why this keeps biting)

| provider | format | style | MCP entry must look like |
|---|---|---|---|
| codex/sakana/grok | toml | — | `[mcp_servers.X]` `command`/`args`/`env`, HTTP: `type="http"`+`url`+`http_headers` |
| claude | json | claude | `{"type":"http","url":…}` or command |
| gemini-family | json | gemini | `{"httpUrl":…}` or command |
| **opencode** | json | opencode | **strict**: `{"type":"local"\|"remote", …, "enabled":bool}` — anything else fails its WHOLE config load |
| **antigravity** | json | gemini | **no HTTP at all** (`http_mcp_unsupported=true`): only `command` entries; any `httpUrl` → "no connector can handle spec" |

## Failure signatures we've actually shipped (now regression-tested)

- **opencode: "Configuration is invalid … Missing key mcp.hyperia.enabled"**
  → something wrote a non-opencode shape into `opencode.json`. The hyperia
  auto-inject did exactly this (gemini `httpUrl`) until it learned the
  opencode branch. Test: `gemini_httpurl_shape_fails_opencode_validation`.
- **antigravity: "no connector can handle spec" on a tool** → an `httpUrl`
  entry leaked in. The auto-inject is now gated off for
  `http_mcp_unsupported` providers. Test:
  `validate_flags_httpurl_for_http_unsupported_provider`.
- **antigravity: a selected server silently missing** (e.g. `meridian`) →
  native-HTTP servers are DROPPED for antigravity; the fix is shim
  substitution (`meridian` → `meridian-mcp.py`) which requires the shim to be
  **in the image** (`MCP/meridian-mcp.py` + `n8 build`). Test:
  `adapt_substitutes_stdio_shims_for_http_servers`.

## The full "did I do everything?" checklist for a new MCP integration

Adding a sidecar/server `foo` (the hyperia/meridian pattern):

1. **Registry def** `mcp-servers/foo.toml` — native HTTP entry (`url`,
   `transport`, `bearer_token_env` if auth planned).
2. **Stdio shim** `MCP/foo-mcp.py` — the dynamic proxy pattern (copy
   `hyperia-mcp.py`); named `<registry-name>-mcp.py` so antigravity's shim
   substitution finds it.
3. **Env forwarding** — add `FOO_URL` (+ token var) to
   `config::MCP_FORWARD_ENV` so the shim's env reaches the container config.
4. **Host binary rebuild** — registry defs are EMBEDDED in the host binary
   (`build.rs`); rebuild + reinstall or the picker won't show `foo`.
5. **Image rebuild** — the shim + registry TOML bake into the image
   (`/opt/mcp-source`, `/opt/defaults/mcp`) via **`n8 build`** — run it from
   the repo dir if commits aren't pushed. Until this, containers don't have
   the shim and antigravity gets nothing.
6. **`n8 mcp test`** in a workspace with `foo` selected — every provider
   PASSes, antigravity shows the substitution note.
7. **Live smoke** — launch the pickiest two: `opencode` (strict schema) and
   `antigravity` (no HTTP), confirm the tool lists load.

Which release channel ships what (see RELEASING.md): host binary = Channel A;
`MCP/`, `mcp-servers/`, `providers/`, `src/entry.rs` = Channel C (`n8 build`).

## Where the code lives

- `src/config.rs` — generators, `adapt_tools_http_unsupported`,
  `inject_hyperia_server`, `validate_provider_config`, `MCP_FORWARD_ENV`,
  regression tests (`cargo test --lib config`)
- `src/entry.rs::write_provider_config` — the in-container orchestration
  (thin: calls the lib fns above)
- `src/main.rs::run_mcp_config_test` — the `n8 mcp test` driver
