# Nemesis8 Setup Log — nemesis server, 2026-03-19

## Environment
- OS: Linux 6.17.0-19-generic, x86_64
- Docker: v29.3.0
- GPU: GTX 1080 8GB
- RAM: 32GB
- Rust: installed via cargo

## Steps Taken

### 1. Build (success)
```
cargo build --release   # already built, 0.33s
nemisis8 build          # Docker image built from cache, tagged nemisis8:latest
nemisis8 doctor         # Docker v29.3.0 detected, platform linux/x86_64
```

### 2. Config Fix
- `.codex-container.toml` had Windows paths (`C:/Users/kordl/...`) from Hyperia desktop
- Updated all `[[mounts]]` to use `/home/kord/Code/Gnosis/nemesis8/` prefix
- Cleared stale `last_session_file` Windows path

### 3. Codex Provider Run (FAILED)
```
nemisis8 run "echo hello"
```
- Websocket connection to `wss://api.openai.com/v1/responses` returns **500 Internal Server Error** (5 retries)
- Falls back to HTTPS `https://api.openai.com/v1/responses`
- HTTPS returns **401 Unauthorized: Missing bearer or basic authentication**
- `OPENAI_API_KEY` IS being forwarded to container (verified with `docker run --rm env`)
- Root cause: Codex CLI may require `codex login` OAuth flow rather than raw API key, OR the API key doesn't have `/v1/responses` endpoint access (different from standard chat completions)

### 4. Gemini Provider Run (FAILED)
```
nemisis8 --provider gemini run "echo hello"
```
- MCP tools installed (19 tools)
- Gemini config written
- **Crash:** `TypeError: Cannot read properties of undefined (reading '/workspace')`
  - Location: `projectRegistry.js:108` → `getShortId()`
  - Root cause: `projects.json` is initialized as `{}` (empty object). Gemini CLI expects the workspace path to be pre-registered in this file. The `trustedFolders.json` is written correctly, but `projects.json` needs a proper entry for `/workspace`.

### 5. API Key Verification
- All three keys present in host environment: `OPENAI_API_KEY`, `GEMINI_API_KEY`, `ANTHROPIC_API_KEY`
- Keys reach container when passed via `-e` flag directly
- Bollard API also passes them (verified in `build_env()` source)

## Suggestions for Dev Agent (Claude)

### Critical Fixes

1. **Gemini projects.json initialization** (`entry.rs`, `write_gemini_config()`):
   The empty `{}` crashes Gemini CLI. Fix: write a proper projects.json entry:
   ```json
   {"/workspace": {"shortId": "workspace", "name": "workspace"}}
   ```
   Or better: run `gemini --init` or parse what Gemini CLI expects and generate it.

2. **Codex auth fallback** (`entry.rs`, `resolve_api_key()`):
   When `OPENAI_API_KEY` is set but Codex CLI still fails auth, the entry point should:
   - Try writing the key to Codex's config file (`~/.codex/config.toml` with `api_key = "..."`)
   - OR detect if `codex login` is needed and provide guidance
   - OR support running Codex with `--api-key` flag if available

3. **Platform-aware config paths** (`.codex-container.toml`):
   The `[[mounts]]` section has hardcoded Windows paths. Add:
   - A `nemisis8 init` enhancement that detects platform and generates correct paths
   - Or support env var expansion in mount paths (`$HOME/Code/...`)

### Quality of Life

4. **bubblewrap warning**: Container prints "could not find system bubblewrap at /usr/bin/bwrap". Either install it in the Dockerfile or suppress the warning.

5. **Error reporting**: When the inner CLI (codex/gemini) fails, `nemisis8` only says "wait error: Docker container wait error". Capture and forward the actual exit code and last stderr lines.

6. **Provider auto-detection**: If `OPENAI_API_KEY` is missing but `GEMINI_API_KEY` is present, auto-switch provider rather than defaulting to codex and failing.

7. **Health check on API before launch**: Before spawning the container, do a quick HTTP check against the provider's API to verify the key works. Fail fast with a clear message instead of watching 5 retries timeout.
