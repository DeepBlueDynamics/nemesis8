#!/usr/bin/env python3
"""
AgentMail MCP server
====================

Tools:
  - agentmail_status: check key/base URL status
  - set_agentmail_key: capture/persist API key (env or .agentmail.env)
  - agentmail_list_inboxes: list inboxes
  - agentmail_get_inbox: get inbox details
  - agentmail_create_inbox: create a new inbox
  - agentmail_list_messages: list messages in an inbox
  - agentmail_get_message: fetch a specific message
  - agentmail_send_message: send a message from an inbox
  - agentmail_get_raw_message: download raw .eml payload

Auth:
  - Set AGENTMAIL_API_KEY in the environment, or call set_agentmail_key()
  - Optional .agentmail.env file in the workspace with AGENTMAIL_API_KEY=...

Base URL:
  - AGENTMAIL_BASE_URL (default: https://api.agentmail.to)
"""

from __future__ import annotations

import json
import os
import base64
import mimetypes
from typing import Any, Dict, List, Optional, Tuple

import aiohttp
from mcp.server.fastmcp import FastMCP

mcp = FastMCP("agentmail")

DEFAULT_BASE_URL = "https://api.agentmail.to"
AGENTMAIL_ENV_FILE = os.path.join(os.getcwd(), ".agentmail.env")
MAX_INLINE_RAW_BYTES = 200_000


def _get_base_url() -> str:
    base = os.environ.get("AGENTMAIL_BASE_URL", DEFAULT_BASE_URL).strip()
    if not base:
        base = DEFAULT_BASE_URL
    return base.rstrip("/")


def _get_agentmail_key() -> Optional[str]:
    key = os.environ.get("AGENTMAIL_API_KEY") or os.environ.get("AGENTMAIL_KEY")
    if key:
        return key.strip()
    try:
        if os.path.exists(AGENTMAIL_ENV_FILE):
            with open(AGENTMAIL_ENV_FILE, "r", encoding="utf-8") as f:
                for line in f:
                    line = line.strip()
                    if not line or line.startswith("#"):
                        continue
                    if line.startswith("AGENTMAIL_API_KEY="):
                        return line.split("=", 1)[1].strip().strip('"').strip("'")
                    if line.startswith("AGENTMAIL_KEY="):
                        return line.split("=", 1)[1].strip().strip('"').strip("'")
    except Exception:
        pass
    return None


def _extract_key_from_text(text: str) -> Optional[str]:
    if not text:
        return None
    raw = text.strip()
    for prefix in (
        "AGENTMAIL_API_KEY=",
        "AGENTMAIL_KEY=",
        "agentmail_api_key=",
        "agentmail_key=",
        "api_key=",
        "key=",
    ):
        if prefix in raw:
            candidate = raw.split(prefix, 1)[1].strip().strip('"').strip("'")
            if candidate:
                return candidate
    for line in raw.splitlines():
        line = line.strip()
        if not line:
            continue
        if "AGENTMAIL_API_KEY=" in line:
            return line.split("AGENTMAIL_API_KEY=", 1)[1].strip().strip('"').strip("'")
        if line.lower().startswith("agentmail_api_key="):
            return line.split("=", 1)[1].strip().strip('"').strip("'")
    tokens: List[str] = []
    current: List[str] = []
    for ch in raw:
        if ch.isalnum() or ch in ("-", "_"):
            current.append(ch)
        else:
            if current:
                tokens.append("".join(current))
                current = []
    if current:
        tokens.append("".join(current))
    candidates = [t for t in tokens if 20 <= len(t) <= 120]
    if not candidates:
        return None
    preferred = [t for t in candidates if t.lower().startswith("am_")]
    preferred.sort(key=len, reverse=True)
    if preferred:
        return preferred[0]
    candidates.sort(key=len, reverse=True)
    return candidates[0]


def _write_agentmail_env_file(key: str) -> Tuple[bool, Optional[str]]:
    try:
        with open(AGENTMAIL_ENV_FILE, "w", encoding="utf-8") as f:
            f.write(f"AGENTMAIL_API_KEY={key}\n")
        return True, None
    except Exception as e:
        return False, str(e)


def _normalize_params(params: Dict[str, Any]) -> Dict[str, Any]:
    cleaned: Dict[str, Any] = {}
    for key, value in params.items():
        if value is None:
            continue
        if isinstance(value, bool):
            cleaned[key] = "true" if value else "false"
        elif isinstance(value, (list, tuple)):
            cleaned[key] = ",".join(str(v) for v in value)
        else:
            cleaned[key] = value
    return cleaned


def _normalize_path_list(value: Optional[List[str] | str]) -> List[str]:
    if value is None:
        return []
    if isinstance(value, list):
        return [str(v).strip() for v in value if str(v).strip()]
    raw = str(value).strip()
    if not raw:
        return []
    if "," in raw:
        return [p.strip() for p in raw.split(",") if p.strip()]
    return [raw]


