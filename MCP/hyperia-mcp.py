#!/usr/bin/env python3
"""MCP: hyperia

Controls a running Hyperia terminal emulator over HTTP.
Connects to the Hyperia sidecar API at port 9800.

IMPORTANT: Call terminal_status first to see available windows, tabs, and panes.
Use tab NAME (e.g. "Furious Capybara") and pane LABEL (e.g. "a", "b") — NOT UUIDs.
Use window INDEX (0, 1, 2...) when multiple windows are open.

Environment:
    HYPERIA_URL: Hyperia sidecar URL (default http://localhost:9800)
"""

import json
import os
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Optional

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("hyperia")

BASE_URL = os.environ.get("HYPERIA_URL", "http://localhost:9800").rstrip("/")


# ---------------------------------------------------------------------------
# HTTP helpers
# ---------------------------------------------------------------------------

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


def _pane_qs(
    window: Optional[int] = None,
    tab: Optional[str] = None,
    pane: Optional[str] = None,
) -> str:
    params = []
    if window is not None:
        params.append(f"window={window}")
    if tab:
        params.append(f"tab={urllib.parse.quote(tab)}")
    if pane:
        params.append(f"pane={urllib.parse.quote(pane)}")
    return ("?" + "&".join(params)) if params else ""


# ---------------------------------------------------------------------------
# Terminal tools
# ---------------------------------------------------------------------------

@mcp.tool()
def terminal_status() -> str:
    """List all Hyperia windows, tabs, and panes. Call this FIRST before using
    other tools. Returns a nested hierarchy: windows → tabs → panes.

    Use tab 'name' (e.g. "Furious Capybara") and pane 'label' (e.g. "a", "b")
    when addressing panes. Use window index (0, 1, 2...) for multi-window setups."""
    return _get("/api/status")


@mcp.tool()
def terminal_screen(
    window: Optional[int] = None,
    tab: Optional[str] = None,
    pane: Optional[str] = None,
) -> str:
    """Read the visible text content of a terminal pane.

    Args:
        window: Window index (0, 1, 2...). Omit for focused window.
        tab: Tab name (e.g. "Furious Capybara"). Omit for active tab.
        pane: Pane label (e.g. "a", "b"). Omit for first pane in tab."""
    return _get(f"/api/screen{_pane_qs(window, tab, pane)}")


@mcp.tool()
def terminal_keys(
    keys: str,
    window: Optional[int] = None,
    tab: Optional[str] = None,
    pane: Optional[str] = None,
) -> str:
    """Type keystrokes into a terminal pane. Use \\n for Enter, \\t for Tab,
    \\x03 for Ctrl+C, \\e for Escape.

    Args:
        keys: Keystrokes to send.
        window: Window index. Omit for focused window.
        tab: Tab name. Omit for active tab.
        pane: Pane label. Omit for first pane."""
    return _post_text(f"/api/type{_pane_qs(window, tab, pane)}", keys)


@mcp.tool()
def terminal_run(
    command: str,
    window: Optional[int] = None,
    tab: Optional[str] = None,
    pane: Optional[str] = None,
    wait_ms: int = 2000,
) -> str:
    """Run a shell command: types it, sends Enter, waits, returns screen output.

    Args:
        command: Shell command to execute.
        window: Window index. Omit for focused window.
        tab: Tab name. Omit for active tab.
        pane: Pane label. Omit for first pane.
        wait_ms: Milliseconds to wait before reading screen (default 2000)."""
    qs = _pane_qs(window, tab, pane)
    _post_text(f"/api/type{qs}", command + "\r")
    time.sleep(wait_ms / 1000.0)
    return _get(f"/api/screen{qs}")


@mcp.tool()
def terminal_split(
    direction: str = "vertical",
    window: Optional[int] = None,
) -> str:
    """Split the focused pane. Direction: "vertical" (left/right, default) or
    "horizontal" (top/bottom).

    Args:
        direction: "vertical" or "horizontal".
        window: Window index. Omit for focused window."""
    return _post_json("/api/pane/split", {"direction": direction})


