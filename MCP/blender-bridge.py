#!/usr/bin/env python3
"""Blender Bridge (MCP)
=======================

Thin MCP client for a local Blender HTTP bridge add-on.

Quick start:
- Ensure the Blender add-on is enabled and listening (default http://127.0.0.1:8787).
- Call blender_health() to confirm connectivity.
- Use blender_workspace("Scripting") + blender_script_set(..., open_editor=True) + blender_script_run(...).
- Use blender_view_set("top") or blender_view_orbit(yaw=15) to move the view.
- Use blender_screenshot(path=...) to save a viewport PNG (no binary data returned).

Env:
- BLENDER_BRIDGE_URL (default http://host.docker.internal:8787)
- BLENDER_BRIDGE_TIMEOUT (seconds, default 5)
"""

from __future__ import annotations

import json
import os
import urllib.parse
import urllib.request
from urllib.error import HTTPError, URLError
from pathlib import Path
from typing import Any, Dict, Optional

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("blender-bridge")

DEFAULT_BASE_URL = os.environ.get("BLENDER_BRIDGE_URL", "http://host.docker.internal:8787")
DEFAULT_TIMEOUT = float(os.environ.get("BLENDER_BRIDGE_TIMEOUT", "5"))

_CONFIG_PATH = Path(os.environ.get("CODEX_WORKSPACE_ROOT", "/workspace")) / ".blender-bridge.json"


ENDPOINT_CATALOG = [
    {"tool": "blender_health", "path": "/api/health", "purpose": "Health check + Blender version"},
    {"tool": "blender_workspace", "path": "/api/ui/workspace", "purpose": "Switch workspace (e.g. Scripting)"},
    {"tool": "blender_script_set", "path": "/api/script/set", "purpose": "Create/update a text block"},
    {"tool": "blender_script_run", "path": "/api/script/run", "purpose": "Run a text block"},
    {"tool": "blender_view_set", "path": "/api/view/set", "purpose": "Set standard view (top/front/etc)"},
    {"tool": "blender_view_orbit", "path": "/api/view/orbit", "purpose": "Orbit view by yaw/pitch/roll degrees"},
    {"tool": "blender_view_zoom", "path": "/api/view/zoom", "purpose": "Zoom view by delta/factor"},
    {"tool": "blender_screenshot", "path": "/api/screenshot", "purpose": "Save viewport/window screenshot to file"},
    {"tool": "blender_logs", "path": "/api/logs", "purpose": "Fetch bridge logs"},
]


def _load_local_config() -> None:
    global DEFAULT_BASE_URL, DEFAULT_TIMEOUT
    if not _CONFIG_PATH.exists():
        return
    try:
        payload = json.loads(_CONFIG_PATH.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return
    if isinstance(payload, dict):
        base_url = payload.get("base_url")
        timeout = payload.get("timeout_seconds")
        if base_url:
            DEFAULT_BASE_URL = str(base_url).rstrip("/")
        if timeout is not None:
            try:
                DEFAULT_TIMEOUT = float(timeout)
            except (TypeError, ValueError):
                pass


def _save_local_config() -> None:
    payload = {"base_url": DEFAULT_BASE_URL, "timeout_seconds": DEFAULT_TIMEOUT}
    try:
        _CONFIG_PATH.write_text(json.dumps(payload, indent=2), encoding="utf-8")
    except OSError:
        pass


_load_local_config()


def _result(success: bool, **kwargs: Any) -> Dict[str, Any]:
    data = {"ok": success, "success": success}
    data.update(kwargs)
    return data


def _request_json(path: str, payload: Optional[Dict[str, Any]] = None, method: str = "POST") -> Dict[str, Any]:
    url = DEFAULT_BASE_URL.rstrip("/") + path
    data = None
    headers = {"Content-Type": "application/json"}
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=DEFAULT_TIMEOUT) as resp:
            raw = resp.read().decode("utf-8")
            return json.loads(raw)
    except HTTPError as e:
        try:
            detail = e.read().decode("utf-8")
        except Exception:
            detail = str(e)
        return _result(False, error=f"HTTP {e.code}", detail=detail)
    except URLError as e:
        return _result(False, error=f"Connection error: {e}")
    except json.JSONDecodeError as e:
        return _result(False, error=f"Invalid JSON: {e}")


