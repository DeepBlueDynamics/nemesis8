#!/usr/bin/env bash
#
# n8 wipe — clean up stale n8 / antigravity state.
#
#   config  Remove the stale antigravity config that keeps resurrecting retired
#           tools — the orphaned ~/.gemini/config/mcp_config.json (brings back
#           gnosis-*), any rename-aside backups, and the per-server MCP schema
#           cache. SAFE: it's regenerated on the next run, and your logins and
#           conversation history are left untouched.
#
#   image   Delete the built n8 container image(s). DESTRUCTIVE — afterward a
#           full `n8 build` is required (many minutes of compiling + multi-GB
#           downloads). Double-confirmed.
#
# Usage:  bash wipe.sh [config|image]      (no arg = interactive menu)
#
# Self-contained: needs only bash + docker/podman. Copy it to any machine.
set -euo pipefail

N8_HOME="${NEMESIS8_HOME:-$HOME/.nemesis8}"
GEMINI="$N8_HOME/home/.gemini"

detect_runtime() {
  if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    echo docker
  elif command -v podman >/dev/null 2>&1; then
    echo podman
  elif command -v docker >/dev/null 2>&1; then
    echo docker
  else
    echo ""
  fi
}

wipe_config() {
  echo "Scanning for stale antigravity config under:"
  echo "  $GEMINI"
  echo

  local targets=(
    "$GEMINI/config/mcp_config.json"        # the orphan that re-merges gnosis-*
    "$GEMINI/antigravity-cli/mcp"           # per-server schema cache (ghost surface)
  )
  shopt -s nullglob
  for b in "$GEMINI"/config/mcp_config.json.orphaned-* "$GEMINI"/config/mcp_config.json.bak*; do
    targets+=("$b")
  done
  shopt -u nullglob

  local found=()
  for t in "${targets[@]}"; do [ -e "$t" ] && found+=("$t"); done

  if [ ${#found[@]} -eq 0 ]; then
    echo "Nothing stale found — already clean. ✅"
    return 0
  fi

  echo "Will remove:"
  for t in "${found[@]}"; do echo "  - $t"; done
  echo
  echo "(Your antigravity login + conversation history are NOT touched. These"
  echo " files are regenerated cleanly on the next session.)"
  echo
  read -r -p "Remove them? [y/N] " a
  [[ "$a" =~ ^[Yy]$ ]] || { echo "Aborted — nothing removed."; return 1; }

  for t in "${found[@]}"; do rm -rf "$t" && echo "  removed $t"; done
  echo
  echo "Done. Start a FRESH antigravity session — the gnosis ghosts are gone. ✅"
  echo
  echo "Heads-up: the duplicate native 'hyperia' (alongside 'hyperia-mcp') is"
  echo "fixed by the container IMAGE, not this wipe. Run 'n8 build' on this"
  echo "machine to pick up the fix (it also auto-sweeps this orphan from then on)."
}

wipe_image() {
  local rt
  rt="$(detect_runtime)"
  if [ -z "$rt" ]; then echo "No docker/podman runtime found — nothing to do."; return 1; fi

  local images
  # Match the image with OR without a registry/namespace prefix — podman names
  # local images "localhost/nemesis8:latest", and the base is published as
  # "docker.io/deepbluedynamics/nemesis8-base". "^nemesis8" alone missed both, so
  # "wipe image" found nothing and appeared to do nothing.
  images="$("$rt" images --format '{{.Repository}}:{{.Tag}}' 2>/dev/null | grep -E '(^|/)nemesis8(-base)?:' || true)"

  echo
  echo "########################################################################"
  echo "#                                                                      #"
  echo "#   ⚠  DANGER — WIPE n8 CONTAINER IMAGE(S)  ⚠                           #"
  echo "#                                                                      #"
  echo "########################################################################"
  echo
  echo "This will DELETE the following image(s) via '$rt rmi -f':"
  if [ -n "$images" ]; then echo "$images" | sed 's/^/      /'; else echo "      (none found matching nemesis8*)"; fi
  echo
  echo "CONSEQUENCES — read this:"
  echo "  • You CANNOT use n8 again until you rebuild with 'n8 build'."
  echo "  • That rebuild is a FULL build: many minutes of compiling plus"
  echo "    multi-gigabyte base-layer downloads."
  echo "  • This is NOT reversible — there is no undo, only rebuild."
  echo "  • Running agents survive, but no NEW sessions start until rebuilt."
  echo
  if [ -z "$images" ]; then echo "Nothing to remove. ✅"; return 0; fi

  read -r -p "To proceed, type exactly  wipe image  : " c1
  if [ "$c1" != "wipe image" ]; then echo "Aborted — text did not match."; return 1; fi
  echo
  read -r -p "FINAL CONFIRM — really delete the image and force a full rebuild? [y/N] " c2
  [[ "$c2" =~ ^[Yy]$ ]] || { echo "Aborted — nothing removed."; return 1; }

  echo
  echo "$images" | while IFS= read -r img; do
    [ -n "$img" ] && "$rt" rmi -f "$img" && echo "  removed $img"
  done
  echo
  echo "Image(s) wiped. Run 'n8 build' to rebuild. ⏳"
}

menu() {
  echo "n8 wipe — choose an action:"
  echo
  echo "  1) Wipe stale antigravity config   (safe — clears gnosis ghosts)"
  echo "  2) Wipe n8 container image          (DANGEROUS — full rebuild needed)"
  echo "  q) Quit"
  echo
  read -r -p "> " choice
  case "$choice" in
    1) wipe_config ;;
    2) wipe_image ;;
    q|Q|"") echo "Bye." ;;
    *) echo "Unknown choice: $choice"; exit 2 ;;
  esac
}

case "${1:-}" in
  config) wipe_config ;;
  image)  wipe_image ;;
  "")     menu ;;
  *) echo "Usage: $0 [config|image]   (no arg = menu)"; exit 2 ;;
esac