@mcp.tool()
def terminal_focus(
    window: Optional[int] = None,
    tab: Optional[str] = None,
    pane: Optional[str] = None,
) -> str:
    """Focus a specific terminal pane.

    Args:
        window: Window index. Omit for focused window.
        tab: Tab name. Omit for active tab.
        pane: Pane label. Omit for first pane."""
    body: dict = {}
    if window is not None:
        body["window"] = window
    if tab:
        body["tab"] = tab
    if pane:
        body["pane"] = pane
    return _post_json("/api/pane/focus", body)


@mcp.tool()
def terminal_close() -> str:
    """Close the currently focused pane."""
    return _post_json("/api/pane/close", {})


@mcp.tool()
def terminal_new_tab(command: Optional[str] = None) -> str:
    """Open a new terminal tab. Optionally run a startup command in it.

    Args:
        command: Shell command to run after the tab opens (optional)."""
    body: dict = {}
    if command:
        body["command"] = command
    return _post_json("/api/pane/new", body)


@mcp.tool()
def terminal_rename(
    name: str,
    window: Optional[int] = None,
    tab: Optional[str] = None,
) -> str:
    """Rename a tab by its current name or by window/tab address.

    Args:
        name: New display name for the tab.
        window: Window index. Omit for focused window.
        tab: Current tab name to rename. Omit for active tab."""
    body: dict = {"name": name}
    if window is not None:
        body["window"] = window
    if tab:
        body["tab"] = tab
    return _post_json("/api/pane/rename", body)


@mcp.tool()
def terminal_ui_key(
    key_code: str,
    modifiers: Optional[list] = None,
    window: Optional[int] = None,
) -> str:
    """Send a keyboard event to Hyperia's UI layer (bypasses PTY). Use this for
    Escape, Ctrl+C as a shortcut, Alt+arrows, etc. — keys handled by the React
    UI rather than the shell.

    Args:
        key_code: Electron key name: 'Escape', 'Return', 'c', 'Up', 'Down', etc.
        modifiers: Modifier list e.g. ['ctrl'], ['alt'], ['ctrl', 'shift'].
        window: Window index. Omit for focused window."""
    body: dict = {
        "keyCode": key_code,
        "modifiers": modifiers or [],
    }
    if window is not None:
        body["windowId"] = window
    return _post_json("/api/ui/key", body)


@mcp.tool()
def tab_snapshot() -> str:
    """Read all pane screens across all windows and tabs. Returns labeled output
    grouped by window and tab. Use this for a holistic view of everything running."""
    status_raw = _get("/api/status")
    try:
        status = json.loads(status_raw)
    except Exception:
        return status_raw

    output = []
    for win in status.get("windows", []):
        win_id = win.get("id", 0)
        output.append(f"=== Window {win_id} ===")
        for tab in win.get("tabs", []):
            tab_name = tab.get("name", "shell")
            for pane in tab.get("panes", []):
                label = pane.get("label", "")
                cols = pane.get("cols", 0)
                rows = pane.get("rows", 0)
                screen = _get(f"/api/screen{_pane_qs(win_id, tab_name, label or None)}")
                header = (
                    f"--- {tab_name} ({label}) | {cols}x{rows} ---"
                    if label
                    else f"--- {tab_name} | {cols}x{rows} ---"
                )
                output.append(f"{header}\n{screen.strip()}")
    return "\n\n".join(output)


@mcp.tool()
def shell_state() -> str:
    """Analyze all panes and return their state: idle (at prompt), running
    (command in progress), dialog (waiting for input), or empty."""
    status_raw = _get("/api/status")
    try:
        status = json.loads(status_raw)
    except Exception:
        return status_raw

    results = []
    for win in status.get("windows", []):
        win_id = win.get("id", 0)
        for tab in win.get("tabs", []):
            tab_name = tab.get("name", "shell")
            for pane in tab.get("panes", []):
                label = pane.get("label", "")
                screen = _get(f"/api/screen{_pane_qs(win_id, tab_name, label or None)}")
                state = _detect_shell_state(screen)
                results.append({
                    "window": win_id,
                    "tab": tab_name,
                    "pane": label,
                    **state,
                })
    return json.dumps(results, indent=2)


