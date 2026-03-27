#!/usr/bin/env python3
"""MCP: terminal-mcp

Expose a terminal bridge HTTP API as MCP tools.

Primary target:
- legacy terminal bridge (examples/interactive_plus/terminal_bridge.py):
  - GET  /health
  - GET  /tail?bytes=...
  - POST /input
  - POST /prompt
  - POST /key

Also supports (if available) richer endpoints:
- GET  /api/status
- GET  /api/screen
- POST /api/keys
- POST /api/pane/split
- POST /api/pane/close
- POST /api/pane/focus
- POST /api/pane/rename
- POST /api/quit
- GET  /api/screenshot

Console (quake-style agent overlay):
- POST /api/console/open
- POST /api/console/close
- POST /api/console/toggle
- POST /api/console/chat
- GET  /api/console/logs?last=N
- GET  /api/console/messages
- GET  /api/console/status
"""

from __future__ import annotations

import base64
import json
import os
import re
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple
from urllib import error as urlerror
from urllib import parse as urlparse
from urllib import request as urlrequest

try:
    from mcp.server.fastmcp import FastMCP  # type: ignore
except Exception:
    class FastMCP:  # pragma: no cover
        def __init__(self, _name: str):
            self.name = _name

        def tool(self, *args: Any, **kwargs: Any):
            def _decorator(fn: Any) -> Any:
                return fn

            return _decorator

        def run(self, *args: Any, **kwargs: Any) -> None:
            raise RuntimeError("FastMCP runtime not available")


mcp = FastMCP("terminal-mcp")

DEFAULT_BASE_URL = os.environ.get("TERMINAL_BRIDGE_URL", "http://host.docker.internal:8096").rstrip("/")
DEFAULT_TIMEOUT = float(os.environ.get("TERMINAL_BRIDGE_TIMEOUT_SEC", "10"))
SCREENSHOT_DIR = Path(
    os.environ.get("TERMINAL_MCP_SCREENSHOT_DIR", "/workspace/codex-container/outputs/terminal-screenshots")
)


def _now_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


def _base_url(base_url: Optional[str]) -> str:
    return (base_url or DEFAULT_BASE_URL).rstrip("/")


def _request(
    method: str,
    base_url: str,
    path: str,
    json_payload: Optional[Dict[str, Any]] = None,
    timeout: float = DEFAULT_TIMEOUT,
) -> Tuple[int, bytes, Dict[str, str], Optional[str]]:
    url = f"{base_url}{path}"
    body: Optional[bytes] = None
    headers: Dict[str, str] = {"Accept": "application/json, text/plain, */*"}
    if json_payload is not None:
        body = json.dumps(json_payload).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = urlrequest.Request(url=url, data=body, headers=headers, method=method)
    try:
        with urlrequest.urlopen(req, timeout=timeout) as resp:
            data = resp.read()
            return int(resp.status), data, dict(resp.headers.items()), None
    except urlerror.HTTPError as exc:
        data = exc.read() if hasattr(exc, "read") else b""
        hdr = dict(exc.headers.items()) if getattr(exc, "headers", None) else {}
        return int(exc.code), data, hdr, None
    except Exception as exc:
        return 0, b"", {}, f"{type(exc).__name__}: {exc}"


def _parse_json(raw: bytes) -> Optional[Dict[str, Any]]:
    if not raw:
        return None
    try:
        parsed = json.loads(raw.decode("utf-8", errors="replace"))
        return parsed if isinstance(parsed, dict) else None
    except Exception:
        return None


def _endpoint_probe(base_url: str) -> Dict[str, bool]:
    out: Dict[str, bool] = {}
    for p in (
        "/health",
        "/tail?bytes=8",
        "/api/status",
        "/api/screen",
        "/api/screenshot",
        "/api/pane/close",
        "/api/quit",
    ):
        status, _, _, err = _request("GET", base_url, p)
        out[p] = bool(not err and 200 <= status < 300)
    return out


def _status_payload(base_url: str) -> Optional[Dict[str, Any]]:
    status, raw, _, err = _request("GET", base_url, "/api/status")
    if err or status != 200:
        return None
    return _parse_json(raw)


