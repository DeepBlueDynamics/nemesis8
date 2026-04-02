#!/usr/bin/env python3
"""MCP: hyperia

Controls a running Hyperia terminal emulator over HTTP.
Connects to the Hyperia bridge API — no sidecar binary needed.

IMPORTANT: Call terminal_status first to see available windows, tabs, and panes.
Use tab NAME (e.g. "Furious Capybara") and pane LABEL (e.g. "a", "b") — NOT UUIDs.

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


def _resolve_pane(tab: Optional[str] = None, pane: Optional[str] = None) -> str:
    """Resolve a numeric pane ID from tab name + pane label.

    The status API returns: {windows: [{tabs: [{name, panes: [{id, label, ...}]}]}]}
    We walk that tree to find the matching pane.
    """
    try:
        status = json.loads(_get("/api/status"))
    except Exception:
        return "0"

    if "error" in status:
        return "0"

    all_panes = []
    for window in status.get("windows", []):
        for t in window.get("tabs", []):
            tab_name = t.get("name", "")
            for p in t.get("panes", []):
                p["_tabName"] = tab_name
                all_panes.append(p)

    if not all_panes:
        return "0"

    # Match by tab name
    if tab:
        candidates = [p for p in all_panes if p["_tabName"] == tab]
        if candidates:
            # Match by pane label within tab
            if pane:
                for p in candidates:
                    if p.get("label", "") == pane:
                        return str(p.get("id", 0))
            return str(candidates[0].get("id", 0))

    # Match by pane label across all tabs
    if pane:
        for p in all_panes:
            if p.get("label", "") == pane:
                return str(p.get("id", 0))

    # Default: first pane of the active tab
    for window in status.get("windows", []):
        for t in window.get("tabs", []):
            if t.get("active"):
                panes = t.get("panes", [])
                if panes:
                    return str(panes[0].get("id", 0))

    return str(all_panes[0].get("id", 0))


@mcp.tool()
def terminal_status() -> str:
    """List all Hyperia windows, tabs, and panes. Call this FIRST to discover
    tab names and pane labels before using other tools.

    Returns JSON: {windows: [{tabs: [{name: "Tab Name", panes: [{id, label, ...}]}]}]}

    Use the tab 'name' (e.g. "Furious Capybara") and pane 'label' (e.g. "a", "b")
    when calling other tools. Do NOT use paneId UUIDs."""
    return _get("/api/status")


@mcp.tool()
def terminal_screen(
    tab: Optional[str] = None,
    pane: Optional[str] = None,
) -> str:
    """Read the visible text content of a terminal pane.

    Args:
        tab: Tab name from terminal_status (e.g. "Furious Capybara"). Omit for active tab.
        pane: Pane label (e.g. "a", "b"). Omit for first pane in tab."""
    pane_id = _resolve_pane(tab, pane)
    return _get(f"/api/screen/{pane_id}")


@mcp.tool()
def terminal_keys(
    keys: str,
    tab: Optional[str] = None,
    pane: Optional[str] = None,
) -> str:
    """Type keystrokes into a terminal pane. Use \\n for Enter, \\t for Tab.

    Args:
        keys: Keystrokes to send. Use \\n for Enter, \\t for Tab, \\x03 for Ctrl+C.
        tab: Tab name (e.g. "Furious Capybara"). Omit for active tab.
        pane: Pane label (e.g. "a", "b"). Omit for first pane."""
    pane_id = _resolve_pane(tab, pane)
    return _post_text(f"/api/type/{pane_id}", keys)


@mcp.tool()
def terminal_run(
    command: str,
    tab: Optional[str] = None,
    pane: Optional[str] = None,
    wait_ms: int = 2000,
) -> str:
    """Run a shell command: types it, sends Enter, waits, returns screen content.

    Args:
        command: Shell command to execute.
        tab: Tab name (e.g. "Furious Capybara"). Omit for active tab.
        pane: Pane label (e.g. "a", "b"). Omit for first pane.
        wait_ms: Milliseconds to wait before reading screen (default 2000)."""
    pane_id = _resolve_pane(tab, pane)
    _post_text(f"/api/type/{pane_id}", command + "\n")
    import time
    time.sleep(wait_ms / 1000.0)
    return _get(f"/api/screen/{pane_id}")


@mcp.tool()
def terminal_split(
    direction: str = "horizontal",
    tab: Optional[str] = None,
    pane: Optional[str] = None,
) -> str:
    """Split a terminal pane horizontally or vertically.

    Args:
        direction: "horizontal" (top/bottom) or "vertical" (left/right).
        tab: Tab name. Omit for active tab.
        pane: Pane label. Omit for first pane."""
    pane_id = _resolve_pane(tab, pane)
    return _post_json("/api/pane/split", {"paneId": pane_id, "direction": direction})


@mcp.tool()
def terminal_focus(
    tab: Optional[str] = None,
    pane: Optional[str] = None,
) -> str:
    """Focus a specific terminal pane.

    Args:
        tab: Tab name (e.g. "Furious Capybara"). Omit for active tab.
        pane: Pane label (e.g. "a", "b"). Omit for first pane."""
    pane_id = _resolve_pane(tab, pane)
    return _post_json("/api/pane/focus", {"paneId": pane_id})


@mcp.tool()
def terminal_close() -> str:
    """Close the currently focused pane."""
    return _post_json("/api/pane/close", {})


@mcp.tool()
def terminal_new_tab(command: Optional[str] = None) -> str:
    """Open a new tab. Optionally run a startup command.

    Args:
        command: Shell command to run in the new tab (optional)."""
    body = {}
    if command:
        body["command"] = command
    return _post_json("/api/pane/new", body)


@mcp.tool()
def terminal_rename(name: str) -> str:
    """Rename the current tab.

    Args:
        name: New name for the tab."""
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
    """Toggle voice input on/off."""
    return _post_json("/api/voice/toggle", {})


@mcp.tool()
def sidecar_logs() -> str:
    """Get Hyperia sidecar logs."""
    return _get("/api/logs")


if __name__ == "__main__":
    mcp.run()