def _detect_shell_state(screen: str) -> dict:
    """Heuristic shell state detection matching the sidecar's logic."""
    lines = [l.rstrip() for l in screen.splitlines() if l.strip()]
    if not lines:
        return {"state": "empty", "detail": "no output", "actionable": None}

    last = lines[-1]

    # Claude Code prompt
    if last.endswith("❯") or (last.endswith(">") and "claude" in screen.lower()):
        return {"state": "idle", "detail": "claude prompt", "actionable": None}

    # Common shell prompts
    for suffix in ("$ ", "# ", "% ", "> ", "❯ ", "❯"):
        if last.endswith(suffix) or last.rstrip().endswith(suffix.rstrip()):
            return {"state": "idle", "detail": "shell prompt", "actionable": None}

    # y/n dialog
    lower = last.lower()
    if lower.endswith("(y/n)") or lower.endswith("(y/n):") or lower.endswith("[y/n]"):
        return {"state": "dialog", "detail": "y/n prompt", "actionable": "y\r"}

    # Press enter
    if "press enter" in lower or "press any key" in lower:
        return {"state": "dialog", "detail": "press enter", "actionable": "\r"}

    # Trust / continue prompts
    if "do you trust" in lower or "do you want to" in lower:
        return {"state": "dialog", "detail": "trust prompt", "actionable": "y\r"}

    return {"state": "running", "detail": last[:80], "actionable": None}


@mcp.tool()
def shell_confirm(
    window: Optional[int] = None,
    tab: Optional[str] = None,
    pane: Optional[str] = None,
) -> str:
    """Auto-handle common shell prompts (y/n, press enter, trust dialogs).
    If window/tab/pane are omitted, scans all panes.

    Args:
        window: Window index. Omit to scan all.
        tab: Tab name. Omit to scan all.
        pane: Pane label. Omit to scan all."""
    if window is not None or tab or pane:
        qs = _pane_qs(window, tab, pane)
        screen = _get(f"/api/screen{qs}")
        state = _detect_shell_state(screen)
        if state["actionable"]:
            _post_text(f"/api/type{qs}", state["actionable"])
            return f"Sent '{state['actionable']!r}' ({state['detail']})"
        return f"No action needed ({state['state']})"

    status_raw = _get("/api/status")
    try:
        status = json.loads(status_raw)
    except Exception:
        return status_raw

    actions = []
    for win in status.get("windows", []):
        win_id = win.get("id", 0)
        for t in win.get("tabs", []):
            tab_name = t.get("name", "shell")
            for p in t.get("panes", []):
                label = p.get("label", "")
                qs = _pane_qs(win_id, tab_name, label or None)
                screen = _get(f"/api/screen{qs}")
                state = _detect_shell_state(screen)
                desc = f"{tab_name} ({label})" if label else tab_name
                if state["actionable"]:
                    _post_text(f"/api/type{qs}", state["actionable"])
                    actions.append(f"{desc}: sent '{state['actionable']!r}' ({state['detail']})")
                else:
                    actions.append(f"{desc}: no action ({state['state']})")
    return "\n".join(actions)


@mcp.tool()
def agent_status(
    connected: bool,
    working: Optional[bool] = None,
    label: Optional[str] = None,
    human_percent: Optional[int] = None,
    window: Optional[int] = None,
    tab: Optional[str] = None,
    pane: Optional[str] = None,
) -> str:
    """Set the agent status indicator on a pane. Shows a colored light in the UI.

    Args:
        connected: True = green (agent connected), False = grey (disconnected).
        working: True = red (actively working).
        label: Short status label (e.g. "Claude working...").
        human_percent: 0-100, how much of the session is human-driven.
        window: Window index. Omit for focused window.
        tab: Tab name. Omit for active tab.
        pane: Pane label. Omit for first pane."""
    body: dict = {"connected": connected}
    if working is not None:
        body["working"] = working
    if label is not None:
        body["label"] = label
    if human_percent is not None:
        body["humanPercent"] = human_percent
    if window is not None:
        body["window"] = window
    if tab:
        body["tab"] = tab
    if pane:
        body["pane"] = pane
    return _post_json("/api/agent/status", body)