def _attachment_from_file(path_str: str) -> Dict[str, Any]:
    abs_path = os.path.abspath(os.path.expanduser(path_str))
    if not os.path.exists(abs_path):
        raise FileNotFoundError(f"Attachment file not found: {abs_path}")
    if not os.path.isfile(abs_path):
        raise ValueError(f"Attachment path is not a file: {abs_path}")
    with open(abs_path, "rb") as f:
        raw = f.read()
    content_type = mimetypes.guess_type(abs_path)[0] or "application/octet-stream"
    return {
        "filename": os.path.basename(abs_path),
        "content_type": content_type,
        "content": base64.b64encode(raw).decode("ascii"),
    }


def _decode_response(raw: bytes, content_type: str) -> Any:
    if not raw:
        return None
    if "application/json" in (content_type or "").lower():
        try:
            return json.loads(raw.decode("utf-8"))
        except Exception:
            return raw.decode("utf-8", errors="replace")
    try:
        return raw.decode("utf-8")
    except Exception:
        return raw.decode("utf-8", errors="replace")


async def _agentmail_request(
    method: str,
    path: str,
    params: Optional[Dict[str, Any]] = None,
    json_body: Optional[Dict[str, Any]] = None,
    timeout_seconds: float = 30.0,
) -> Dict[str, Any]:
    key = _get_agentmail_key()
    if not key:
        return {"success": False, "error": "AGENTMAIL_API_KEY not set. Use set_agentmail_key() or env var."}
    url = f"{_get_base_url()}{path}"
    headers = {
        "Authorization": f"Bearer {key}",
        "User-Agent": "agentmail-mcp/1.0",
    }
    if json_body is not None:
        headers["Content-Type"] = "application/json"
    try:
        timeout = aiohttp.ClientTimeout(total=timeout_seconds)
        async with aiohttp.ClientSession(timeout=timeout) as session:
            async with session.request(
                method.upper(),
                url,
                headers=headers,
                params=params,
                json=json_body,
            ) as resp:
                raw = await resp.read()
                payload = _decode_response(raw, resp.headers.get("Content-Type", ""))
                if 200 <= resp.status < 300:
                    return {"success": True, "status": resp.status, "data": payload, "url": url}
                return {"success": False, "status": resp.status, "error": payload, "url": url}
    except aiohttp.ClientError as exc:
        return {"success": False, "error": str(exc), "url": url}


if __name__ == "__main__":
    mcp.run(transport="stdio")


@mcp.tool()
def agentmail_status() -> Dict[str, Any]:
    """Check AgentMail API key status and base URL."""
    key = _get_agentmail_key()
    return {
        "success": True,
        "api_key_present": key is not None,
        "key_last4": key[-4:] if key else None,
        "base_url": _get_base_url(),
        "env_file": AGENTMAIL_ENV_FILE,
    }


@mcp.tool()
def set_agentmail_key(text: str, persist: bool = False) -> Dict[str, Any]:
    """Extract and set the AgentMail API key from pasted text.

    - Parses common forms (e.g., "AGENTMAIL_API_KEY=..." or raw token)
    - Sets the key in-memory for this process
    - If persist=True, writes to a local .agentmail.env file
    """
    if not text:
        return {"success": False, "error": "No text provided"}
    key = _extract_key_from_text(text)
    if not key:
        return {"success": False, "error": "No valid key found in text"}

    os.environ["AGENTMAIL_API_KEY"] = key
    result: Dict[str, Any] = {
        "success": True,
        "set_in_memory": True,
        "key_last4": key[-4:],
        "persisted": False,
        "source": "env",
    }
    if persist:
        ok, err = _write_agentmail_env_file(key)
        result["persisted"] = bool(ok)
        if not ok:
            result["persist_error"] = err
        else:
            result["source"] = ".agentmail.env"
    return result


@mcp.tool()
async def agentmail_list_inboxes(limit: int = 50, page_token: Optional[str] = None) -> Dict[str, Any]:
    """List inboxes (GET /v0/inboxes)."""
    params = _normalize_params({"limit": limit, "page_token": page_token})
    return await _agentmail_request("GET", "/v0/inboxes", params=params)


@mcp.tool()
async def agentmail_get_inbox(inbox_id: str) -> Dict[str, Any]:
    """Get a specific inbox (GET /v0/inboxes/:inbox_id)."""
    if not inbox_id:
        return {"success": False, "error": "Missing inbox_id"}
    return await _agentmail_request("GET", f"/v0/inboxes/{inbox_id}")


@mcp.tool()
async def agentmail_create_inbox(
    username: Optional[str] = None,
    domain: Optional[str] = None,
    display_name: Optional[str] = None,
    client_id: Optional[str] = None,
) -> Dict[str, Any]:
    """Create an inbox (POST /v0/inboxes)."""
    payload = _normalize_params(
        {
            "username": username,
            "domain": domain,
            "display_name": display_name,
            "client_id": client_id,
        }
    )
    return await _agentmail_request("POST", "/v0/inboxes", json_body=payload)


