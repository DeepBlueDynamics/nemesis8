#!/usr/bin/env python3
"""MCP: streamdeck-server

Stream Deck Plus MCP server (stdio JSON-RPC) with a companion HTTP status surface.

Server name: streamdeck-server
Transport: stdio
HTTP port: 9850

Tools:
- get_device_info
- set_brightness
- set_button_image
- set_button_color
- clear_button
- set_touchstrip_image
- set_touchstrip_color
- screenshot
- reset

Resource:
- streamdeck://device/status
"""

from __future__ import annotations

import base64
import json
import os
import random
import re
import sys
import threading
import time
import urllib.parse
import urllib.request
import urllib.error
import zlib
from pathlib import Path
from dataclasses import dataclass, field
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Dict, List, Optional, Tuple

try:
    from mcp.server.fastmcp import FastMCP  # type: ignore
except Exception:
    # Local compatibility fallback so this file can import/compile without MCP runtime.
    class FastMCP:  # pragma: no cover
        def __init__(self, _name: str):
            self.name = _name

        def tool(self, *args: Any, **kwargs: Any):
            def _decorator(fn: Any) -> Any:
                return fn

            return _decorator

        def resource(self, *args: Any, **kwargs: Any):
            def _decorator(fn: Any) -> Any:
                return fn

            return _decorator

        def run(self, *args: Any, **kwargs: Any) -> None:
            raise RuntimeError("FastMCP runtime not available")


mcp = FastMCP("streamdeck-server")

HTTP_HOST = os.environ.get("STREAMDECK_HTTP_HOST", "127.0.0.1")
HTTP_PORT = int(os.environ.get("STREAMDECK_HTTP_PORT", "9850"))
SCREENSHOT_DIR = Path(
    os.environ.get("STREAMDECK_SCREENSHOT_DIR", "/workspace/codex-container/outputs/streamdeck-screenshots")
)
REMOTE_BASE_URL = os.environ.get("STREAMDECK_DEVICE_BASE_URL", "http://host.docker.internal:9850").rstrip("/")
REMOTE_TIMEOUT_SEC = float(os.environ.get("STREAMDECK_REMOTE_TIMEOUT_SEC", "3"))
REMOTE_MODE = os.environ.get("STREAMDECK_REMOTE_MODE", "1").strip().lower() not in {"0", "false", "no", "off"}


def _now_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


def _u8(name: str, value: int, minimum: int = 0, maximum: int = 255) -> Tuple[bool, Optional[str]]:
    if not isinstance(value, int):
        return False, f"{name} must be an integer"
    if value < minimum or value > maximum:
        return False, f"{name} must be in range {minimum}..{maximum}"
    return True, None


def _decode_base64_image(value: str) -> Tuple[Optional[bytes], Optional[str]]:
    if not value or not isinstance(value, str):
        return None, "image_base64 must be a non-empty base64 string"

    raw = value.strip()
    if raw.startswith("data:"):
        m = re.match(r"^data:[^;]+;base64,(.*)$", raw, flags=re.IGNORECASE | re.DOTALL)
        if not m:
            return None, "Invalid data URI format for image_base64"
        raw = m.group(1)

    try:
        decoded = base64.b64decode(raw, validate=True)
    except Exception:
        return None, "image_base64 is not valid base64"

    if not decoded:
        return None, "Decoded image is empty"

    return decoded, None


def _read_image_file(image_path: str) -> Tuple[Optional[bytes], Optional[str], Optional[str]]:
    if not image_path or not isinstance(image_path, str):
        return None, "image_path must be a non-empty string", None

    try:
        resolved = Path(image_path).expanduser().resolve()
    except Exception:
        return None, "image_path is invalid", None

    if not resolved.exists():
        return None, f"image_path does not exist: {resolved}", None
    if not resolved.is_file():
        return None, f"image_path is not a file: {resolved}", None

    try:
        data = resolved.read_bytes()
    except Exception as exc:
        return None, f"failed to read image_path: {exc}", None
    if not data:
        return None, f"image file is empty: {resolved}", None

    return data, None, str(resolved)