@mcp.tool()
def auto_describe(
    window: Optional[int] = None,
    tab: Optional[str] = None,
    pane: Optional[str] = None,
) -> str:
    """Auto-describe a pane's content using local Ollama (llama3.2). Generates
    a short description and stores it on the tab.

    Args:
        window: Window index. Omit for focused window.
        tab: Tab name. Omit for active tab.
        pane: Pane label. Omit for first pane."""
    return _post_text(f"/api/pane/describe{_pane_qs(window, tab, pane)}", "")


# ---------------------------------------------------------------------------
# Telemetry tools
# ---------------------------------------------------------------------------

@mcp.tool()
def telemetry_toggle(enabled: bool) -> str:
    """Enable or disable telemetry collection.

    Args:
        enabled: True to enable, False to disable."""
    return _post_json("/api/telemetry/toggle", {"enabled": enabled})


@mcp.tool()
def telemetry_snapshot(
    level: str = "window",
    pane_uid: Optional[str] = None,
) -> str:
    """Get a telemetry snapshot.

    Args:
        level: "window" or "pane".
        pane_uid: Pane UID (only used when level is "pane")."""
    url = f"/api/telemetry/snapshot?level={level}"
    if pane_uid:
        url += f"&uid={urllib.parse.quote(pane_uid)}"
    return _get(url)


@mcp.tool()
def telemetry_record(pane_uid: str, event: dict) -> str:
    """Record a telemetry event (file op, network, tokens) for a pane.

    Args:
        pane_uid: Pane UID to record against.
        event: Event dict with kind and fields (e.g. {"kind": "Tokens", "count": 100})."""
    body = dict(event)
    body["pane_uid"] = pane_uid
    return _post_json("/api/telemetry/event", body)


@mcp.tool()
def telemetry_reset() -> str:
    """Reset all telemetry counters."""
    return _post_json("/api/telemetry/reset", {})


# ---------------------------------------------------------------------------
# Dashboard
# ---------------------------------------------------------------------------

@mcp.tool()
def dashboard_widgets(widgets: Optional[list] = None) -> str:
    """Get or set dashboard widget configuration.

    Args:
        widgets: List of widget configs to set. Omit to get current config.
                 Each widget has: id, kind, title, color, level, visible, order."""
    if widgets is None:
        return _get("/api/dashboard/widgets")
    return _post_json("/api/dashboard/widgets", widgets)


# ---------------------------------------------------------------------------
# Sticky notes
# ---------------------------------------------------------------------------

@mcp.tool()
def note_list() -> str:
    """List all sticky notes. Returns id, name, text, color, and position for each."""
    return _get("/api/notes")


@mcp.tool()
def note_create(
    text: Optional[str] = None,
    color: Optional[str] = None,
) -> str:
    """Create a new sticky note floating window.

    Args:
        text: Initial text content (optional).
        color: Background color hex e.g. "#fff9c4" (yellow), "#c8e6c9" (green),
               "#bbdefb" (blue), "#f8bbd0" (pink). Omit to auto-assign."""
    body: dict = {}
    if text is not None:
        body["text"] = text
    if color is not None:
        body["color"] = color
    return _post_json("/api/notes", body)


@mcp.tool()
def note_close(id: str) -> str:
    """Close an open sticky note window. Note content is preserved on disk.

    Args:
        id: Note ID from note_list (e.g. "note-1712345678-abc1")."""
    return _post_json("/api/notes/close", {"id": id})


# ---------------------------------------------------------------------------
# Diagnostics
# ---------------------------------------------------------------------------

@mcp.tool()
def hyperia_version() -> str:
    """Get the running Hyperia version."""
    status_raw = _get("/api/status")
    try:
        status = json.loads(status_raw)
        version = status.get("version", "unknown")
        return f"Hyperia v{version}"
    except Exception:
        return status_raw


@mcp.tool()
def sidecar_logs() -> str:
    """Get Hyperia sidecar logs (last N lines)."""
    return _get("/api/logs")


if __name__ == "__main__":
    mcp.run()
