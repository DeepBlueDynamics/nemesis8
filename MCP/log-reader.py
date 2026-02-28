#!/usr/bin/env python3
"""
Log Reader MCP Server
=====================

Provides tools to fetch Stickys logs via Control API (/logs) or fall back to reading
local log files under sticky/logs/ with simple level filtering.
"""

import os
import json
import glob
from datetime import datetime
from typing import Any, Dict, List, Optional

from mcp.server.fastmcp import FastMCP

try:
    import aiohttp
except Exception:
    aiohttp = None  # Optional; fallback to file mode

mcp = FastMCP("log-reader")

CONTROL_URL = os.environ.get("STICKY_CONTROL_URL", "http://127.0.0.1:8765")
CONTROL_TOKEN = os.environ.get("STICKY_CONTROL_TOKEN", "").strip()
LOG_DIR = os.environ.get("STICKY_LOG_DIR", os.path.join("sticky", "logs"))


@mcp.tool()
async def logs_status() -> Dict[str, Any]:
    """Report configured control URL and log directory; best-effort health ping."""
    ok = False
    err = None
    if aiohttp:
        try:
            headers = {"Authorization": f"Bearer {CONTROL_TOKEN}"} if CONTROL_TOKEN else {}
            async with aiohttp.ClientSession() as session:
                async with session.get(f"{CONTROL_URL}/logs?tail=1", headers=headers, timeout=2) as resp:
                    ok = resp.status == 200
        except Exception as e:
            err = str(e)
    return {
        "success": True,
        "control_url": CONTROL_URL,
        "log_dir": LOG_DIR,
        "http_available": bool(aiohttp),
        "control_ok": ok,
        "error": err,
    }


@mcp.tool()
async def logs_tail(tail: int = 200, level: Optional[str] = None, since: Optional[str] = None) -> Dict[str, Any]:
    """Tail recent logs (via Control API if available, else fallback to files)."""
    entries: List[Dict[str, Any]] = []
    if aiohttp:
        try:
            headers = {"Authorization": f"Bearer {CONTROL_TOKEN}"} if CONTROL_TOKEN else {}
            qs = f"tail={int(tail)}"
            if level:
                qs += f"&level={level}"
            if since:
                qs += f"&since={since}"
            async with aiohttp.ClientSession() as session:
                async with session.get(f"{CONTROL_URL}/logs?{qs}", headers=headers, timeout=3) as resp:
                    data = await resp.json()
                    if data.get("success") and isinstance(data.get("entries"), list):
                        entries = data["entries"]
                        return {"success": True, "count": len(entries), "entries": entries}
        except Exception:
            pass
    # Fallback: read files
    files = sorted(glob.glob(os.path.join(LOG_DIR, "*.txt")), key=lambda p: os.path.getmtime(p), reverse=True)
    def _map_level(line: str) -> str:
        if "❌" in line or " ERROR" in line:
            return "error"
        if "⚠️" in line or " WARN" in line:
            return "warning"
        if "✅" in line or " INFO" in line:
            return "info"
        if "🛈" in line or " DEBUG" in line:
            return "debug"
        return "info"
    remaining = int(tail)
    for path in files:
        try:
            with open(path, "r", encoding="utf-8", errors="ignore") as f:
                lines = f.readlines()
            for ln in reversed(lines):
                lvl = _map_level(ln)
                if level and lvl != level:
                    continue
                entries.append({"ts": "", "level": lvl, "msg": ln.rstrip()})
                remaining -= 1
                if remaining <= 0:
                    break
            if remaining <= 0:
                break
        except Exception:
            continue
    entries = entries[-int(tail):]
    return {"success": True, "count": len(entries), "entries": entries}


@mcp.tool()
async def logs_list_files() -> Dict[str, Any]:
    """List log files in the Stickys logs directory (mtime desc)."""
    files = []
    for p in sorted(glob.glob(os.path.join(LOG_DIR, "*.txt")), key=lambda p: os.path.getmtime(p), reverse=True):
        try:
            st = os.stat(p)
            files.append({"path": p, "bytes": st.st_size, "mtime": datetime.fromtimestamp(st.st_mtime).isoformat()})
        except Exception:
            pass
    return {"success": True, "count": len(files), "files": files}


@mcp.tool()
async def logs_read_file(path: str, tail: int = 200, level: Optional[str] = None) -> Dict[str, Any]:
    """Read a single log file with optional tail/level filter."""
    try:
        with open(path, "r", encoding="utf-8", errors="ignore") as f:
            lines = f.readlines()
    except Exception as e:
        return {"success": False, "error": str(e)}

    def _map_level(line: str) -> str:
        if "❌" in line or " ERROR" in line:
            return "error"
        if "⚠️" in line or " WARN" in line:
            return "warning"
        if "✅" in line or " INFO" in line:
            return "info"
        if "🛈" in line or " DEBUG" in line:
            return "debug"
        return "info"

    out: List[Dict[str, Any]] = []
    for ln in reversed(lines):
        lvl = _map_level(ln)
        if level and lvl != level:
            continue
        out.append({"ts": "", "level": lvl, "msg": ln.rstrip()})
        if len(out) >= tail:
            break
    out = list(reversed(out))
    return {"success": True, "count": len(out), "entries": out}


if __name__ == "__main__":
    mcp.run(transport="stdio")

