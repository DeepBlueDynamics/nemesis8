#!/usr/bin/env python3
"""MCP tool to request a self-restart of the current container."""

from __future__ import annotations

import json
import os
import signal
import time
from typing import Dict

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("container-restart")


def _marker_path() -> str:
    root = os.environ.get("CODEX_WORKSPACE_ROOT", "/workspace")
    return os.path.join(root, ".codex-self-restart.json")


@mcp.tool()
async def request_self_restart(reason: str = "mcp requested", delay_seconds: float = 0.0) -> Dict[str, str]:
    """Request a self-restart by writing a marker file then stopping PID 1.

    Args:
        reason: Short reason for the restart request.
        delay_seconds: Optional delay before stopping PID 1.
    """
    marker = _marker_path()
    payload = {
        "reason": reason,
        "requested_at": time.time(),
    }
    os.makedirs(os.path.dirname(marker), exist_ok=True)
    with open(marker, "w", encoding="utf-8") as handle:
        json.dump(payload, handle)
        handle.flush()
        os.fsync(handle.fileno())
    if delay_seconds and delay_seconds > 0:
        time.sleep(delay_seconds)
    os.kill(1, signal.SIGTERM)
    return {"status": "restart_requested", "marker": marker}


if __name__ == "__main__":
    mcp.run()