def _normalize_panes(status_json: Optional[Dict[str, Any]]) -> List[Dict[str, Any]]:
    if not isinstance(status_json, dict):
        return []
    raw_panes = status_json.get("panes")
    if not isinstance(raw_panes, list):
        return []
    panes: List[Dict[str, Any]] = []
    focused_id = status_json.get("focused_pane_id")
    for pane in raw_panes:
        if not isinstance(pane, dict):
            continue
        pane_id = pane.get("id") or pane.get("pane_id")
        if not pane_id:
            continue
        title = pane.get("title") or pane.get("name") or ""
        focused = bool(pane.get("focused") or (focused_id and pane_id == focused_id))
        panes.append({"pane_id": str(pane_id), "title": str(title), "focused": focused})
    return panes


def _close_pane_call(base_url: str, pane_id: str) -> Dict[str, Any]:
    payload = {"pane_id": pane_id}
    for path in ("/api/pane/close", "/api/close"):
        status, raw, _, err = _request("POST", base_url, path, json_payload=payload)
        if err:
            return {"ok": False, "error": err, "path": path}
        if status == 200:
            return {"ok": True, "path": path, "result": _parse_json(raw) or raw.decode("utf-8", errors="replace")}
        if status in (404, 405):
            continue
        return {"ok": False, "error": f"HTTP {status}", "path": path, "body": raw.decode("utf-8", errors="replace")}
    return {"ok": False, "error": "close endpoint unavailable", "path": "/api/pane/close"}


def _screen_text(base_url: str, max_lines: int = 120, max_bytes: int = 65536) -> str:
    # Prefer /api/screen (structured).
    status, raw, _, err = _request("GET", base_url, "/api/screen")
    if not err and status == 200:
        j = _parse_json(raw)
        if isinstance(j, dict):
            lines = j.get("lines")
            if isinstance(lines, list):
                out: List[str] = []
                for row in lines[:max_lines]:
                    if isinstance(row, dict):
                        out.append(str(row.get("text", "")))
                    else:
                        out.append(str(row))
                return "\n".join(out)
    # Fallback: /tail
    path = f"/tail?{urlparse.urlencode({'bytes': str(max_bytes)})}"
    status2, raw2, _, err2 = _request("GET", base_url, path)
    if err2 or status2 != 200:
        return ""
    j2 = _parse_json(raw2)
    if isinstance(j2, dict) and isinstance(j2.get("tail"), str):
        return str(j2["tail"])
    return raw2.decode("utf-8", errors="replace")


def _send_ctrl_c(base_url: str) -> Dict[str, Any]:
    # Raw ETX (Ctrl+C).
    payload = {"raw_b64": base64.b64encode(b"\x03").decode("ascii")}
    status, raw, _, err = _request("POST", base_url, "/api/keys", json_payload=payload)
    if err:
        return {"ok": False, "detail": err}
    if status == 200:
        return {"ok": True, "result": _parse_json(raw) or raw.decode("utf-8", errors="replace")}
    # Legacy fallback.
    status2, raw2, _, err2 = _request("POST", base_url, "/key", json_payload={"key": "ctrl+c"})
    if err2:
        return {"ok": False, "detail": err2}
    if status2 == 200:
        return {"ok": True, "result": _parse_json(raw2) or raw2.decode("utf-8", errors="replace")}
    return {"ok": False, "detail": f"HTTP {status2}"}


def _unsupported(action: str, base_url: str, detail: str) -> Dict[str, Any]:
    return {
        "success": False,
        "error": "unsupported_endpoint",
        "action": action,
        "detail": detail,
        "base_url": base_url,
        "next_steps": [
            "Use examples/interactive_plus/terminal_bridge.py for /tail,/input,/prompt,/key support",
            "Or add /api/* endpoints on the bridge and retry",
        ],
    }