def _request_json_get(path: str, params: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
    if params:
        query = urllib.parse.urlencode(params)
        path = f"{path}?{query}"
    return _request_json(path, payload=None, method="GET")


@mcp.tool()
def blender_set_base_url(base_url: str) -> Dict[str, Any]:
    """Set the Blender bridge base URL (persisted in .blender-bridge.json)."""
    global DEFAULT_BASE_URL
    DEFAULT_BASE_URL = base_url.rstrip("/")
    _save_local_config()
    return _result(True, base_url=DEFAULT_BASE_URL)


@mcp.tool()
def blender_health() -> Dict[str, Any]:
    """Health check the Blender bridge and return Blender version."""
    return _request_json_get("/api/health")


@mcp.tool()
def blender_workspace(name: str = "Scripting") -> Dict[str, Any]:
    """Switch Blender workspace (e.g. "Scripting")."""
    return _request_json("/api/ui/workspace", {"name": name})


@mcp.tool()
def blender_script_set(
    name: str,
    text: str,
    open_editor: bool = True,
    make_active: bool = True,
) -> Dict[str, Any]:
    """Create/update a Blender text block.

    Args:
      name: Text block name (e.g. "codex_script").
      text: Script contents.
      open_editor: Switch to Scripting workspace + open text editor.
      make_active: Make this text active in the editor if available.
    """
    payload = {
        "name": name,
        "text": text,
        "open_editor": open_editor,
        "make_active": make_active,
    }
    return _request_json("/api/script/set", payload)


@mcp.tool()
def blender_script_run(name: str) -> Dict[str, Any]:
    """Run a Blender text block by name."""
    return _request_json("/api/script/run", {"name": name})


@mcp.tool()
def blender_view_set(view: str) -> Dict[str, Any]:
    """Set a standard view: top/bottom/front/back/left/right/camera/iso/persp/ortho."""
    return _request_json("/api/view/set", {"view": view})


@mcp.tool()
def blender_view_orbit(yaw: float = 0.0, pitch: float = 0.0, roll: float = 0.0) -> Dict[str, Any]:
    """Orbit the view by degrees. Positive yaw rotates around Z, pitch around X, roll around Y."""
    return _request_json("/api/view/orbit", {"yaw": yaw, "pitch": pitch, "roll": roll})


@mcp.tool()
def blender_view_zoom(delta: Optional[float] = None, factor: Optional[float] = None) -> Dict[str, Any]:
    """Zoom the view.

    Args:
      delta: Positive zooms in, negative zooms out (applied as factor = 1 - delta).
      factor: Direct multiplier on view distance (overrides delta).
    """
    payload: Dict[str, Any] = {}
    if delta is not None:
        payload["delta"] = delta
    if factor is not None:
        payload["factor"] = factor
    return _request_json("/api/view/zoom", payload)


@mcp.tool()
def blender_screenshot(path: str, mode: str = "viewport") -> Dict[str, Any]:
    """Save a screenshot to disk (no binary returned).

    Args:
      path: Host file path to save (e.g. C:/tmp/shot.png).
      mode: "viewport" (OpenGL) or "window" (full UI).
    """
    return _request_json("/api/screenshot", {"path": path, "mode": mode})


@mcp.tool()
def blender_logs(limit: int = 200) -> Dict[str, Any]:
    """Fetch recent bridge logs."""
    return _request_json_get("/api/logs", {"limit": limit})


if __name__ == "__main__":
    mcp.run()