def _load_image_bytes(image_base64: Optional[str], image_path: Optional[str]) -> Tuple[Optional[bytes], Optional[str], Optional[str]]:
    if image_path:
        data, err, resolved = _read_image_file(image_path)
        if err:
            return None, err, None
        return data, None, resolved
    if image_base64:
        data, err = _decode_base64_image(image_base64)
        if err:
            return None, err, None
        return data, None, "base64"
    return None, "Provide either image_base64 or image_path", None


def _center_trim_resize_image(raw: bytes, width: int, height: int) -> Tuple[Optional[bytes], Optional[str]]:
    """Center-crop to target aspect ratio and resize to exact target dimensions."""
    try:
        from PIL import Image  # type: ignore
        from io import BytesIO

        with Image.open(BytesIO(raw)) as img:
            img = img.convert("RGB")
            src_w, src_h = img.size
            target_ratio = width / float(height)
            src_ratio = src_w / float(src_h) if src_h else target_ratio

            if src_ratio > target_ratio:
                crop_w = int(src_h * target_ratio)
                crop_h = src_h
                left = max(0, (src_w - crop_w) // 2)
                top = 0
            else:
                crop_w = src_w
                crop_h = int(src_w / target_ratio) if target_ratio else src_h
                left = 0
                top = max(0, (src_h - crop_h) // 2)

            img = img.crop((left, top, left + crop_w, top + crop_h))
            if hasattr(Image, "Resampling"):
                resample = Image.Resampling.LANCZOS
            else:
                resample = Image.LANCZOS
            img = img.resize((width, height), resample=resample)
            out = BytesIO()
            img.save(out, format="PNG")
            return out.getvalue(), None
    except Exception as exc:
        return None, f"image processing failed (requires Pillow): {exc}"


def _chunk(tag: bytes, data: bytes) -> bytes:
    crc = zlib.crc32(tag)
    crc = zlib.crc32(data, crc) & 0xFFFFFFFF
    return len(data).to_bytes(4, "big") + tag + data + crc.to_bytes(4, "big")


def _png_rgb(width: int, height: int, rgb: bytearray) -> bytes:
    scanlines = bytearray()
    row_size = width * 3
    for y in range(height):
        scanlines.append(0)  # filter type 0
        start = y * row_size
        scanlines.extend(rgb[start : start + row_size])

    ihdr = (
        width.to_bytes(4, "big")
        + height.to_bytes(4, "big")
        + b"\x08"  # bit depth
        + b"\x02"  # color type RGB
        + b"\x00\x00\x00"
    )
    idat = zlib.compress(bytes(scanlines), level=6)

    return b"\x89PNG\r\n\x1a\n" + _chunk(b"IHDR", ihdr) + _chunk(b"IDAT", idat) + _chunk(b"IEND", b"")


def _clamp(v: int) -> int:
    return 0 if v < 0 else 255 if v > 255 else v


def _fill_rect(rgb: bytearray, w: int, h: int, x: int, y: int, rw: int, rh: int, color: Tuple[int, int, int]) -> None:
    x0 = max(0, x)
    y0 = max(0, y)
    x1 = min(w, x + rw)
    y1 = min(h, y + rh)
    if x0 >= x1 or y0 >= y1:
        return

    r, g, b = color
    for yy in range(y0, y1):
        base = yy * w * 3
        for xx in range(x0, x1):
            i = base + xx * 3
            rgb[i] = r
            rgb[i + 1] = g
            rgb[i + 2] = b


def _draw_rect_outline(
    rgb: bytearray,
    w: int,
    h: int,
    x: int,
    y: int,
    rw: int,
    rh: int,
    color: Tuple[int, int, int],
    thickness: int = 2,
) -> None:
    for t in range(thickness):
        _fill_rect(rgb, w, h, x + t, y + t, rw - 2 * t, 1, color)
        _fill_rect(rgb, w, h, x + t, y + rh - 1 - t, rw - 2 * t, 1, color)
        _fill_rect(rgb, w, h, x + t, y + t, 1, rh - 2 * t, color)
        _fill_rect(rgb, w, h, x + rw - 1 - t, y + t, 1, rh - 2 * t, color)


def _draw_circle(rgb: bytearray, w: int, h: int, cx: int, cy: int, radius: int, color: Tuple[int, int, int]) -> None:
    rr = radius * radius
    r, g, b = color
    x0 = max(0, cx - radius)
    x1 = min(w - 1, cx + radius)
    y0 = max(0, cy - radius)
    y1 = min(h - 1, cy + radius)
    for y in range(y0, y1 + 1):
        dy = y - cy
        for x in range(x0, x1 + 1):
            dx = x - cx
            if dx * dx + dy * dy <= rr:
                idx = (y * w + x) * 3
                rgb[idx] = r
                rgb[idx + 1] = g
                rgb[idx + 2] = b


@dataclass
class ButtonState:
    mode: str = "clear"  # clear | color | image
    color: Tuple[int, int, int] = (0, 0, 0)
    image_base64: Optional[str] = None
    updated_at: str = field(default_factory=_now_iso)


@dataclass
class TouchstripState:
    mode: str = "clear"  # clear | color | image
    color: Tuple[int, int, int] = (0, 0, 0)
    image_base64: Optional[str] = None
    updated_at: str = field(default_factory=_now_iso)


class StreamDeckState:
    def __init__(self) -> None:
        self.lock = threading.RLock()
        self.device = {
            "name": "Elgato Stream Deck Plus",
            "model": "Stream Deck Plus",
            "vid": "0x0FD9",
            "pid": "0x0084",
            "buttons": 8,
            "button_size": {"width": 120, "height": 120},
            "encoders": 4,
            "touchstrip": {"width": 800, "height": 100},
        }
        self.brightness = 100
        self.buttons: List[ButtonState] = [ButtonState() for _ in range(8)]
        self.touchstrip = TouchstripState()
        self.encoder_positions = [0, 0, 0, 0]
        self.encoder_pressed = [False, False, False, False]
        self.touch = {
            "last_event": None,
            "x": None,
            "gesture": None,
            "updated_at": _now_iso(),
        }
        self.last_reset_at = _now_iso()

    def reset(self) -> None:
        with self.lock:
            self.brightness = 100
            self.buttons = [ButtonState() for _ in range(8)]
            self.touchstrip = TouchstripState()
            self.encoder_positions = [0, 0, 0, 0]
            self.encoder_pressed = [False, False, False, False]
            self.touch = {
                "last_event": None,
                "x": None,
                "gesture": None,
                "updated_at": _now_iso(),
            }
            self.last_reset_at = _now_iso()

    def snapshot(self) -> Dict[str, Any]:
        with self.lock:
            return {
                "server": {
                    "name": "streamdeck-server",
                    "transport": "stdio",
                    "http": {"host": HTTP_HOST, "port": HTTP_PORT},
                    "timestamp": _now_iso(),
                },
                "device": self.device,
                "brightness": self.brightness,
                "buttons": [
                    {
                        "index": i,
                        "mode": b.mode,
                        "color": {"r": b.color[0], "g": b.color[1], "b": b.color[2]},
                        "has_image": bool(b.image_base64),
                        "updated_at": b.updated_at,
                    }
                    for i, b in enumerate(self.buttons)
                ],
                "encoders": [
                    {
                        "index": i,
                        "position": self.encoder_positions[i],
                        "pressed": self.encoder_pressed[i],
                    }
                    for i in range(4)
                ],
                "touchstrip": {
                    "mode": self.touchstrip.mode,
                    "color": {
                        "r": self.touchstrip.color[0],
                        "g": self.touchstrip.color[1],
                        "b": self.touchstrip.color[2],
                    },
                    "has_image": bool(self.touchstrip.image_base64),
                    "updated_at": self.touchstrip.updated_at,
                },
                "touch": self.touch,
                "last_reset_at": self.last_reset_at,
            }


STATE = StreamDeckState()


def _composite_png() -> bytes:
    snap = STATE.snapshot()

    width, height = 800, 500
    rgb = bytearray([18, 18, 20] * (width * height))

    # Buttons: 2 rows x 4 cols, 120x120 each.
    bx0, by0 = 110, 20
    bsize, gap = 120, 20

    for b in snap["buttons"]:
        idx = int(b["index"])
        row = idx // 4
        col = idx % 4
        x = bx0 + col * (bsize + gap)
        y = by0 + row * (bsize + gap)

        mode = b["mode"]
        if mode == "color":
            c = b["color"]
            fill = (int(c["r"]), int(c["g"]), int(c["b"]))
        elif mode == "image":
            fill = (35, 105, 210)
        else:
            fill = (40, 40, 44)

        _fill_rect(rgb, width, height, x, y, bsize, bsize, fill)
        _draw_rect_outline(rgb, width, height, x, y, bsize, bsize, (220, 220, 230), thickness=2)

    # Knobs visualization.
    knob_y = 325
    for i, enc in enumerate(snap["encoders"]):
        cx = bx0 + 60 + i * (bsize + gap)
        pos = int(enc["position"])
        intensity = _clamp(90 + abs(pos) % 140)
        color = (intensity, intensity, 180)
        _draw_circle(rgb, width, height, cx, knob_y, 28, color)
        _draw_circle(rgb, width, height, cx, knob_y, 10, (25, 25, 30))

    # Touchstrip 800x100
    ts = snap["touchstrip"]
    if ts["mode"] == "color":
        tc = ts["color"]
        ts_color = (int(tc["r"]), int(tc["g"]), int(tc["b"]))
    elif ts["mode"] == "image":
        ts_color = (58, 129, 92)
    else:
        ts_color = (30, 30, 32)

    _fill_rect(rgb, width, height, 0, 390, 800, 100, ts_color)
    _draw_rect_outline(rgb, width, height, 0, 390, 800, 100, (200, 200, 210), thickness=2)

    # Brightness overlay bar.
    bright = int(snap["brightness"])
    bar_w = int((bright / 100.0) * 760)
    _fill_rect(rgb, width, height, 20, 365, 760, 10, (50, 50, 50))
    _fill_rect(rgb, width, height, 20, 365, bar_w, 10, (230, 190, 60))

    return _png_rgb(width, height, rgb)


def _write_screenshot_file(png: bytes) -> Tuple[Optional[Path], Optional[str]]:
    try:
        SCREENSHOT_DIR.mkdir(parents=True, exist_ok=True)
        stamp = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S-%f")
        out = SCREENSHOT_DIR / f"streamdeck-screenshot-{stamp}.png"
        out.write_bytes(png)
        return out, None
    except Exception as exc:
        return None, str(exc)


def _remote_get(path: str) -> Tuple[Optional[bytes], Optional[str]]:
    url = f"{REMOTE_BASE_URL}{path}"
    req = urllib.request.Request(url, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=REMOTE_TIMEOUT_SEC) as resp:
            return resp.read(), None
    except urllib.error.HTTPError as exc:
        return None, f"HTTP {exc.code} from {url}"
    except Exception as exc:
        return None, f"{type(exc).__name__}: {exc}"


def _remote_get_json(path: str) -> Tuple[Optional[Dict[str, Any]], Optional[str]]:
    raw, err = _remote_get(path)
    if err:
        return None, err
    assert raw is not None
    try:
        parsed = json.loads(raw.decode("utf-8", errors="replace"))
        if isinstance(parsed, dict):
            return parsed, None
        return None, "Remote JSON payload was not an object"
    except Exception as exc:
        return None, f"Invalid JSON from remote endpoint: {exc}"


def _unsupported_remote_action(action: str, why: str) -> Dict[str, Any]:
    return {
        "success": False,
        "error": "unsupported_by_host_http_api",
        "action": action,
        "detail": why,
        "remote_base_url": REMOTE_BASE_URL,
        "next_steps": [
            "Add matching POST endpoints on host streamdeck service (e.g. /touchstrip/image, /touchstrip/color, /brightness)",
            "Or run MCP stdio streamdeck-server directly attached to a client",
        ],
    }


@mcp.tool()
def get_device_info() -> Dict[str, Any]:
    """Return Stream Deck device info and full current state."""
    if REMOTE_MODE:
        status, err = _remote_get_json("/status")
        if err:
            return {
                "success": False,
                "error": "remote_status_unavailable",
                "detail": err,
                "remote_base_url": REMOTE_BASE_URL,
                "next_steps": ["Verify host service on port 9850", "Check /health and /status on host"],
            }
        return {
            "success": True,
            "remote": True,
            "remote_base_url": REMOTE_BASE_URL,
            "state": status,
        }
    return {"success": True, "state": STATE.snapshot()}


@mcp.tool()
def set_brightness(percent: int) -> Dict[str, Any]:
    """Set display brightness percentage (0-100)."""
    ok, err = _u8("percent", percent, 0, 100)
    if not ok:
        return {"success": False, "error": err}
    if REMOTE_MODE:
        return _unsupported_remote_action(
            action="set_brightness",
            why="Host HTTP API does not expose a brightness write endpoint.",
        )

    with STATE.lock:
        STATE.brightness = percent

    return {"success": True, "brightness": percent}


@mcp.tool()
def set_button_image(key: int, image_base64: Optional[str] = None, image_path: Optional[str] = None) -> Dict[str, Any]:
    """Set button image from base64 or file path; center-trim/resize to 120x120."""
    ok, err = _u8("key", key, 0, 7)
    if not ok:
        return {"success": False, "error": err}
    if REMOTE_MODE:
        return _unsupported_remote_action(
            action="set_button_image",
            why="Host HTTP API does not expose a button image write endpoint.",
        )

    decoded, load_err, source = _load_image_bytes(image_base64=image_base64, image_path=image_path)
    if load_err:
        return {"success": False, "error": load_err}

    assert decoded is not None
    resized, process_err = _center_trim_resize_image(decoded, 120, 120)
    if process_err or resized is None:
        return {
            "success": False,
            "error": process_err or "image processing failed",
            "next_steps": [
                "Install Pillow in the MCP runtime (python -m pip install pillow)",
                "Retry set_button_image()",
            ],
        }
    encoded = base64.b64encode(resized).decode("ascii")

    with STATE.lock:
        btn = STATE.buttons[key]
        btn.mode = "image"
        btn.image_base64 = encoded
        btn.updated_at = _now_iso()

    out: Dict[str, Any] = {
        "success": True,
        "key": key,
        "mode": "image",
        "size_bytes": len(resized),
        "source": source,
    }
    return out


@mcp.tool()
def set_button_color(key: int, r: int, g: int, b: int) -> Dict[str, Any]:
    """Set a button to a solid RGB color."""
    for name, value in (("key", key), ("r", r), ("g", g), ("b", b)):
        maxv = 7 if name == "key" else 255
        ok, err = _u8(name, value, 0, maxv)
        if not ok:
            return {"success": False, "error": err}
    if REMOTE_MODE:
        raw, remote_err = _remote_get(f"/test/button/{key}")
        if remote_err:
            return {
                "success": False,
                "error": "remote_test_button_failed",
                "detail": remote_err,
                "remote_base_url": REMOTE_BASE_URL,
            }
        msg = (raw or b"").decode("utf-8", errors="replace").strip()
        return {
            "success": True,
            "remote": True,
            "action": "test_button_route",
            "key": key,
            "requested_color": {"r": r, "g": g, "b": b},
            "warning": "Host test route ignores requested RGB and applies service-defined test color.",
            "detail": msg,
        }

    with STATE.lock:
        btn = STATE.buttons[key]
        btn.mode = "color"
        btn.color = (r, g, b)
        btn.image_base64 = None
        btn.updated_at = _now_iso()

    return {"success": True, "key": key, "mode": "color", "color": {"r": r, "g": g, "b": b}}


@mcp.tool()
def clear_button(key: Optional[int] = None) -> Dict[str, Any]:
    """Clear one button (0-7) or all buttons when key is omitted."""
    if REMOTE_MODE:
        return _unsupported_remote_action(
            action="clear_button",
            why="Host HTTP API does not expose a clear button endpoint.",
        )
    with STATE.lock:
        if key is None:
            for btn in STATE.buttons:
                btn.mode = "clear"
                btn.color = (0, 0, 0)
                btn.image_base64 = None
                btn.updated_at = _now_iso()
            return {"success": True, "cleared": "all"}

        ok, err = _u8("key", key, 0, 7)
        if not ok:
            return {"success": False, "error": err}

        btn = STATE.buttons[key]
        btn.mode = "clear"
        btn.color = (0, 0, 0)
        btn.image_base64 = None
        btn.updated_at = _now_iso()
        return {"success": True, "cleared": key}


@mcp.tool()
def set_touchstrip_image(image_base64: Optional[str] = None, image_path: Optional[str] = None) -> Dict[str, Any]:
    """Set touchstrip image from base64 or file path; center-trim/resize to 800x100."""
    if REMOTE_MODE:
        return _unsupported_remote_action(
            action="set_touchstrip_image",
            why="Host HTTP API does not expose /touchstrip/image.",
        )
    decoded, load_err, source = _load_image_bytes(image_base64=image_base64, image_path=image_path)
    if load_err:
        return {"success": False, "error": load_err}

    assert decoded is not None
    resized, process_err = _center_trim_resize_image(decoded, 800, 100)
    if process_err or resized is None:
        return {
            "success": False,
            "error": process_err or "image processing failed",
            "next_steps": [
                "Install Pillow in the MCP runtime (python -m pip install pillow)",
                "Retry set_touchstrip_image()",
            ],
        }
    encoded = base64.b64encode(resized).decode("ascii")

    with STATE.lock:
        STATE.touchstrip.mode = "image"
        STATE.touchstrip.image_base64 = encoded
        STATE.touchstrip.updated_at = _now_iso()

    out: Dict[str, Any] = {
        "success": True,
        "mode": "image",
        "size_bytes": len(resized),
        "source": source,
    }
    return out


@mcp.tool()
def set_touchstrip_color(r: int, g: int, b: int) -> Dict[str, Any]:
    """Set touchstrip to a solid RGB color."""
    for name, value in (("r", r), ("g", g), ("b", b)):
        ok, err = _u8(name, value, 0, 255)
        if not ok:
            return {"success": False, "error": err}
    if REMOTE_MODE:
        return _unsupported_remote_action(
            action="set_touchstrip_color",
            why="Host HTTP API does not expose /touchstrip/color.",
        )

    with STATE.lock:
        STATE.touchstrip.mode = "color"
        STATE.touchstrip.color = (r, g, b)
        STATE.touchstrip.image_base64 = None
        STATE.touchstrip.updated_at = _now_iso()

    return {"success": True, "mode": "color", "color": {"r": r, "g": g, "b": b}}


@mcp.tool()
def screenshot() -> Dict[str, Any]:
    """Save a composite PNG screenshot locally and return only metadata (no raw image bytes)."""
    if REMOTE_MODE:
        png, remote_err = _remote_get("/screenshot.png")
        if remote_err or png is None:
            return {
                "success": False,
                "error": "remote_screenshot_unavailable",
                "detail": remote_err or "no data",
                "remote_base_url": REMOTE_BASE_URL,
                "next_steps": ["Verify host /screenshot.png endpoint", "Check streamdeck service logs on host"],
            }
    else:
        png = _composite_png()
    out_path, err = _write_screenshot_file(png)
    if err or out_path is None:
        return {
            "success": False,
            "error": "Failed to write screenshot file",
            "detail": err,
            "next_steps": ["Check STREAMDECK_SCREENSHOT_DIR path permissions", "Retry screenshot()"],
        }

    return {
        "success": True,
        "mime_type": "image/png",
        "output_path": str(out_path),
        "filename": out_path.name,
        "size_bytes": len(png),
        "width": 800,
        "height": 500,
        "remote": REMOTE_MODE,
        "remote_base_url": REMOTE_BASE_URL if REMOTE_MODE else None,
        "note": "Raw image data intentionally omitted from MCP response.",
    }


@mcp.tool()
def reset() -> Dict[str, Any]:
    """Factory reset: clear all buttons/touchstrip and reset state."""
    if REMOTE_MODE:
        return _unsupported_remote_action(
            action="reset",
            why="Host HTTP API does not expose a reset endpoint.",
        )
    STATE.reset()
    return {"success": True, "reset_at": STATE.last_reset_at}


if hasattr(mcp, "resource"):

    @mcp.resource("streamdeck://device/status")
    def streamdeck_status_resource() -> str:
        """JSON status resource for streamdeck://device/status."""
        return json.dumps(STATE.snapshot(), ensure_ascii=False)


class _HttpHandler(BaseHTTPRequestHandler):
    server_version = "streamdeck-server/0.1"

    def _write_json(self, code: int, payload: Dict[str, Any]) -> None:
        data = json.dumps(payload).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _write_text(self, code: int, text: str) -> None:
        data = text.encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self) -> None:  # noqa: N802
        path = urllib.parse.urlparse(self.path).path

        if path == "/health":
            self._write_text(200, "ok")
            return

        if path == "/status":
            self._write_json(200, STATE.snapshot())
            return

        if path == "/screenshot.png":
            png = _composite_png()
            self.send_response(200)
            self.send_header("Content-Type", "image/png")
            self.send_header("Content-Length", str(len(png)))
            self.end_headers()
            self.wfile.write(png)
            return

        m = re.match(r"^/test/button/([0-7])$", path)
        if m:
            key = int(m.group(1))
            color = (
                (key * 41 + random.randint(30, 90)) % 256,
                (key * 73 + random.randint(60, 120)) % 256,
                (key * 29 + random.randint(100, 180)) % 256,
            )
            with STATE.lock:
                btn = STATE.buttons[key]
                btn.mode = "color"
                btn.color = color
                btn.image_base64 = None
                btn.updated_at = _now_iso()
            self._write_json(200, {"success": True, "key": key, "color": {"r": color[0], "g": color[1], "b": color[2]}})
            return

        self._write_json(404, {"success": False, "error": "not_found", "path": path})

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A003
        return


def _start_http_server() -> Optional[ThreadingHTTPServer]:
    try:
        server = ThreadingHTTPServer((HTTP_HOST, HTTP_PORT), _HttpHandler)
    except OSError as exc:
        print(f"[streamdeck-server] HTTP disabled: {exc}", file=sys.stderr, flush=True)
        return None

    t = threading.Thread(target=server.serve_forever, daemon=True, name="streamdeck-http")
    t.start()
    print(f"[streamdeck-server] HTTP listening on http://{HTTP_HOST}:{HTTP_PORT}", file=sys.stderr, flush=True)
    return server


if __name__ == "__main__":
    _start_http_server()
    mcp.run(transport="stdio")