@mcp.tool()
def terminal_status(base_url: Optional[str] = None) -> Dict[str, Any]:
    """Get terminal bridge status and endpoint availability."""
    base = _base_url(base_url)
    probe = _endpoint_probe(base)
    status, raw, _, err = _request("GET", base, "/api/status")
    if err:
        return {
            "success": False,
            "error": "connection_failed",
            "detail": err,
            "base_url": base,
            "next_steps": ["Start terminal bridge", "Verify TERMINAL_BRIDGE_URL and port"],
        }

    api_status = _parse_json(raw) if status == 200 else None
    legacy_health = None
    if api_status is None:
        h_status, h_raw, _, _ = _request("GET", base, "/health")
        h_json = _parse_json(h_raw)
        legacy_health = h_json if h_json is not None else {"status": h_status, "body": h_raw.decode("utf-8", errors="replace")}

    return {
        "success": True,
        "base_url": base,
        "timestamp": _now_iso(),
        "endpoints": probe,
        "api_status": api_status,
        "legacy_health": legacy_health,
    }


@mcp.tool()
def terminal_screen(
    max_bytes: int = 65536,
    base_url: Optional[str] = None,
    include_full: bool = False,
    max_lines: int = 120,
) -> Dict[str, Any]:
    """Read terminal screen/tail text."""
    base = _base_url(base_url)
    if max_bytes < 256:
        max_bytes = 256
    if max_bytes > 2_000_000:
        max_bytes = 2_000_000
    if max_lines < 20:
        max_lines = 20
    if max_lines > 1000:
        max_lines = 1000

    status, raw, _, err = _request("GET", base, "/api/screen")
    if err:
        return {"success": False, "error": "connection_failed", "detail": err, "base_url": base}
    if status == 200:
        j = _parse_json(raw)
        if isinstance(j, dict):
            if include_full:
                return {"success": True, "base_url": base, "screen": j}

            if "error" in j:
                return {
                    "success": False,
                    "error": "screen_error",
                    "detail": str(j.get("error")),
                    "base_url": base,
                    "next_steps": ["Check terminal_status() for active/focused panes", "Focus a valid pane and retry"],
                }

            lines = j.get("lines")
            if isinstance(lines, list):
                text_lines: List[str] = []
                for row in lines[:max_lines]:
                    if isinstance(row, dict):
                        text_lines.append(str(row.get("text", "")))
                    else:
                        text_lines.append(str(row))
                text = "\n".join(text_lines).rstrip()
                return {
                    "success": True,
                    "base_url": base,
                    "rows": j.get("rows"),
                    "cols": j.get("cols"),
                    "cursor": j.get("cursor"),
                    "title": j.get("title"),
                    "line_count": len(text_lines),
                    "text": text,
                    "truncated": len(lines) > len(text_lines),
                }

            # Unknown /api/screen shape: return compact payload.
            return {"success": True, "base_url": base, "screen": j}

    # Legacy fallback: /tail?bytes=N
    path = f"/tail?{urlparse.urlencode({'bytes': str(max_bytes)})}"
    t_status, t_raw, _, t_err = _request("GET", base, path)
    if t_err:
        return {"success": False, "error": "connection_failed", "detail": t_err, "base_url": base}
    if t_status != 200:
        return _unsupported("terminal_screen", base, "Neither /api/screen nor /tail is available")
    j = _parse_json(t_raw)
    if isinstance(j, dict) and "tail" in j:
        return {"success": True, "base_url": base, "tail": j.get("tail", ""), "bytes": max_bytes}
    return {"success": True, "base_url": base, "tail": t_raw.decode("utf-8", errors="replace"), "bytes": max_bytes}


@mcp.tool()
def terminal_keys(
    text: Optional[str] = None,
    keys: Optional[List[str]] = None,
    submit: bool = False,
    raw_base64: Optional[str] = None,
    base_url: Optional[str] = None,
) -> Dict[str, Any]:
    """Inject text/keys into terminal."""
    base = _base_url(base_url)
    warning: Optional[str] = None
    payload: Dict[str, Any] = {}
    if text:
        payload["text"] = text
    if keys:
        payload["keys"] = keys
    if submit:
        payload["submit"] = True
    if raw_base64:
        payload["raw_b64"] = raw_base64
    if text and not submit and not keys and not raw_base64:
        warning = "text sent without submit=True; command may be typed but not executed"

    status, raw, _, err = _request("POST", base, "/api/keys", json_payload=payload)
    if err:
        return {"success": False, "error": "connection_failed", "detail": err, "base_url": base}
    if status == 200:
        out = {"success": True, "base_url": base, "endpoint": "/api/keys", "result": _parse_json(raw) or raw.decode("utf-8", errors="replace")}
        if warning:
            out["warning"] = warning
        return out

    # Legacy fallback: /input
    status2, raw2, _, err2 = _request("POST", base, "/input", json_payload=payload)
    if err2:
        return {"success": False, "error": "connection_failed", "detail": err2, "base_url": base}
    if status2 == 200:
        out = {"success": True, "base_url": base, "endpoint": "/input", "result": _parse_json(raw2) or raw2.decode("utf-8", errors="replace")}
        if warning:
            out["warning"] = warning
        return out
    return _unsupported("terminal_keys", base, "Neither /api/keys nor /input is available")


