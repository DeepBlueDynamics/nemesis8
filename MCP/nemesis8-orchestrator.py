#!/usr/bin/env python3
"""MCP: nemesis8-orchestrator

Orchestrator surface for the n8 TUI and chat agent. Lets a connected agent
inspect and drive the gateway: check its status, manage scheduled triggers, and
manage the workspace's MCP tools list.

All gateway operations are HTTP calls to ${GATEWAY_URL} (defaults to
http://host.docker.internal:40008). The orchestrator runs INSIDE a nemesis8
container and never touches Docker directly.

Tools provided:

  gateway_status            Health + active sessions + trigger counts.

  trigger_list              List all scheduled triggers.
  trigger_create            Create a new trigger (once / daily / interval).
  trigger_update            Patch an existing trigger.
  trigger_delete            Remove a trigger.

  tool_add                  Add a local file or remote URL to mcp_tools.
  tool_remove               Remove an entry from mcp_tools.
  tool_list_workspace       Show the workspace's active mcp_tools list.
  tool_list_community       Show MCP tools available in the image.
"""

from __future__ import annotations

import logging
import os
from pathlib import Path
from typing import Any, Dict, List, Optional

import httpx
import tomlkit

from mcp.server.fastmcp import FastMCP

# ── Setup ───────────────────────────────────────────────────────────

_default_log_root = Path(os.environ.get("CODEX_WORKSPACE_ROOT", "/workspace"))
log_dir = _default_log_root / ".mcp-logs"
log_dir.mkdir(parents=True, exist_ok=True)

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s - %(name)s - %(levelname)s - %(message)s",
    handlers=[logging.FileHandler(log_dir / "nemesis8-orchestrator.log")],
)
logger = logging.getLogger("nemesis8-orchestrator")

mcp = FastMCP("nemesis8-orchestrator")

# ── Paths ──────────────────────────────────────────────────────────

MCP_SOURCE = Path("/opt/mcp-installed")
CONFIG_FILENAME = ".nemesis8.toml"

# ── Gateway client ─────────────────────────────────────────────────


def _gateway_url() -> str:
    return os.environ.get("GATEWAY_URL", "http://host.docker.internal:40008").rstrip("/")


def _gateway_headers() -> Dict[str, str]:
    headers = {"Content-Type": "application/json"}
    token = os.environ.get("NEMESIS8_AUTH_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def _gateway_request(
    method: str,
    path: str,
    body: Optional[Dict[str, Any]] = None,
    timeout: float = 10.0,
) -> Dict[str, Any]:
    """Make a JSON HTTP request to the gateway and return the parsed body."""
    url = f"{_gateway_url()}{path}"
    try:
        with httpx.Client(timeout=timeout) as client:
            resp = client.request(method, url, json=body, headers=_gateway_headers())
    except httpx.ConnectError as e:
        return {"success": False, "error": f"Cannot reach gateway at {_gateway_url()}: {e}"}
    except httpx.HTTPError as e:
        return {"success": False, "error": f"Gateway HTTP error: {e}"}

    if resp.status_code >= 400:
        return {
            "success": False,
            "error": f"Gateway returned {resp.status_code}: {resp.text}",
            "status": resp.status_code,
        }

    if not resp.content:
        return {"success": True}

    try:
        return {"success": True, "data": resp.json()}
    except ValueError:
        return {"success": True, "data": resp.text}


# ── Workspace config helpers (mirrors tool-manager.py) ─────────────


def _is_url(s: str) -> bool:
    return s.startswith("http://") or s.startswith("https://")


def _has_config(root: Path) -> bool:
    return (root / CONFIG_FILENAME).exists()


def _resolve_workspace_root() -> Path:
    env_root = os.environ.get("CODEX_WORKSPACE_ROOT")
    if env_root:
        candidate = Path(env_root)
        if candidate.exists():
            return candidate

    cwd = Path.cwd()
    for parent in [cwd] + list(cwd.parents):
        if _has_config(parent):
            return parent

    workspace = Path("/workspace")
    if workspace.exists():
        if _has_config(workspace):
            return workspace
        candidates = [p for p in workspace.iterdir() if p.is_dir() and _has_config(p)]
        if len(candidates) == 1:
            return candidates[0]

    return workspace


def _workspace_config_path() -> Path:
    return _resolve_workspace_root() / CONFIG_FILENAME


def _load_workspace_config() -> tomlkit.TOMLDocument:
    path = _workspace_config_path()
    if not path.exists():
        return tomlkit.document()
    try:
        return tomlkit.parse(path.read_text(encoding="utf-8"))
    except Exception as e:
        logger.error("Failed to parse %s: %s", path, e)
        return tomlkit.document()


def _save_workspace_config(doc: tomlkit.TOMLDocument) -> Path:
    path = _workspace_config_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(tomlkit.dumps(doc), encoding="utf-8")
    return path


# ── gateway_status ─────────────────────────────────────────────────


