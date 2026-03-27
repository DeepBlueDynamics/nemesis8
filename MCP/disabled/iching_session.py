#!/usr/bin/env python3
"""MCP tool for controlling I-Ching oracle sessions.

Quick start (AI hints):
- Call session_help() for endpoints and workflow guidance.
- Call session_list(), then session_attach(session_id).
- Use session_status() to confirm state, then drive actions.

This tool is a thin client for a separate I-Ching control service that exposes
JSON endpoints under /api/session/*.

Service implementer notes (keep this in the server repo too):
- Required endpoints (JSON POST):
  - /api/health -> {ok: bool}
  - /api/session/list -> {ok: true, sessions: [...]}
  - /api/session/status -> {ok: true, session: {...}}
  - /api/session/reset -> {ok: true}
  - /api/session/emitter/start -> {ok: true}
  - /api/session/emitter/stop -> {ok: true}
  - /api/session/clear -> {ok: true}
  - /api/session/zap -> {ok: true}
  - /api/session/ignite -> {ok: true}
  - /api/session/scan -> {ok: true}
  - /api/session/screenshot -> {ok: true, path?: str, image?: base64}
  - /api/session/camera/orbit -> {ok: true}
  - /api/session/camera/zoom -> {ok: true}
  - /api/session/chat/clear -> {ok: true}
  - /api/session/chat/show -> {ok: true}
  - /api/session/chat/hide -> {ok: true}
  - /api/session/cast -> {ok: true}
  - /api/session/logs -> {ok: true, logs: [...]}
  - /api/session/burn -> {ok: true, burn_id?: str}
- Payloads should accept {"session": "<id>", ...} and return {ok: bool}.
- If you can, include structured errors: {ok:false, error:"reason", detail?: "..."}.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import urllib.parse
import urllib.request
from urllib.error import HTTPError, URLError
from typing import Any, Dict, Optional

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("iching-session")

DEFAULT_BASE_URL = os.environ.get("ICHING_BASE_URL", "http://localhost:3000")
DEFAULT_TIMEOUT = float(os.environ.get("ICHING_TIMEOUT", "5"))
DEFAULT_SESSION_ID: Optional[str] = None

_CONFIG_PATH = Path(os.environ.get("CODEX_WORKSPACE_ROOT", "/workspace")) / ".iching-session.json"


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
    payload = {
        "base_url": DEFAULT_BASE_URL,
        "timeout_seconds": DEFAULT_TIMEOUT,
    }
    try:
        _CONFIG_PATH.write_text(json.dumps(payload, indent=2), encoding="utf-8")
    except OSError:
        # Ignore persistence errors; tool still functions with in-memory config.
        pass


_load_local_config()

ENDPOINT_CATALOG = [
    {"tool": "session_list", "path": "/api/session/list", "purpose": "List available sessions"},
    {"tool": "session_status", "path": "/api/session/status", "purpose": "Get session state"},
    {"tool": "session_reset", "path": "/api/session/reset", "purpose": "Reset laser/scene state"},
    {"tool": "session_emit_start", "path": "/api/session/emitter/start", "purpose": "Start emitter"},
    {"tool": "session_emit_stop", "path": "/api/session/emitter/stop", "purpose": "Stop emitter"},
    {"tool": "session_clear_straws", "path": "/api/session/clear", "purpose": "Force clear straws"},
    {"tool": "session_zap_remaining", "path": "/api/session/zap", "purpose": "Zap remaining straws"},
    {"tool": "session_ignite_straws", "path": "/api/session/ignite", "purpose": "Ignite without perturb"},
    {"tool": "session_scan_now", "path": "/api/session/scan", "purpose": "Force scan pass"},
    {"tool": "session_screenshot", "path": "/api/session/screenshot", "purpose": "Capture screenshot"},
    {"tool": "session_screenshot_latest", "path": "/api/session/screenshot/latest", "purpose": "Fetch latest screenshot (binary)"},
    {"tool": "session_camera_orbit", "path": "/api/session/camera/orbit", "purpose": "Set camera orbit"},
    {"tool": "session_camera_zoom", "path": "/api/session/camera/zoom", "purpose": "Dolly camera"},
    {"tool": "session_chat_clear", "path": "/api/session/chat/clear", "purpose": "Clear chat UI"},
    {"tool": "session_chat_show", "path": "/api/session/chat/show", "purpose": "Show chat UI"},
    {"tool": "session_chat_hide", "path": "/api/session/chat/hide", "purpose": "Hide chat UI"},
    {"tool": "session_ui_cast", "path": "/api/session/cast", "purpose": "Trigger cast action"},
    {"tool": "session_log_recent", "path": "/api/session/logs", "purpose": "Fetch recent logs"},
    {"tool": "session_burn", "path": "/api/session/burn", "purpose": "Clear then burn content"},
    {"tool": "session_audio_url", "path": "/api/audio/<file>", "purpose": "Get streaming URL for MP3 in server/audio"},
    {"tool": "session_audio_fetch", "path": "/api/audio/<file>", "purpose": "Download MP3 to local outputs"},
    {"tool": "session_audio_play", "path": "/api/session/audio/play", "purpose": "Play audio in the client (file or URL)"},
    {"tool": "session_straws_update", "path": "/api/session/straws/update", "purpose": "Send straw snapshot update"},
    {"tool": "session_straws_snapshot", "path": "/api/session/straws/snapshot", "purpose": "Fetch latest straw snapshot"},
]


def _result(success: bool, **kwargs: Any) -> Dict[str, Any]:
    data = {"ok": success, "success": success}
    data.update(kwargs)
    return data


def _request(
    path: str,
    payload: Optional[Dict[str, Any]] = None,
    method: str = "POST",
    base_url: Optional[str] = None,
    timeout: Optional[float] = None,
    params: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    base = (base_url or DEFAULT_BASE_URL).rstrip("/")
    url = f"{base}{path}"
    data = None
    headers = {"Content-Type": "application/json"}
    method = method.upper()
    query_params = params or (payload if method in {"GET", "HEAD"} else None)
    if query_params:
        query = urllib.parse.urlencode(query_params, doseq=True)
        url = f"{url}?{query}"
    if payload is not None and method not in {"GET", "HEAD"}:
        data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout or DEFAULT_TIMEOUT) as response:
            raw = response.read().decode("utf-8")
        if not raw:
            return _result(True, url=url)
        parsed = json.loads(raw)
        if isinstance(parsed, dict):
            parsed.setdefault("ok", True)
            parsed.setdefault("success", True)
            parsed.setdefault("url", url)
            return parsed
        return _result(True, data=parsed, url=url)
    except HTTPError as exc:
        detail = None
        try:
            detail = exc.read().decode("utf-8")
        except Exception:
            detail = None
        return _result(False, url=url, status=exc.code, error=str(exc), detail=detail)
    except URLError as exc:
        return _result(False, url=url, error=str(exc.reason))
    except Exception as exc:  # noqa: BLE001 - surface unexpected issues to caller
        return _result(False, url=url, error=str(exc))


def _request_bytes(
    path: str,
    base_url: Optional[str] = None,
    timeout: Optional[float] = None,
    query: Optional[str] = None,
) -> Dict[str, Any]:
    url = f"{(base_url or DEFAULT_BASE_URL).rstrip('/')}{path}"
    if query:
        url = f"{url}?{query}"
    req = urllib.request.Request(url, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=timeout or DEFAULT_TIMEOUT) as response:
            raw = response.read()
            content_type = response.headers.get("Content-Type", "")
        return _result(True, url=url, content_type=content_type, bytes=raw)
    except HTTPError as exc:
        detail = None
        try:
            detail = exc.read().decode("utf-8")
        except Exception:
            detail = None
        return _result(False, url=url, status=exc.code, error=str(exc), detail=detail)
    except URLError as exc:
        return _result(False, url=url, error=str(exc.reason))
    except Exception as exc:  # noqa: BLE001 - surface unexpected issues to caller
        return _result(False, url=url, error=str(exc))


def _normalize_audio_name(filename: str) -> str:
    name = os.path.basename(filename)
    if not name.lower().endswith(".mp3"):
        name += ".mp3"
    return name


def _audio_url(filename: str, base_url: Optional[str] = None) -> str:
    safe_name = urllib.parse.quote(filename)
    return f"{(base_url or DEFAULT_BASE_URL).rstrip('/')}/api/audio/{safe_name}"


def _ensure_session(session_id: Optional[str]) -> str:
    sid = session_id or DEFAULT_SESSION_ID
    if not sid:
        raise ValueError("session_id is required (call session_attach first)")
    return sid


@mcp.tool()
async def session_attach(session_id: str) -> Dict[str, Any]:
    """Attach to a session id for subsequent calls.

    This sets the default session for all subsequent calls that omit session_id.
    Use this after session_list() to bind the agent to a specific session.
    """
    global DEFAULT_SESSION_ID
    DEFAULT_SESSION_ID = session_id
    return _result(True, session=session_id)


@mcp.tool()
async def session_config() -> Dict[str, Any]:
    """Show current base URL, timeout, and attached session id.

    Use this to confirm where the tool is pointing and whether a session is attached.
    """
    return _result(
        True,
        base_url=DEFAULT_BASE_URL,
        timeout_seconds=DEFAULT_TIMEOUT,
        session=DEFAULT_SESSION_ID,
        next_steps=["Call session_help() for workflow guidance", "Call session_list() to discover sessions"],
    )


@mcp.tool()
async def session_set_base_url(base_url: str) -> Dict[str, Any]:
    """Set the base URL for the control service (in-memory, per process).

    Call this if the server is not running on localhost:3000.
    """
    global DEFAULT_BASE_URL
    DEFAULT_BASE_URL = base_url.rstrip("/")
    _save_local_config()
    return _result(True, base_url=DEFAULT_BASE_URL)


@mcp.tool()
async def session_set_timeout(timeout_seconds: float) -> Dict[str, Any]:
    """Set the HTTP timeout (seconds) for control service calls."""
    global DEFAULT_TIMEOUT
    DEFAULT_TIMEOUT = float(timeout_seconds)
    _save_local_config()
    return _result(True, timeout_seconds=DEFAULT_TIMEOUT)


@mcp.tool()
async def session_help() -> Dict[str, Any]:
    """Return workflow hints and the endpoint catalog for this tool.

    Agents should call this first to understand available actions and sequence.
    """
    workflow = [
        "Call session_list() to find active sessions.",
        "Call session_attach(session_id) once to set the default.",
        "Call session_status() to confirm state.",
        "Drive actions: emitter, scan, screenshot, cast, or camera controls.",
        "Use session_log_recent() for debugging when something looks stuck.",
    ]
    return _result(
        True,
        base_url=DEFAULT_BASE_URL,
        session=DEFAULT_SESSION_ID,
        workflow=workflow,
        endpoints=ENDPOINT_CATALOG,
        next_steps=["Call session_list()", "Call session_attach(session_id)", "Call session_status()"],
    )


@mcp.tool()
async def session_ping() -> Dict[str, Any]:
    """Ping the control service (expects /api/health or returns error details)."""
    return _request("/api/health", payload={}, method="POST")


@mcp.tool()
async def session_list() -> Dict[str, Any]:
    """List available sessions (server must implement /api/session/list).

    Use this before session_attach().
    """
    return _request("/api/session/list", {})


@mcp.tool()
async def session_status(session_id: Optional[str] = None) -> Dict[str, Any]:
    """Get session state. Requires session_attach() or session_id.

    Good for sanity checks before firing emit/scan/burn actions.
    """
    sid = _ensure_session(session_id)
    return _request("/api/session/status", {"session": sid})


@mcp.tool()
async def session_reset(session_id: Optional[str] = None) -> Dict[str, Any]:
    """Laser blast reset (no perturb). Requires session_attach() or session_id."""
    sid = _ensure_session(session_id)
    return _request("/api/session/reset", {"session": sid})


@mcp.tool()
async def session_emit_start(session_id: Optional[str] = None) -> Dict[str, Any]:
    """Start emitter (clears straws first if needed). Requires session_attach() or session_id.

    Use this to begin the physical/visual emission process.
    """
    sid = _ensure_session(session_id)
    return _request("/api/session/emitter/start", {"session": sid})


@mcp.tool()
async def session_emit_stop(session_id: Optional[str] = None) -> Dict[str, Any]:
    """Stop emitter. Requires session_attach() or session_id."""
    sid = _ensure_session(session_id)
    return _request("/api/session/emitter/stop", {"session": sid})


@mcp.tool()
async def session_clear_straws(session_id: Optional[str] = None) -> Dict[str, Any]:
    """Force clear all straws. Requires session_attach() or session_id.

    Use when the scene is cluttered or after a cast.
    """
    sid = _ensure_session(session_id)
    return _request("/api/session/clear", {"session": sid})


@mcp.tool()
async def session_zap_remaining(session_id: Optional[str] = None) -> Dict[str, Any]:
    """Zap last remaining straws. Requires session_attach() or session_id."""
    sid = _ensure_session(session_id)
    return _request("/api/session/zap", {"session": sid})


@mcp.tool()
async def session_ignite_straws(session_id: Optional[str] = None) -> Dict[str, Any]:
    """Ignite straws without perturbing them. Requires session_attach() or session_id."""
    sid = _ensure_session(session_id)
    return _request("/api/session/ignite", {"session": sid})


@mcp.tool()
async def session_scan_now(session_id: Optional[str] = None) -> Dict[str, Any]:
    """Force a scan pass now. Requires session_attach() or session_id.

    Use after a burn or cast to refresh the state.
    """
    sid = _ensure_session(session_id)
    return _request("/api/session/scan", {"session": sid})


@mcp.tool()
async def session_screenshot(session_id: Optional[str] = None) -> Dict[str, Any]:
    """Capture a screenshot. Requires session_attach() or session_id.

    Useful for debugging or recording outputs for the UI.
    """
    sid = _ensure_session(session_id)
    return _request("/api/session/screenshot", {"session": sid})


@mcp.tool()
async def session_screenshot_latest(session_id: Optional[str] = None, save: bool = True) -> Dict[str, Any]:
    """Fetch the latest screenshot and save it to ./outputs.

    This tool never returns raw/binary image data to the agent.
    """
    sid = _ensure_session(session_id)
    result = _request_bytes("/api/session/screenshot/latest", query=f"session={sid}")
    if not result.get("ok"):
        return result
    raw = result.get("bytes", b"")
    if not raw:
        return _result(False, url=result.get("url"), error="empty_response")
    # Always save to disk; do not return binary or base64 payloads.
    out_dir = os.path.join(os.getcwd(), "outputs")
    os.makedirs(out_dir, exist_ok=True)
    path = os.path.join(out_dir, f"session_{sid}_latest.png")
    with open(path, "wb") as handle:
        handle.write(raw)
    return _result(True, url=result.get("url"), path=path, content_type=result.get("content_type"))


@mcp.tool()
async def session_audio_url(filename: str, base_url: Optional[str] = None) -> Dict[str, Any]:
    """Return a streaming URL for an MP3 file served by the control service.

    The server only serves .mp3 from /api/audio/<file>. This tool never returns
    binary data; it only provides the URL.
    """
    name = _normalize_audio_name(filename)
    url = _audio_url(name, base_url=base_url)
    return _result(True, url=url, filename=name)


@mcp.tool()
async def session_audio_fetch(
    filename: str,
    save_dir: str = "outputs",
    base_url: Optional[str] = None,
) -> Dict[str, Any]:
    """Download an MP3 from the control service and save it locally.

    Set save_dir="voice-outbox" if you want audio next to other TTS outputs.
    This tool never returns binary data to the agent.
    """
    name = _normalize_audio_name(filename)
    result = _request_bytes(f"/api/audio/{urllib.parse.quote(name)}", base_url=base_url)
    if not result.get("ok"):
        return result
    raw = result.get("bytes", b"")
    if not raw:
        return _result(False, url=result.get("url"), error="empty_response")
    out_dir = os.path.join(os.getcwd(), save_dir)
    os.makedirs(out_dir, exist_ok=True)
    path = os.path.join(out_dir, name)
    with open(path, "wb") as handle:
        handle.write(raw)
    return _result(True, url=result.get("url"), path=path, content_type=result.get("content_type"))


@mcp.tool()
async def session_audio_play(
    session_id: Optional[str] = None,
    file: Optional[str] = None,
    url: Optional[str] = None,
    volume: Optional[float] = None,
    loop: bool = False,
) -> Dict[str, Any]:
    """Play an audio clip in the client.

    Provide either a file name (served from server/audio) or a full URL.
    """
    if not file and not url:
        return _result(False, error="Provide file or url")
    sid = _ensure_session(session_id)
    payload: Dict[str, Any] = {"session": sid, "loop": bool(loop)}
    if file:
        payload["file"] = _normalize_audio_name(file)
    if url:
        payload["url"] = url
    if volume is not None:
        payload["volume"] = float(volume)
    return _request("/api/session/audio/play", payload)


@mcp.tool()
async def session_camera_orbit(angle: float, session_id: Optional[str] = None) -> Dict[str, Any]:
    """Set camera orbit angle. Requires session_attach() or session_id.

    angle is typically degrees around the scene origin.
    """
    sid = _ensure_session(session_id)
    return _request("/api/session/camera/orbit", {"session": sid, "angle": angle})


@mcp.tool()
async def session_camera_zoom(delta: float, session_id: Optional[str] = None) -> Dict[str, Any]:
    """Set camera dolly. Requires session_attach() or session_id.

    delta is a signed value; positive zooms in, negative zooms out.
    """
    sid = _ensure_session(session_id)
    return _request("/api/session/camera/zoom", {"session": sid, "delta": delta})


@mcp.tool()
async def session_chat_clear(session_id: Optional[str] = None) -> Dict[str, Any]:
    """Clear chat UI. Requires session_attach() or session_id."""
    sid = _ensure_session(session_id)
    return _request("/api/session/chat/clear", {"session": sid})


@mcp.tool()
async def session_chat_show(session_id: Optional[str] = None) -> Dict[str, Any]:
    """Show chat UI. Requires session_attach() or session_id."""
    sid = _ensure_session(session_id)
    return _request("/api/session/chat/show", {"session": sid})


@mcp.tool()
async def session_chat_hide(session_id: Optional[str] = None) -> Dict[str, Any]:
    """Hide chat UI. Requires session_attach() or session_id."""
    sid = _ensure_session(session_id)
    return _request("/api/session/chat/hide", {"session": sid})


@mcp.tool()
async def session_ui_cast(session_id: Optional[str] = None) -> Dict[str, Any]:
    """Trigger cast action. Requires session_attach() or session_id.

    This is the high-level "do the I Ching cast now" action in the UI.
    """
    sid = _ensure_session(session_id)
    return _request("/api/session/cast", {"session": sid})


@mcp.tool()
async def session_log_recent(limit: int = 50, session_id: Optional[str] = None) -> Dict[str, Any]:
    """Fetch recent session logs. Requires session_attach() or session_id.

    Use this when debugging stuck sessions or burn failures.
    """
    sid = _ensure_session(session_id)
    return _request("/api/session/logs", {"session": sid, "limit": limit})


@mcp.tool()
async def session_burn(
    content: str,
    kind: str = "text",
    format: str = "dot_matrix",
    clear_first: bool = True,
    session_id: Optional[str] = None,
    options: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    """Clear the scene and burn content (text, graph, or image) into the session.

    Args:
        content: Raw content to burn (text, DOT graph source, or encoded image data).
        kind: "text", "graph", "image", or "raw".
        format: Rendering format hint (default "dot_matrix").
        clear_first: If True, clear the scene before burning.
        session_id: Override session id (otherwise uses session_attach).
        options: Optional rendering options (font, scale, brightness, etc.).
    """
    sid = _ensure_session(session_id)
    steps: list[str] = []

    if clear_first:
        cleared = _request("/api/session/clear", {"session": sid, "reason": "burn"})
        if not cleared.get("ok", False):
            return _result(False, error="clear_failed", detail=cleared)
        steps.append("cleared")

    payload: Dict[str, Any] = {
        "session": sid,
        "kind": kind,
        "content": content,
        "format": format,
    }
    if options:
        payload["options"] = options

    burned = _request("/api/session/burn", payload)
    if isinstance(burned, dict):
        burned["steps"] = steps
    return burned


@mcp.tool()
async def session_straws_update(
    straws: list[Dict[str, Any]],
    stats: Optional[Dict[str, Any]] = None,
    t: Optional[int] = None,
    session_id: Optional[str] = None,
) -> Dict[str, Any]:
    """Send a snapshot of straw positions/orientations to the control service.

    Payload shape matches the /api/session/straws/update endpoint spec.
    """
    sid = _ensure_session(session_id)
    payload: Dict[str, Any] = {
        "session": sid,
        "straws": straws,
    }
    if t is not None:
        payload["t"] = t
    if stats is not None:
        payload["stats"] = stats
    return _request("/api/session/straws/update", payload)


@mcp.tool()
async def session_straws_snapshot(session_id: Optional[str] = None) -> Dict[str, Any]:
    """Fetch the latest straw snapshot from the server."""
    sid = _ensure_session(session_id)
    return _request("/api/session/straws/snapshot", {"session": sid})


@mcp.tool()
async def session_raw_request(
    path: str,
    payload: Optional[Dict[str, Any]] = None,
    method: str = "POST",
    session_id: Optional[str] = None,
    include_session: bool = True,
) -> Dict[str, Any]:
    """Send an arbitrary request to the control service (debugging use).

    Use this when the server adds new endpoints before the tool is updated.
    """
    body = dict(payload or {})
    if include_session:
        sid = _ensure_session(session_id)
        body.setdefault("session", sid)
    return _request(path, body if body else None, method=method)


if __name__ == "__main__":
    mcp.run()
