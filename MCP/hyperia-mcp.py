#!/usr/bin/env python3
"""MCP: hyperia

Controls a running Hyperia terminal emulator over HTTP.
Connects to the Hyperia bridge API — no sidecar binary needed.

Environment:
    HYPERIA_URL: Hyperia bridge URL (default http://host.docker.internal:9800)
"""

import os
import json
import urllib.request
import urllib.error
from typing import Optional

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("hyperia")

BASE_URL = os.environ.get("HYPERIA_URL", "http://host.docker.internal:9800")


def _get(path: str) -> str:
    req = urllib.request.Request(f"{BASE_URL}{path}")
    try:
        with urllib.request.urlopen(req, timeout=5) as resp:
            return resp.read().decode()
    except Exception as e:
        return json.dumps({"error": str(e)})


def _post_json(path: str, body: dict) -> str:
    data = json.dumps(body).encode()
    req = urllib.request.Request(f"{BASE_URL}{path}", data=data, method="POST")
    req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return resp.read().decode()
    except Exception as e:
        return json.dumps({"error": str(e)})


def _post_text(path: str, text: str) -> str:
    data = text.encode()
    req = urllib.request.Request(f"{BASE_URL}{path}", data=data, method="POST")
    req.add_header("Content-Type", "text/plain")
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return resp.read().decode()
    except Exception as e:
        return json.dumps({"error": str(e)})


def _resolve_pane(window: Optional[int] = None, tab: Optional[str] = None, pane: Optional[str] = None) -> str:
    """Resolve a pane ID from window/tab/pane selectors."""
    status = json.loads(_get("/api/status"))
    if "error" in status:
        return "0"

    panes = status.get("panes", [])
    if not panes:
        return "0"

    # Filter by window
    # The status API returns flat pane list — use tab name to filter
    if tab:
        for p in panes:
            if p.get("tabName", "") == tab:
                if pane and p.get("splitLabel", "") != pane:
                    continue
                return str(p.get("id", 0))

    if pane:
        for p in panes:
            if p.get("splitLabel", "") == pane:
                return str(p.get("id", 0))

    # Default to first pane
    return str(panes[0].get("id", 0))


@mcp.tool()
def terminal_status() -> str:
    """List all open windows, tabs, and panes."""
    return _get("/api/status")


@mcp.tool()
def terminal_screen(
    window: Optional[int] = None,
    tab: Optional[str] = None,
    pane: Optional[str] = None,
) -> str:
    """Read the current screen content of a terminal pane."""
    pane_id = _resolve_pane(window, tab, pane)
    return _get(f"/api/screen/{pane_id}")


@mcp.tool()
def terminal_keys(
    keys: str,
    window: Optional[int] = None,
    tab: Optional[str] = None,
    pane: Optional[str] = None,
) -> str:
    """Type keystrokes into a terminal pane. Use \\n for Enter, \\t for Tab."""
    pane_id = _resolve_pane(window, tab, pane)
    return _post_text(f"/api/type/{pane_id}", keys)


@mcp.tool()
def terminal_run(
    command: str,
    window: Optional[int] = None,
    tab: Optional[str] = None,
    pane: Optional[str] = None,
    wait_ms: int = 2000,
) -> str:
    """Run a shell command in a terminal pane. Sends command + Enter, waits, returns screen."""
    pane_id = _resolve_pane(window, tab, pane)
    _post_text(f"/api/type/{pane_id}", command)
    import time
    time.sleep(wait_ms / 1000.0)
    return _get(f"/api/screen/{pane_id}")


@mcp.tool()
def terminal_split(
    direction: str = "horizontal",
    window: Optional[int] = None,
    tab: Optional[str] = None,
    pane: Optional[str] = None,
) -> str:
    """Split a terminal pane. Direction: horizontal or vertical."""
    pane_id = _resolve_pane(window, tab, pane)
    return _post_json("/api/pane/split", {"paneId": pane_id, "direction": direction})


@mcp.tool()
def terminal_focus(
    window: Optional[int] = None,
    tab: Optional[str] = None,
    pane: Optional[str] = None,
) -> str:
    """Focus a specific terminal pane."""
    pane_id = _resolve_pane(window, tab, pane)
    return _post_json("/api/pane/focus", {"paneId": pane_id})


@mcp.tool()
def terminal_close() -> str:
    """Close the currently focused pane."""
    return _post_json("/api/pane/close", {})


@mcp.tool()
def terminal_new_tab(command: Optional[str] = None) -> str:
    """Open a new tab. Optionally run a startup command."""
    body = {}
    if command:
        body["command"] = command
    return _post_json("/api/pane/new", body)


@mcp.tool()
def terminal_rename(name: str) -> str:
    """Rename the current tab."""
    return _post_json("/api/pane/rename", {"name": name})


@mcp.tool()
def voice_status() -> str:
    """Get voice input status."""
    return _get("/api/voice/status")


@mcp.tool()
def voice_start() -> str:
    """Start voice input."""
    return _post_json("/api/voice/start", {})


@mcp.tool()
def voice_stop() -> str:
    """Stop voice input."""
    return _post_json("/api/voice/stop", {})


@mcp.tool()
def voice_toggle() -> str:
    """Toggle voice input."""
    return _post_json("/api/voice/toggle", {})


@mcp.tool()
def sidecar_logs() -> str:
    """Get sidecar logs."""
    return _get("/api/logs")


if __name__ == "__main__":
    mcp.run()