@mcp.tool()
def terminal_prompt(text: str, base_url: Optional[str] = None) -> Dict[str, Any]:
    """Send prompt text and press Enter."""
    base = _base_url(base_url)
    if not text:
        return {"success": False, "error": "text is required"}

    status, raw, _, err = _request("POST", base, "/prompt", json_payload={"text": text})
    if err:
        return {"success": False, "error": "connection_failed", "detail": err, "base_url": base}
    if status == 200:
        return {"success": True, "base_url": base, "endpoint": "/prompt", "result": _parse_json(raw) or raw.decode("utf-8", errors="replace")}

    # API fallback
    return terminal_keys(text=text, submit=True, base_url=base)


@mcp.tool()
def terminal_run(
    text: str,
    recover_continuation: bool = True,
    base_url: Optional[str] = None,
) -> Dict[str, Any]:
    """Run a command robustly; if PowerShell is stuck at `>>`, send Ctrl+C and retry."""
    base = _base_url(base_url)
    if not text:
        return {"success": False, "error": "text is required"}

    steps: List[Dict[str, Any]] = []
    steps.append({"task": f"Send command: {text}", "checkbox": "☐", "status": "pending"})
    first = terminal_prompt(text=text, base_url=base)
    if not first.get("success"):
        steps[-1]["checkbox"] = "☒"
        steps[-1]["status"] = "failed"
        return {"success": False, "base_url": base, "steps": steps, "detail": first}
    steps[-1]["checkbox"] = "☑"
    steps[-1]["status"] = "done"

    screen = _screen_text(base)
    continuation = bool(re.search(r"(?m)^>>\\s*", screen))
    if continuation and recover_continuation:
        steps.append({"task": "Detected PowerShell continuation prompt (`>>`), send Ctrl+C", "checkbox": "☐", "status": "pending"})
        c = _send_ctrl_c(base)
        if c.get("ok"):
            steps[-1]["checkbox"] = "☑"
            steps[-1]["status"] = "done"
            steps.append({"task": f"Retry command: {text}", "checkbox": "☐", "status": "pending"})
            second = terminal_prompt(text=text, base_url=base)
            if second.get("success"):
                steps[-1]["checkbox"] = "☑"
                steps[-1]["status"] = "done"
            else:
                steps[-1]["checkbox"] = "☒"
                steps[-1]["status"] = "failed"
                return {"success": False, "base_url": base, "steps": steps, "detail": second}
        else:
            steps[-1]["checkbox"] = "☒"
            steps[-1]["status"] = "failed"
            return {"success": False, "base_url": base, "steps": steps, "detail": c}

    return {
        "success": True,
        "base_url": base,
        "steps": steps,
        "continuation_detected": continuation,
    }


@mcp.tool()
def terminal_key(key: str, base_url: Optional[str] = None) -> Dict[str, Any]:
    """Send one key chord (e.g. ctrl+c, up, enter)."""
    base = _base_url(base_url)
    if not key:
        return {"success": False, "error": "key is required"}

    status, raw, _, err = _request("POST", base, "/key", json_payload={"key": key})
    if err:
        return {"success": False, "error": "connection_failed", "detail": err, "base_url": base}
    if status == 200:
        return {"success": True, "base_url": base, "endpoint": "/key", "result": _parse_json(raw) or raw.decode("utf-8", errors="replace")}

    # API fallback
    return terminal_keys(keys=[key], base_url=base)


