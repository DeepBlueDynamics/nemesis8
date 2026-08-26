#!/usr/bin/env python3
"""MCP: discord

Discord connector for agents. Send and read messages on a channel via the
Discord REST API (a bot token), or post via an incoming webhook (no bot needed).

Config — forward these into the container with `env_imports` in .nemesis8.toml,
e.g. `env_imports = ["DISCORD_BOT_TOKEN", "DISCORD_CHANNEL_ID"]`:
  DISCORD_BOT_TOKEN    Bot token — enables send + read on any channel the bot
                       can access. Create a bot at discord.com/developers, invite
                       it to the server, and give it Send/Read Message perms.
  DISCORD_CHANNEL_ID   Optional default channel id for send/read (so agents can
                       just post without knowing the id).
  DISCORD_WEBHOOK_URL  Optional webhook URL — enables discord_post (send-only,
                       no bot required; posts to that webhook's channel).
  DISCORD_API_URL      Override the API base (default https://discord.com/api/v10).
"""
# n8:secrets required=DISCORD_BOT_TOKEN optional=DISCORD_CHANNEL_ID,DISCORD_WEBHOOK_URL

from __future__ import annotations

import json
import logging
import os
import sys
from pathlib import Path
from typing import Dict, Optional
from urllib import request as _urlrequest
from urllib.error import HTTPError, URLError

from mcp.server.fastmcp import FastMCP

# Log to the workspace like the other MCP tools; degrade gracefully off-container.
try:
    log_dir = Path("/workspace/.mcp-logs")
    log_dir.mkdir(parents=True, exist_ok=True)
    _handlers = [logging.FileHandler(log_dir / "discord.log"), logging.StreamHandler(sys.stderr)]
except OSError:
    _handlers = [logging.StreamHandler(sys.stderr)]

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s - %(name)s - %(levelname)s - %(message)s",
    handlers=_handlers,
)
logger = logging.getLogger("discord")

mcp = FastMCP("discord")

API_URL = os.getenv("DISCORD_API_URL", "https://discord.com/api/v10").rstrip("/")
MAX_LEN = 2000  # Discord's per-message character limit.


def _bot_request(method: str, path: str, payload: Optional[dict] = None):
    """Call the Discord REST API with the bot token. Returns the parsed JSON, or
    an {"success": False, "error": ...} dict on any failure."""
    token = os.getenv("DISCORD_BOT_TOKEN", "").strip()
    if not token:
        return {"success": False, "error": "DISCORD_BOT_TOKEN not set — add it to env_imports in .nemesis8.toml"}
    data = json.dumps(payload).encode("utf-8") if payload is not None else None
    headers = {
        "Authorization": f"Bot {token}",
        # Discord requires a descriptive User-Agent on bot requests.
        "User-Agent": "nemesis8-discord (https://deepbluedynamics.com, 1.0)",
    }
    if data is not None:
        headers["Content-Type"] = "application/json"
    req = _urlrequest.Request(f"{API_URL}{path}", data=data, headers=headers, method=method)
    try:
        with _urlrequest.urlopen(req, timeout=30) as resp:
            body = resp.read().decode("utf-8")
            return json.loads(body) if body else {}
    except HTTPError as e:
        detail = e.read().decode("utf-8", "replace")
        logger.error("Discord API %s %s -> HTTP %s: %s", method, path, e.code, detail)
        return {"success": False, "error": f"HTTP {e.code}", "detail": detail}
    except URLError as e:
        logger.error("Discord API %s %s -> %s", method, path, e)
        return {"success": False, "error": str(e)}


@mcp.tool()
def discord_send_message(
    content: str,
    channel_id: Optional[str] = None,
    reply_to_message_id: Optional[str] = None,
) -> Dict:
    """Send a message to a Discord channel (requires DISCORD_BOT_TOKEN).

    Args:
        content: Message text — Discord markdown supported; max 2000 chars.
        channel_id: Target channel id. Defaults to DISCORD_CHANNEL_ID if omitted.
        reply_to_message_id: Optional message id to reply to (threads the reply).

    Returns:
        The created message object, or {"success": False, "error": ...}.
    """
    channel_id = (channel_id or os.getenv("DISCORD_CHANNEL_ID", "")).strip()
    if not channel_id:
        return {"success": False, "error": "no channel_id given and DISCORD_CHANNEL_ID not set"}
    if len(content) > MAX_LEN:
        return {"success": False, "error": f"content is {len(content)} chars; Discord limit is {MAX_LEN}"}
    payload: Dict = {"content": content}
    if reply_to_message_id:
        payload["message_reference"] = {"message_id": reply_to_message_id}
    logger.info("send -> channel %s (%d chars)", channel_id, len(content))
    return _bot_request("POST", f"/channels/{channel_id}/messages", payload)


@mcp.tool()
def discord_read_messages(channel_id: Optional[str] = None, limit: int = 20) -> Dict:
    """Read recent messages from a Discord channel (requires DISCORD_BOT_TOKEN).

    Args:
        channel_id: Channel id. Defaults to DISCORD_CHANNEL_ID if omitted.
        limit: How many recent messages to fetch (1-100, default 20).

    Returns:
        {"messages": [{id, author, content, timestamp}], "count": N} — newest
        first — or {"success": False, "error": ...}.
    """
    channel_id = (channel_id or os.getenv("DISCORD_CHANNEL_ID", "")).strip()
    if not channel_id:
        return {"success": False, "error": "no channel_id given and DISCORD_CHANNEL_ID not set"}
    limit = max(1, min(int(limit), 100))
    raw = _bot_request("GET", f"/channels/{channel_id}/messages?limit={limit}")
    if isinstance(raw, dict) and raw.get("success") is False:
        return raw
    msgs = [
        {
            "id": m.get("id"),
            "author": (m.get("author") or {}).get("username"),
            "content": m.get("content"),
            "timestamp": m.get("timestamp"),
        }
        for m in raw
    ] if isinstance(raw, list) else []
    return {"messages": msgs, "count": len(msgs)}


@mcp.tool()
def discord_post(content: str, username: Optional[str] = None) -> Dict:
    """Post a message via a Discord webhook — no bot needed (requires DISCORD_WEBHOOK_URL).

    Args:
        content: Message text — max 2000 chars.
        username: Optional override for the webhook's displayed name.

    Returns:
        {"success": True} on success (Discord returns 204), or
        {"success": False, "error": ...}.
    """
    webhook = os.getenv("DISCORD_WEBHOOK_URL", "").strip()
    if not webhook:
        return {"success": False, "error": "DISCORD_WEBHOOK_URL not set — add it to env_imports in .nemesis8.toml"}
    if len(content) > MAX_LEN:
        return {"success": False, "error": f"content is {len(content)} chars; Discord limit is {MAX_LEN}"}
    payload: Dict = {"content": content}
    if username:
        payload["username"] = username
    req = _urlrequest.Request(
        webhook,
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with _urlrequest.urlopen(req, timeout=30) as resp:
            body = resp.read().decode("utf-8")
            logger.info("webhook post ok (%d chars)", len(content))
            return {"success": True, **({"response": json.loads(body)} if body else {})}
    except HTTPError as e:
        detail = e.read().decode("utf-8", "replace")
        logger.error("webhook post -> HTTP %s: %s", e.code, detail)
        return {"success": False, "error": f"HTTP {e.code}", "detail": detail}
    except URLError as e:
        return {"success": False, "error": str(e)}


if __name__ == "__main__":
    mcp.run()
