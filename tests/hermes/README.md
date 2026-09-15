# Hermes restoration checks

## Build and launch

Build the current source with the default provider set (including Hermes):

```sh
docker build --build-arg BINS_MODE=source -t n8-hermes:test .
n8 --tag n8-hermes:test --provider hermes --model ollama/gemma4:12b interactive
```

Use a model installed on your Ollama host. The model above was used in the
2026-09-15 smoke test; it is not downloaded by this integration.
The separate image tag lets existing sessions continue using their current image.

For the original installer/base compatibility check:

```sh
docker build -f tests/hermes/Dockerfile.install-test -t n8-hermes-install:test .
```

That test pins the inspected base digest. The production Dockerfile continues
using its existing base selection and upstream provider installer policy.

## Automated checks

```sh
cargo test --lib --bin nemesis8-entry -- --test-threads=1
python3 -m unittest discover -s integrations/hermes/nemesis8/tests
```

The Rust checks cover YAML preservation and invalid-input failure, MCP schemas,
plugin copy and refresh at startup, the Nous alias/default install, and Hermes
SQLite timestamps/arguments while preserving OpenCode timestamps.

The Python checks cover the native plugin registration contract, gateway route
and payload construction, optional bearer authentication, validation, and
offline/HTTP error behavior. Spawn/stop tests use mocked HTTP; they do not
create or terminate real agents.

## Live image smoke

With the repository at the current directory and Ollama plus `n8 serve` on the
host, run from a POSIX shell:

```sh
docker run --rm \
  -e HERMES_SMOKE_MODEL=ollama/gemma4:12b \
  --mount "type=bind,source=$(pwd),target=/workspace/nemesis8,readonly" \
  n8-hermes:test /usr/local/lib/hermes-agent/venv/bin/python \
  /workspace/nemesis8/tests/hermes/runtime_smoke.py
```

PowerShell users can replace `$(pwd)` with an absolute host path and run the
command on one line. Each test uses a fresh container home. It does not mount
the user's persistent Hermes configuration or credentials.

The test launches the real n8 entry binary, checks the generated YAML/SOUL and
copied plugin, loads all five native tool definitions through Hermes, and
verifies a model-issued gateway-status call and its response in SQLite.
It recognizes both direct calls and Hermes's `tool_call` wrapper.
A reachable gateway is required for this live test.

The bundled plugin can also be checked without inference:

```sh
docker run --rm n8-hermes:test hermes plugins doctor --ci \
  /opt/defaults/integrations/hermes/nemesis8
```

## Verified result (2026-09-15)

These image checks ran on n8 0.24.2. The source was subsequently bumped through
`scripts/bump.sh patch` to **0.24.3** for local builds. Rebuild the image from the
0.24.3 commit to include that version; the recorded image below remains 0.24.2.

- Final image: `n8-hermes:test`, image ID
  `sha256:644f8f341106d3f24fc099e54d8faf6729f2d270539fcf9d45fdc4b2a9c1a459`.
- Full default-provider source build: exit 0.
- Hermes: v0.21.3, upstream revision reported by CLI: `d6d9e67f`.
- Rust: 279 library tests passed, 2 ignored; 1 entry startup test passed.
- Plugin: 19 Python unit tests passed.
- `n8 mcp test`: all MCP-capable provider configurations passed.
- Actual Hermes plugin doctor: discovery, import and five tool registrations passed.
- Finished-image live smoke (no implementation overlays): exit 0;
  `NATIVE_FLEET_SKILL_VERIFIED`, `MODEL_NATIVE_TOOL_CALL_VERIFIED`,
  and `HERMES_RUNTIME_SMOKE_PASS`.
- Post-install version checks passed for Node 24.21.0, npm 11.19.0,
  Codex, Claude, Antigravity, Grok, Pi, and OpenCode.
- The live model called the tool but did not consistently follow the requested
  final-response wording. The smoke test checks the saved invocation and actual
  gateway response, rather than relying on a claimed action in model text.

## Observations (2026-09-15)

- The historical removal commit gated installation; it did not delete the adapter.
- The current installer successfully falls back to Node 24 on the bookworm base
  after its Node 26 executable fails. The old default-build blocker did not
  reproduce with the tested installer.
- A local `gemma4:e2b` simple-response test passed, but its tool-instruction test
  did not. `gemma4:12b` issued the native gateway-status call successfully.
  Tool use remains dependent on the selected model.
- Hermes writes fractional Unix seconds and OpenAI-style nested function
  arguments to SQLite. The restored reader handles those values.
- `--skip-browser` remains in the installer arguments; Playwright browser
  automation is not included by this change.
- User YAML values and explicit plugin disablement are preserved. YAML comments
  and formatting are rewritten when n8 refreshes its MCP configuration.