@mcp.tool()
async def gateway_status() -> Dict[str, Any]:
    """Report the nemesis8 gateway's health and scheduler state.

    Returns success=true when the gateway responds. The response includes the
    gateway version, how many runs are in flight, max concurrency, and a
    summary of scheduled triggers (count, next fire time).
    """
    logger.info("gateway_status")

    health = _gateway_request("GET", "/health", timeout=5.0)
    if not health.get("success"):
        return health

    status = _gateway_request("GET", "/status", timeout=5.0)
    return {
        "success": True,
        "url": _gateway_url(),
        "health": health.get("data"),
        "status": status.get("data") if status.get("success") else None,
    }


# ── trigger_* ──────────────────────────────────────────────────────


@mcp.tool()
async def trigger_list() -> Dict[str, Any]:
    """List all scheduled triggers known to the gateway."""
    logger.info("trigger_list")
    return _gateway_request("GET", "/triggers")


@mcp.tool()
async def trigger_create(
    title: str,
    prompt_text: str,
    schedule_kind: str,
    schedule_value: str,
    description: str = "",
    tags: Optional[List[str]] = None,
    timezone: str = "UTC",
) -> Dict[str, Any]:
    """Create a new scheduled trigger.

    Args:
        title: Short human label for the trigger.
        prompt_text: The prompt the gateway runs when the trigger fires.
        schedule_kind: One of "once", "daily", "interval".
        schedule_value: For "once" an ISO timestamp (e.g. "2026-05-09T14:00:00Z");
            for "daily" the time as "HH:MM"; for "interval" the period in
            minutes as a string (e.g. "30").
        description: Optional longer description.
        tags: Optional list of tag strings.
        timezone: Timezone for daily schedules (default UTC).

    Returns:
        The created trigger record from the gateway.
    """
    logger.info("trigger_create kind=%s value=%s title=%s", schedule_kind, schedule_value, title)

    kind = schedule_kind.lower().strip()
    if kind == "once":
        schedule: Dict[str, Any] = {"Once": {"at": schedule_value}}
    elif kind == "daily":
        schedule = {"Daily": {"time": schedule_value, "timezone": timezone}}
    elif kind == "interval":
        try:
            minutes = int(schedule_value)
        except ValueError:
            return {
                "success": False,
                "error": f"interval schedule_value must be integer minutes, got: {schedule_value!r}",
            }
        schedule = {"Interval": {"minutes": minutes}}
    else:
        return {
            "success": False,
            "error": f"schedule_kind must be one of: once, daily, interval (got {schedule_kind!r})",
        }

    body = {
        "title": title,
        "description": description,
        "prompt_text": prompt_text,
        "schedule": schedule,
        "tags": tags or [],
    }
    return _gateway_request("POST", "/triggers", body=body)


@mcp.tool()
async def trigger_update(
    trigger_id: str,
    title: Optional[str] = None,
    description: Optional[str] = None,
    prompt_text: Optional[str] = None,
    enabled: Optional[bool] = None,
    tags: Optional[List[str]] = None,
) -> Dict[str, Any]:
    """Patch fields on an existing trigger. Schedule changes are not supported
    by this tool yet — to reschedule, delete and re-create."""
    logger.info("trigger_update id=%s", trigger_id)

    patch: Dict[str, Any] = {}
    if title is not None:
        patch["title"] = title
    if description is not None:
        patch["description"] = description
    if prompt_text is not None:
        patch["prompt_text"] = prompt_text
    if enabled is not None:
        patch["enabled"] = enabled
    if tags is not None:
        patch["tags"] = tags

    if not patch:
        return {"success": False, "error": "no fields to update"}

    return _gateway_request("PUT", f"/triggers/{trigger_id}", body=patch)


@mcp.tool()
async def trigger_delete(trigger_id: str) -> Dict[str, Any]:
    """Delete a trigger by id."""
    logger.info("trigger_delete id=%s", trigger_id)
    return _gateway_request("DELETE", f"/triggers/{trigger_id}")


# ── tool_* (workspace mcp_tools editing) ──────────────────────────


@mcp.tool()
async def tool_list_workspace() -> Dict[str, Any]:
    """Return the workspace's active mcp_tools list, split into local files
    and remote URLs."""
    logger.info("tool_list_workspace")
    doc = _load_workspace_config()
    tools = doc.get("mcp_tools") or []
    if not isinstance(tools, list):
        tools = []
    local = [t for t in tools if not _is_url(str(t))]
    remote = [t for t in tools if _is_url(str(t))]
    return {
        "success": True,
        "count": len(tools),
        "local": local,
        "remote": remote,
        "config_path": str(_workspace_config_path()),
    }


@mcp.tool()
async def tool_list_community() -> Dict[str, Any]:
    """List all file-based MCP tools available in the image (not necessarily
    installed in this workspace)."""
    logger.info("tool_list_community")
    if not MCP_SOURCE.exists():
        return {"success": True, "count": 0, "tools": []}
    tools = sorted(
        f.name for f in MCP_SOURCE.glob("*.py") if not f.name.startswith("_")
    )
    return {"success": True, "count": len(tools), "tools": tools, "path": str(MCP_SOURCE)}


