#!/usr/bin/env python3
"""MCP: nemesis8

Unified control plane for the nemesis8 service.
Talks to the nemisis8 gateway (nemisis8 serve) over HTTP.

Provides:
  - Trigger scheduling (cron, once, interval)
  - Prompt execution through Docker containers
  - Session listing and inspection
  - Gateway status and health
  - Time utilities for scheduling
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from datetime import datetime, timedelta, timezone
from typing import Any, Dict, List, Optional
from zoneinfo import ZoneInfo

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("nemesis8")

GATEWAY_URL = os.environ.get("GATEWAY_URL", os.environ.get("NEMESIS8_GATEWAY_URL", "http://localhost:4000"))
AUTH_TOKEN = os.environ.get("NEMESIS8_AUTH_TOKEN", "")


# ── HTTP helpers ──

def _gateway(method: str, path: str, body: Optional[dict] = None, timeout: int = 30) -> dict:
    """Make an HTTP request to the nemesis8 gateway."""
    url = f"{GATEWAY_URL}{path}"
    data = json.dumps(body).encode() if body else None
    headers: dict = {"Content-Type": "application/json"} if data else {}
    if AUTH_TOKEN:
        headers["Authorization"] = f"Bearer {AUTH_TOKEN}"
    req = urllib.request.Request(
        url,
        data=data,
        method=method,
        headers=headers,
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode()
            return json.loads(raw) if raw.strip() else {}
    except urllib.error.HTTPError as e:
        body_text = e.read().decode() if e.fp else ""
        try:
            err = json.loads(body_text)
            raise RuntimeError(f"gateway {e.code}: {err.get('error', body_text)}")
        except json.JSONDecodeError:
            raise RuntimeError(f"gateway {e.code}: {body_text}")
    except urllib.error.URLError as e:
        raise RuntimeError(
            f"cannot reach gateway at {GATEWAY_URL} - is 'nemisis8 serve' running? ({e.reason})"
        )


def _ok(result: Any, next_steps: Optional[List[str]] = None) -> str:
    """Format a tool response with optional next_steps hints."""
    out: Dict[str, Any] = {"result": result}
    if next_steps:
        out["next_steps"] = next_steps
    return json.dumps(out, indent=2, default=str)


# ── Status & Health ──

@mcp.tool()
def status() -> str:
    """Check nemesis8 gateway status: active runs, scheduler info, uptime."""
    data = _gateway("GET", "/status")
    return _ok(data, [
        "Use list_triggers() to see scheduled jobs",
        "Use run_prompt() to execute a prompt",
    ])


@mcp.tool()
def health() -> str:
    """Quick liveness check on the nemesis8 gateway."""
    data = _gateway("GET", "/health")
    return _ok(data)


# ── Prompt Execution ──

@mcp.tool()
def run_prompt(prompt: str, model: Optional[str] = None, session_id: Optional[str] = None) -> str:
    """Run a prompt through a Docker container via the nemesis8 gateway.

    Args:
        prompt: The instruction to execute
        model: Optional model override
        session_id: Optional session ID to continue
    """
    body: Dict[str, Any] = {"prompt": prompt}
    if model:
        body["model"] = model
    if session_id:
        body["session_id"] = session_id

    data = _gateway("POST", "/completion", body, timeout=300)
    return _ok(data, [
        f"Session: {data.get('session_id', 'unknown')}",
        "Use session_detail() to inspect the session",
    ])


# ── Trigger Scheduling ──

@mcp.tool()
def list_triggers() -> str:
    """List all scheduled triggers with their status and next fire time."""
    triggers = _gateway("GET", "/triggers")
    summary = []
    for t in triggers:
        summary.append({
            "id": t["id"],
            "title": t["title"],
            "enabled": t.get("enabled", True),
            "schedule": t.get("schedule"),
            "last_fired": t.get("last_fired"),
            "last_status": t.get("last_status"),
        })
    return _ok(summary, [
        "Use get_trigger(id) for full details",
        "Use create_trigger() to add a new one",
    ])


@mcp.tool()
def get_trigger(trigger_id: str) -> str:
    """Get full details for a specific trigger.

    Args:
        trigger_id: The trigger ID
    """
    data = _gateway("GET", f"/triggers/{trigger_id}")
    return _ok(data, [
        "Use update_trigger() to modify",
        "Use delete_trigger() to remove",
    ])


@mcp.tool()
def create_trigger(
    title: str,
    prompt_text: str,
    schedule_mode: str,
    description: str = "",
    schedule_time: Optional[str] = None,
    once_at: Optional[str] = None,
    minutes_from_now: Optional[float] = None,
    interval_minutes: Optional[float] = None,
    timezone_name: str = "UTC",
    tags: Optional[List[str]] = None,
) -> str:
    """Create a new scheduled trigger.

    Args:
        title: Human-readable name
        prompt_text: The prompt to execute when triggered
        schedule_mode: One of "daily", "once", "interval"
        description: Optional description
        schedule_time: For daily mode: HH:MM (e.g. "14:30")
        once_at: For once mode: ISO timestamp
        minutes_from_now: For once mode: minutes from now (alternative to once_at)
        interval_minutes: For interval mode: repeat every N minutes
        timezone_name: Timezone for daily schedules (default UTC)
        tags: Optional list of tags
    """
    if schedule_mode == "daily":
        if not schedule_time:
            return _ok({"error": "daily mode requires schedule_time (HH:MM)"})
        schedule = {"type": "daily", "time": schedule_time, "timezone": timezone_name}
    elif schedule_mode == "once":
        if minutes_from_now:
            at = datetime.now(timezone.utc) + timedelta(minutes=minutes_from_now)
            at_str = at.isoformat()
        elif once_at:
            at_str = once_at
        else:
            return _ok({"error": "once mode requires once_at or minutes_from_now"})
        schedule = {"type": "once", "at": at_str}
    elif schedule_mode == "interval":
        if not interval_minutes or interval_minutes <= 0:
            return _ok({"error": "interval mode requires interval_minutes > 0"})
        schedule = {"type": "interval", "minutes": int(interval_minutes)}
    else:
        return _ok({"error": f"unknown schedule_mode: {schedule_mode}. Use daily, once, or interval"})

    body = {
        "title": title,
        "description": description,
        "prompt_text": prompt_text,
        "schedule": schedule,
        "tags": tags or [],
    }

    data = _gateway("POST", "/triggers", body)
    return _ok(data, [
        f"Trigger {data.get('id', '?')} created",
        "Use list_triggers() to verify",
    ])


@mcp.tool()
def update_trigger(
    trigger_id: str,
    title: Optional[str] = None,
    description: Optional[str] = None,
    prompt_text: Optional[str] = None,
    enabled: Optional[bool] = None,
    tags: Optional[List[str]] = None,
) -> str:
    """Update fields on an existing trigger.

    Args:
        trigger_id: The trigger ID to update
        title: New title
        description: New description
        prompt_text: New prompt
        enabled: Enable or disable
        tags: New tags
    """
    body: Dict[str, Any] = {}
    if title is not None:
        body["title"] = title
    if description is not None:
        body["description"] = description
    if prompt_text is not None:
        body["prompt_text"] = prompt_text
    if enabled is not None:
        body["enabled"] = enabled
    if tags is not None:
        body["tags"] = tags

    if not body:
        return _ok({"error": "nothing to update"})

    data = _gateway("PUT", f"/triggers/{trigger_id}", body)
    return _ok(data)


@mcp.tool()
def toggle_trigger(trigger_id: str, enabled: bool) -> str:
    """Enable or disable a trigger.

    Args:
        trigger_id: The trigger ID
        enabled: True to enable, False to disable
    """
    data = _gateway("PUT", f"/triggers/{trigger_id}", {"enabled": enabled})
    return _ok(data, [f"Trigger {'enabled' if enabled else 'disabled'}"])


@mcp.tool()
def delete_trigger(trigger_id: str) -> str:
    """Delete a trigger permanently.

    Args:
        trigger_id: The trigger ID to delete
    """
    _gateway("DELETE", f"/triggers/{trigger_id}")
    return _ok({"deleted": trigger_id})


# ── Sessions ──

@mcp.tool()
def list_sessions(limit: int = 20) -> str:
    """List recent sessions.

    Args:
        limit: Maximum number of sessions to return (default 20)
    """
    sessions = _gateway("GET", "/sessions")
    truncated = sessions[:limit]
    return _ok(truncated, [
        "Use session_detail(id) for full info",
        f"Showing {len(truncated)} of {len(sessions)} sessions",
    ])


@mcp.tool()
def session_detail(session_id: str) -> str:
    """Get details for a specific session.

    Args:
        session_id: Full UUID or last 5 characters
    """
    data = _gateway("GET", f"/sessions/{session_id}")
    return _ok(data, [
        "Use run_prompt(prompt, session_id=id) to continue this session",
    ])


# ── Time Utilities ──

@mcp.tool()
def clock_now(timezone_name: str = "UTC") -> str:
    """Get current time in a timezone.

    Args:
        timezone_name: IANA timezone (e.g. "America/Chicago", "UTC")
    """
    try:
        tz = ZoneInfo(timezone_name)
    except KeyError:
        return _ok({"error": f"unknown timezone: {timezone_name}"})

    now = datetime.now(tz)
    return _ok({
        "timezone": timezone_name,
        "iso": now.isoformat(),
        "human": now.strftime("%Y-%m-%d %H:%M:%S %Z"),
        "utc_offset": str(now.utcoffset()),
    })


@mcp.tool()
def clock_add(
    days: float = 0,
    hours: float = 0,
    minutes: float = 0,
    base_iso: Optional[str] = None,
    timezone_name: str = "UTC",
) -> str:
    """Compute a future timestamp by adding time.

    Args:
        days: Days to add
        hours: Hours to add
        minutes: Minutes to add
        base_iso: Base ISO timestamp (default: now)
        timezone_name: Timezone for display
    """
    try:
        tz = ZoneInfo(timezone_name)
    except KeyError:
        return _ok({"error": f"unknown timezone: {timezone_name}"})

    if base_iso:
        base = datetime.fromisoformat(base_iso)
        if base.tzinfo is None:
            base = base.replace(tzinfo=timezone.utc)
    else:
        base = datetime.now(timezone.utc)

    target = base + timedelta(days=days, hours=hours, minutes=minutes)
    local = target.astimezone(tz)

    return _ok({
        "iso": target.isoformat(),
        "local": local.strftime("%Y-%m-%d %H:%M:%S %Z"),
        "timezone": timezone_name,
    })


if __name__ == "__main__":
    mcp.run()
