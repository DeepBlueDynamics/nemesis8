#!/usr/bin/env bash
# nemesis8 build — one script, two audiences.
#
#   Manual:   ./scripts/build.sh
#     Gates (build + tests + mcp harness), then opens `n8 build`'s
#     interactive checkbox picker for the agent image.
#
#   Agentic:  ./scripts/build.sh --agent [n8-build flags…]
#     Non-interactive end to end: gates fail loudly with nonzero exit,
#     image layers come from flags (e.g. --rust --native), no picker,
#     and stdout ends with a single machine-parseable JSON summary.
#     Logs/progress go to stderr — stdout stays clean for parsing.
#
#   Skip the image step entirely (host binary + gates only):
#     ./scripts/build.sh --host-only        (works with or without --agent)
#
# Examples:
#   ./scripts/build.sh                        # human: gate, then pick layers
#   ./scripts/build.sh --agent --rust         # agent: gate + image with cargo
#   ./scripts/build.sh --agent --host-only    # agent: gate only, no image
set -euo pipefail
cd "$(dirname "$0")/.."

AGENT=0
HOST_ONLY=0
IMAGE_FLAGS=()
for arg in "$@"; do
  case "$arg" in
    --agent) AGENT=1 ;;
    --host-only) HOST_ONLY=1 ;;
    *) IMAGE_FLAGS+=("$arg") ;;
  esac
done

log() { echo "[build] $*" >&2; }

BIN=target/release/nemesis8
if [ -x "$BIN" ]; then
    :
elif [ -f "${BIN}.exe" ]; then
    BIN="${BIN}.exe"
fi

# ── Gate 1: host binary ──────────────────────────────────────────
log "cargo build --release"
cargo build --release 1>&2
if [ -x "$BIN" ]; then
    :
elif [ -f "${BIN}.exe" ]; then
    BIN="${BIN}.exe"
fi

# ── Gate 2: tests ────────────────────────────────────────────────
log "cargo test"
cargo test --lib 1>&2
cargo test --bin nemesis8 1>&2

# ── Gate 3: provider MCP configs ─────────────────────────────────
log "n8 mcp test"
"$BIN" mcp test 1>&2

# ── Gate 4: gateway smoke ────────────────────────────────────────
# Boot a throwaway gateway on a scratch port, prove /health answers and the
# /mcp endpoint completes an initialize handshake, then kill it. Catches
# route/panic regressions the unit tests can't (the gateway wires axum,
# telemetry, the event store, and the embedded fleet dashboard together).
SMOKE_PORT=9899
log "gateway smoke on :$SMOKE_PORT"
NEMESIS8_STUB_RUNTIME=1 "$BIN" --port "$SMOKE_PORT" serve 1>&2 2>/dev/null &
GW_PID=$!
trap 'kill "$GW_PID" 2>/dev/null || true' EXIT
ok=0
for _ in $(seq 1 20); do
  if curl -sf -m 1 "http://127.0.0.1:$SMOKE_PORT/health" >/dev/null 2>&1; then ok=1; break; fi
  sleep 0.25
done
[ "$ok" -eq 1 ] || { log "FAIL: gateway /health never answered"; exit 1; }
curl -sf -m 3 -X POST "http://127.0.0.1:$SMOKE_PORT/mcp" \
  -H "Content-Type: application/json" -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"build-smoke","version":"0"}}}' \
  | grep -q '"serverInfo"' || { log "FAIL: /mcp initialize"; exit 1; }
kill "$GW_PID" 2>/dev/null || true
trap - EXIT
log "gateway smoke: pass"

# ── Image ────────────────────────────────────────────────────────
IMAGE_BUILT=false
if [ "$HOST_ONLY" -eq 0 ]; then
  if [ "$AGENT" -eq 1 ]; then
    # Flags only; `n8 build` skips its picker when stdin isn't a TTY or any
    # layer flag is present. </dev/null guarantees non-interactive.
    log "n8 build ${IMAGE_FLAGS[*]:-<default layers>}"
    "$BIN" build ${IMAGE_FLAGS[@]+"${IMAGE_FLAGS[@]}"} </dev/null 1>&2
  else
    log "n8 build (interactive picker)"
    "$BIN" build ${IMAGE_FLAGS[@]+"${IMAGE_FLAGS[@]}"} 1>&2
  fi
  IMAGE_BUILT=true
fi

VERSION="$("$BIN" -V | awk '{print $2}')"
COMMIT="$(git rev-parse --short HEAD)"

if [ "$AGENT" -eq 1 ]; then
  printf '{"version":"%s","commit":"%s","binary":"%s","image_built":%s,"gates":"pass"}\n' \
    "$VERSION" "$COMMIT" "$BIN" "$IMAGE_BUILT"
else
  log "done — nemesis8 $VERSION ($COMMIT), image_built=$IMAGE_BUILT"
fi