@mcp.tool()
def terminal_split(direction: str = "vertical", base_url: Optional[str] = None) -> Dict[str, Any]:
    """Request terminal pane split (requires /api/pane/split support)."""
    base = _base_url(base_url)
    payload = {"direction": direction}
    status, raw, _, err = _request("POST", base, "/api/pane/split", json_payload=payload)
    if err:
        return {"success": False, "error": "connection_failed", "detail": err, "base_url": base}
    if status == 200:
        return {"success": True, "base_url": base, "result": _parse_json(raw) or raw.decode("utf-8", errors="replace")}
    return _unsupported("terminal_split", base, "/api/pane/split not available")


@mcp.tool()
def terminal_focus(pane_id: str, base_url: Optional[str] = None) -> Dict[str, Any]:
    """Focus a terminal pane by id (requires /api/pane/focus support)."""
    base = _base_url(base_url)
    if not pane_id:
        return {"success": False, "error": "pane_id is required"}
    status, raw, _, err = _request("POST", base, "/api/pane/focus", json_payload={"pane_id": pane_id})
    if err:
        return {"success": False, "error": "connection_failed", "detail": err, "base_url": base}
    if status == 200:
        return {"success": True, "base_url": base, "result": _parse_json(raw) or raw.decode("utf-8", errors="replace")}
    return _unsupported("terminal_focus", base, "/api/pane/focus not available")


@mcp.tool()
def terminal_rename(pane_id: str, title: str, base_url: Optional[str] = None) -> Dict[str, Any]:
    """Rename a terminal pane (requires /api/pane/rename support)."""
    base = _base_url(base_url)
    if not pane_id or not title:
        return {"success": False, "error": "pane_id and title are required"}
    status, raw, _, err = _request(
        "POST",
        base,
        "/api/pane/rename",
        json_payload={"pane_id": pane_id, "title": title},
    )
    if err:
        return {"success": False, "error": "connection_failed", "detail": err, "base_url": base}
    if status == 200:
        return {"success": True, "base_url": base, "result": _parse_json(raw) or raw.decode("utf-8", errors="replace")}
    return _unsupported("terminal_rename", base, "/api/pane/rename not available")


@mcp.tool()
def terminal_close(
    pane_id: Optional[str] = None,
    pane_title: Optional[str] = None,
    base_url: Optional[str] = None,
) -> Dict[str, Any]:
    """Close one pane by id or title (requires /api/pane/close support)."""
    base = _base_url(base_url)
    status_json = _status_payload(base)
    panes = _normalize_panes(status_json)
    target: Optional[Dict[str, Any]] = None

    if pane_id:
        target = next((p for p in panes if p["pane_id"] == pane_id), None)
    elif pane_title:
        wanted = pane_title.strip().lower()
        target = next((p for p in panes if p["title"].strip().lower() == wanted), None)
    else:
        target = next((p for p in panes if p.get("focused")), None) or (panes[0] if panes else None)

    if target is None:
        return {
            "success": False,
            "error": "pane_not_found",
            "base_url": base,
            "detail": "No matching pane found. Use terminal_status() first.",
            "requested": {"pane_id": pane_id, "pane_title": pane_title},
            "available_panes": panes,
        }

    result = _close_pane_call(base, target["pane_id"])
    if result.get("ok"):
        return {
            "success": True,
            "base_url": base,
            "closed_pane_id": target["pane_id"],
            "closed_pane_title": target.get("title", ""),
            "endpoint": result.get("path"),
            "result": result.get("result"),
        }

    return {
        "success": False,
        "error": "close_failed",
        "base_url": base,
        "detail": result.get("error", "unknown error"),
        "endpoint": result.get("path"),
        "closed_pane_id": target["pane_id"],
        "next_steps": ["Check terminal_status() to confirm pane IDs", "Ensure /api/pane/close is implemented"],
    }


@mcp.tool()
def terminal_quit(base_url: Optional[str] = None) -> Dict[str, Any]:
    """Quit terminal application (requires /api/quit support)."""
    base = _base_url(base_url)
    for path in ("/api/quit", "/quit"):
        status, raw, _, err = _request("POST", base, path, json_payload={})
        if err:
            return {"success": False, "error": "connection_failed", "detail": err, "base_url": base}
        if status == 200:
            return {
                "success": True,
                "base_url": base,
                "endpoint": path,
                "result": _parse_json(raw) or raw.decode("utf-8", errors="replace"),
            }
        if status in (404, 405):
            continue
        return {
            "success": False,
            "error": "quit_failed",
            "detail": f"HTTP {status}",
            "body": raw.decode("utf-8", errors="replace"),
            "endpoint": path,
            "base_url": base,
        }
    return _unsupported("terminal_quit", base, "/api/quit not available")