@mcp.tool()
async def tool_add(entry: str) -> Dict[str, Any]:
    """Add a tool to the workspace mcp_tools list.

    Args:
        entry: Either a local filename ("serpapi-search.py") that must exist
            in /opt/mcp-installed, or an http:// / https:// URL pointing at a
            remote MCP server. Local and remote entries coexist in the same
            list — there is no separate mcp_servers key.

    Changes take effect on the next container restart.
    """
    logger.info("tool_add entry=%s", entry)

    if not _is_url(entry):
        available = sorted(f.name for f in MCP_SOURCE.glob("*.py")) if MCP_SOURCE.exists() else []
        if entry not in available:
            return {
                "success": False,
                "error": f"local tool '{entry}' not found in {MCP_SOURCE}",
                "available_local": available,
            }

    doc = _load_workspace_config()
    tools = doc.get("mcp_tools")
    if tools is None:
        tools = tomlkit.array()
        doc["mcp_tools"] = tools
    if entry in [str(t) for t in tools]:
        return {"success": False, "error": f"'{entry}' is already in mcp_tools"}

    tools.append(entry)
    path = _save_workspace_config(doc)

    return {
        "success": True,
        "entry": entry,
        "config_path": str(path),
        "restart_required": True,
    }


@mcp.tool()
async def tool_remove(entry: str) -> Dict[str, Any]:
    """Remove an entry (file name or URL) from the workspace mcp_tools list.

    Changes take effect on the next container restart.
    """
    logger.info("tool_remove entry=%s", entry)

    doc = _load_workspace_config()
    tools = doc.get("mcp_tools")
    if tools is None:
        return {"success": False, "error": "mcp_tools not set in workspace config"}

    current = [str(t) for t in tools]
    if entry not in current:
        return {"success": False, "error": f"'{entry}' is not in mcp_tools", "current": current}

    remaining = tomlkit.array()
    for t in tools:
        if str(t) != entry:
            remaining.append(t)
    doc["mcp_tools"] = remaining
    path = _save_workspace_config(doc)

    return {
        "success": True,
        "entry": entry,
        "config_path": str(path),
        "restart_required": True,
    }


# ── agent_* (fleet control via the gateway control plane) ─────────


@mcp.tool()
async def agent_list() -> Dict[str, Any]:
    """List every agent the control plane knows about — local and, if this
    gateway is a controller, agents reported by worker daemons too.

    Returns each agent's id ({host_id}/{local_id}), provider, state
    (starting/running/idle/exited/killed), source (spawned/discovered/
    registered), container, and workspace.
    """
    logger.info("agent_list")
    return _gateway_request("GET", "/agents")


@mcp.tool()
async def agent_get(agent_id: str) -> Dict[str, Any]:
    """Get one agent's full record. agent_id may be the local id, the global
    {host_id}/{local_id}, or a unique prefix."""
    logger.info("agent_get %s", agent_id)
    return _gateway_request("GET", f"/agents/{agent_id}")


@mcp.tool()
async def agent_spawn(prompt: str, provider: Optional[str] = None) -> Dict[str, Any]:
    """Launch a new agent with a one-shot prompt. The control plane starts a
    fresh container; it appears in agent_list within one reconcile tick.

    Args:
        prompt: The prompt to run.
        provider: Optional provider override (codex, gemini, claude,
            antigravity, ...). Defaults to the gateway's configured provider.
    """
    logger.info("agent_spawn provider=%s", provider)
    body: Dict[str, Any] = {"prompt": prompt}
    if provider:
        body["provider"] = provider
    return _gateway_request("POST", "/agents/spawn", body=body)


@mcp.tool()
async def agent_kill(agent_id: str) -> Dict[str, Any]:
    """Stop an agent and mark it killed. Cross-host kills route through the
    owning worker daemon. agent_id may be local id, global id, or prefix."""
    logger.info("agent_kill %s", agent_id)
    return _gateway_request("POST", f"/agents/{agent_id}/kill")


@mcp.tool()
async def agent_events(tail: int = 50) -> Dict[str, Any]:
    """Recent telemetry events (filesystem/network/heartbeat) from the
    monitor stream. Returns the last `tail` events.
    """
    logger.info("agent_events tail=%d", tail)
    result = _gateway_request("GET", "/monitor/events")
    if result.get("success") and isinstance(result.get("data"), list):
        events = result["data"]
        result["data"] = events[-tail:] if tail > 0 else events
        result["count"] = len(result["data"])
    return result


@mcp.tool()
async def daemon_list() -> Dict[str, Any]:
    """List worker daemons registered with this controller (the fleet's
    hosts). Empty on a standalone gateway."""
    logger.info("daemon_list")
    return _gateway_request("GET", "/daemons")


if __name__ == "__main__":
    mcp.run()