@mcp.tool()
async def agentmail_list_messages(
    inbox_id: str,
    limit: int = 50,
    page_token: Optional[str] = None,
    labels: Optional[List[str] | str] = None,
    before: Optional[str] = None,
    after: Optional[str] = None,
    ascending: Optional[bool] = None,
    include_spam: Optional[bool] = None,
) -> Dict[str, Any]:
    """List messages in an inbox (GET /v0/inboxes/:inbox_id/messages)."""
    if not inbox_id:
        return {"success": False, "error": "Missing inbox_id"}
    params = _normalize_params(
        {
            "limit": limit,
            "page_token": page_token,
            "labels": labels,
            "before": before,
            "after": after,
            "ascending": ascending,
            "include_spam": include_spam,
        }
    )
    return await _agentmail_request("GET", f"/v0/inboxes/{inbox_id}/messages", params=params)


@mcp.tool()
async def agentmail_get_message(inbox_id: str, message_id: str) -> Dict[str, Any]:
    """Get a specific message (GET /v0/inboxes/:inbox_id/messages/:message_id)."""
    if not inbox_id or not message_id:
        return {"success": False, "error": "Missing inbox_id or message_id"}
    return await _agentmail_request("GET", f"/v0/inboxes/{inbox_id}/messages/{message_id}")


@mcp.tool()
async def agentmail_send_message(
    inbox_id: str,
    to: Optional[List[str] | str] = None,
    subject: Optional[str] = None,
    text: Optional[str] = None,
    html: Optional[str] = None,
    cc: Optional[List[str] | str] = None,
    bcc: Optional[List[str] | str] = None,
    reply_to: Optional[str] = None,
    labels: Optional[List[str] | str] = None,
    attachments: Optional[List[Dict[str, Any]]] = None,
    attachment_paths: Optional[List[str] | str] = None,
    headers: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    """Send a message (POST /v0/inboxes/:inbox_id/messages/send)."""
    if not inbox_id:
        return {"success": False, "error": "Missing inbox_id"}
    payload: Dict[str, Any] = {}
    if to is not None:
        payload["to"] = to
    if cc is not None:
        payload["cc"] = cc
    if bcc is not None:
        payload["bcc"] = bcc
    if subject is not None:
        payload["subject"] = subject
    if text is not None:
        payload["text"] = text
    if html is not None:
        payload["html"] = html
    if reply_to is not None:
        payload["reply_to"] = reply_to
    if labels is not None:
        payload["labels"] = labels
    built_attachments: List[Dict[str, Any]] = []
    for p in _normalize_path_list(attachment_paths):
        try:
            built_attachments.append(_attachment_from_file(p))
        except Exception as exc:
            return {
                "success": False,
                "error": f"Failed to load attachment file: {exc}",
                "file": p,
            }
    if attachments is not None:
        built_attachments.extend(attachments)
    if built_attachments:
        payload["attachments"] = built_attachments
    if headers is not None:
        payload["headers"] = headers
    return await _agentmail_request(
        "POST", f"/v0/inboxes/{inbox_id}/messages/send", json_body=payload
    )


@mcp.tool()
async def agentmail_get_raw_message(
    inbox_id: str,
    message_id: str,
    output_path: Optional[str] = None,
) -> Dict[str, Any]:
    """Fetch raw message (GET /v0/inboxes/:inbox_id/messages/:message_id/raw)."""
    if not inbox_id or not message_id:
        return {"success": False, "error": "Missing inbox_id or message_id"}
    key = _get_agentmail_key()
    if not key:
        return {"success": False, "error": "AGENTMAIL_API_KEY not set. Use set_agentmail_key() or env var."}
    url = f"{_get_base_url()}/v0/inboxes/{inbox_id}/messages/{message_id}/raw"
    headers = {"Authorization": f"Bearer {key}", "User-Agent": "agentmail-mcp/1.0"}
    try:
        timeout = aiohttp.ClientTimeout(total=30)
        async with aiohttp.ClientSession(timeout=timeout) as session:
            async with session.get(url, headers=headers) as resp:
                raw = await resp.read()
                if resp.status < 200 or resp.status >= 300:
                    return {
                        "success": False,
                        "status": resp.status,
                        "error": _decode_response(raw, resp.headers.get("Content-Type", "")),
                        "url": url,
                    }
                if output_path:
                    os.makedirs(os.path.dirname(output_path) or ".", exist_ok=True)
                    with open(output_path, "wb") as f:
                        f.write(raw)
                    return {
                        "success": True,
                        "status": resp.status,
                        "output_path": output_path,
                        "size_bytes": len(raw),
                        "url": url,
                    }
                if len(raw) > MAX_INLINE_RAW_BYTES:
                    return {
                        "success": False,
                        "status": resp.status,
                        "error": "Raw message too large for inline return. Provide output_path to save.",
                        "size_bytes": len(raw),
                        "url": url,
                    }
                return {
                    "success": True,
                    "status": resp.status,
                    "raw": _decode_response(raw, resp.headers.get("Content-Type", "")),
                    "size_bytes": len(raw),
                    "url": url,
                }
    except aiohttp.ClientError as exc:
        return {"success": False, "error": str(exc), "url": url}