@mcp.tool()
def terminal_close_all(
    exit_terminal: bool = False,
    max_steps: int = 32,
    base_url: Optional[str] = None,
) -> Dict[str, Any]:
    """Close all panes as a checklist, optionally quitting terminal at the end."""
    base = _base_url(base_url)
    if max_steps < 1:
        max_steps = 1
    if max_steps > 128:
        max_steps = 128

    steps: List[Dict[str, Any]] = []
    closed = 0
    failed = 0

    for _ in range(max_steps):
        status_json = _status_payload(base)
        panes = _normalize_panes(status_json)
        if not panes:
            steps.append({"task": "Close remaining panes", "status": "done", "checkbox": "☑"})
            break

        target = next((p for p in panes if p.get("focused")), panes[0])
        entry: Dict[str, Any] = {
            "task": f"Close pane {target['pane_id']} ({target.get('title','')})",
            "status": "pending",
            "checkbox": "☐",
        }
        result = _close_pane_call(base, target["pane_id"])
        if result.get("ok"):
            closed += 1
            entry["status"] = "done"
            entry["checkbox"] = "☑"
        else:
            failed += 1
            entry["status"] = "failed"
            entry["checkbox"] = "☒"
            entry["detail"] = result.get("error", "unknown error")
            steps.append(entry)
            break
        steps.append(entry)
    else:
        steps.append({"task": "Close remaining panes", "status": "failed", "checkbox": "☒", "detail": "max_steps reached"})
        failed += 1

    quit_result: Optional[Dict[str, Any]] = None
    if exit_terminal and failed == 0:
        q = terminal_quit(base_url=base)
        quit_result = q
        steps.append(
            {
                "task": "Quit terminal application",
                "status": "done" if q.get("success") else "failed",
                "checkbox": "☑" if q.get("success") else "☒",
                "detail": q.get("error") if not q.get("success") else "",
            }
        )
        if not q.get("success"):
            failed += 1

    return {
        "success": failed == 0,
        "base_url": base,
        "closed_count": closed,
        "failed_count": failed,
        "steps": steps,
        "quit_result": quit_result,
    }


@mcp.tool()
def terminal_screenshot(
    save_path: Optional[str] = None,
    include_base64: bool = False,
    base_url: Optional[str] = None,
) -> Dict[str, Any]:
    """Capture terminal screenshot if /api/screenshot exists."""
    base = _base_url(base_url)
    status, raw, hdr, err = _request("GET", base, "/api/screenshot")
    if err:
        return {"success": False, "error": "connection_failed", "detail": err, "base_url": base}
    if status != 200:
        return _unsupported("terminal_screenshot", base, "/api/screenshot not available")

    content_type = (hdr.get("Content-Type") or hdr.get("content-type") or "").lower()
    png_bytes: Optional[bytes] = None
    payload_json = _parse_json(raw)

    if payload_json:
        # Accept common patterns.
        for key in ("image_base64", "screenshot_base64", "png_base64"):
            if payload_json.get(key):
                try:
                    png_bytes = base64.b64decode(str(payload_json[key]))
                    break
                except Exception:
                    pass
    elif "image/png" in content_type or raw.startswith(b"\x89PNG"):
        png_bytes = raw

    if not png_bytes:
        return {
            "success": False,
            "error": "invalid_screenshot_payload",
            "detail": "Expected PNG bytes or JSON containing image_base64/screenshot_base64/png_base64",
            "base_url": base,
        }

    if save_path:
        out = Path(save_path).expanduser().resolve()
    else:
        SCREENSHOT_DIR.mkdir(parents=True, exist_ok=True)
        stamp = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S-%f")
        out = SCREENSHOT_DIR / f"terminal-shot-{stamp}.png"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_bytes(png_bytes)

    result: Dict[str, Any] = {
        "success": True,
        "base_url": base,
        "output_path": str(out),
        "filename": out.name,
        "size_bytes": len(png_bytes),
    }
    if include_base64:
        result["image_base64"] = base64.b64encode(png_bytes).decode("ascii")
    return result


