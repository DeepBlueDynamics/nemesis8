#!/usr/bin/env python3
"""Service Engine (MCP)
=====================

Calls the host-side service-engine HTTP server to start/stop services.

Env:
- SERVICE_ENGINE_URL (default http://host.docker.internal:8098)
- SERVICE_ENGINE_TIMEOUT (seconds; default 8)
"""

from __future__ import annotations

import json
import os
import urllib.parse
import urllib.request
from typing import Any, Dict, Optional
from urllib.error import HTTPError, URLError

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("service-engine")

BASE_URL = os.environ.get("SERVICE_ENGINE_URL", "http://host.docker.internal:8098").rstrip("/")
TIMEOUT = float(os.environ.get("SERVICE_ENGINE_TIMEOUT", "8"))


def _result(success: bool, **kwargs: Any) -> Dict[str, Any]:
    data = {"ok": success, "success": success}
    data.update(kwargs)
    return data


def _request(path: str, payload: Optional[Dict[str, Any]] = None, method: str = "POST") -> Dict[str, Any]:
    url = BASE_URL + path
    data = None
    headers = {"Content-Type": "application/json"}
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
            raw = resp.read().decode("utf-8")
            if not raw:
                return _result(True)
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


@mcp.tool()
def service_engine_health() -> Dict[str, Any]:
    """Check service engine health."""
    return _request("/health", method="GET")


@mcp.tool()
def service_engine_services() -> Dict[str, Any]:
    """List available services."""
    return _request("/services", method="GET")


@mcp.tool()
def service_engine_start(name: str) -> Dict[str, Any]:
    """Start a service by name."""
    return _request("/service/run", payload={"name": name})


@mcp.tool()
def service_engine_stop(name: str) -> Dict[str, Any]:
    """Stop a service by name."""
    return _request("/service/stop", payload={"name": name})


@mcp.tool()
def service_engine_restart(name: str) -> Dict[str, Any]:
    """Restart a service by name."""
    return _request("/service/restart", payload={"name": name})


@mcp.tool()
def service_engine_logs(name: str) -> Dict[str, Any]:
    """Fetch service logs (tail) by name."""
    return _request("/service/logs", payload={"name": name})


if __name__ == "__main__":
    mcp.run()