@mcp.tool()
def console_open(base_url: Optional[str] = None) -> Dict[str, Any]:
    """Open the quake-style agent console (chat + logs)."""
    base = _base_url(base_url)
    status, raw, _, err = _request("POST", base, "/api/console/open")
    if err:
        return {"success": False, "error": "connection_failed", "detail": err, "base_url": base}
    if status == 200:
        return {"success": True, "base_url": base, "result": _parse_json(raw) or raw.decode("utf-8", errors="replace")}
    return _unsupported("console_open", base, "/api/console/open not available")


@mcp.tool()
def console_close(base_url: Optional[str] = None) -> Dict[str, Any]:
    """Close the quake-style agent console."""
    base = _base_url(base_url)
    status, raw, _, err = _request("POST", base, "/api/console/close")
    if err:
        return {"success": False, "error": "connection_failed", "detail": err, "base_url": base}
    if status == 200:
        return {"success": True, "base_url": base, "result": _parse_json(raw) or raw.decode("utf-8", errors="replace")}
    return _unsupported("console_close", base, "/api/console/close not available")


@mcp.tool()
def console_toggle(base_url: Optional[str] = None) -> Dict[str, Any]:
    """Toggle the quake-style agent console open/closed."""
    base = _base_url(base_url)
    status, raw, _, err = _request("POST", base, "/api/console/toggle")
    if err:
        return {"success": False, "error": "connection_failed", "detail": err, "base_url": base}
    if status == 200:
        return {"success": True, "base_url": base, "result": _parse_json(raw) or raw.decode("utf-8", errors="replace")}
    return _unsupported("console_toggle", base, "/api/console/toggle not available")


@mcp.tool()
def console_chat(message: str, base_url: Optional[str] = None) -> Dict[str, Any]:
    """Send a chat message to the embedded agent in the console."""
    base = _base_url(base_url)
    if not message:
        return {"success": False, "error": "message is required"}
    status, raw, _, err = _request("POST", base, "/api/console/chat", json_payload={"message": message})
    if err:
        return {"success": False, "error": "connection_failed", "detail": err, "base_url": base}
    if status == 200:
        return {"success": True, "base_url": base, "result": _parse_json(raw) or raw.decode("utf-8", errors="replace")}
    return _unsupported("console_chat", base, "/api/console/chat not available")


@mcp.tool()
def console_logs(last_n: int = 50, base_url: Optional[str] = None) -> Dict[str, Any]:
    """Read recent lines from the console log buffer."""
    base = _base_url(base_url)
    if last_n < 1:
        last_n = 1
    if last_n > 5000:
        last_n = 5000
    path = f"/api/console/logs?last={last_n}"
    status, raw, _, err = _request("GET", base, path)
    if err:
        return {"success": False, "error": "connection_failed", "detail": err, "base_url": base}
    if status == 200:
        return {"success": True, "base_url": base, "logs": _parse_json(raw) or raw.decode("utf-8", errors="replace")}
    return _unsupported("console_logs", base, "/api/console/logs not available")


@mcp.tool()
def console_messages(base_url: Optional[str] = None) -> Dict[str, Any]:
    """Read the chat message history from the console."""
    base = _base_url(base_url)
    status, raw, _, err = _request("GET", base, "/api/console/messages")
    if err:
        return {"success": False, "error": "connection_failed", "detail": err, "base_url": base}
    if status == 200:
        return {"success": True, "base_url": base, "messages": _parse_json(raw) or raw.decode("utf-8", errors="replace")}
    return _unsupported("console_messages", base, "/api/console/messages not available")


@mcp.tool()
def console_status(base_url: Optional[str] = None) -> Dict[str, Any]:
    """Get console open/closed state and message count."""
    base = _base_url(base_url)
    status, raw, _, err = _request("GET", base, "/api/console/status")
    if err:
        return {"success": False, "error": "connection_failed", "detail": err, "base_url": base}
    if status == 200:
        return {"success": True, "base_url": base, "status": _parse_json(raw) or raw.decode("utf-8", errors="replace")}
    return _unsupported("console_status", base, "/api/console/status not available")


if __name__ == "__main__":
    mcp.run(transport="stdio")
