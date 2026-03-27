#!/usr/bin/env python3
"""MCP: codex-agentmail

High-level AgentMail connector for Codex-oriented workflows.

This wraps common AgentMail actions behind a simpler stateful interface:
- manage API key
- create/select a default inbox
- list/read/send messages using that default inbox

State files:
- .agentmail.env (optional persisted API key)
- .codex-agentmail.json (default inbox metadata)
"""

from __future__ import annotations

import json
import os
import base64
import mimetypes
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

import aiohttp
from mcp.server.fastmcp import FastMCP

mcp = FastMCP("codex-agentmail")

DEFAULT_BASE_URL = "https://api.agentmail.to"
AGENTMAIL_ENV_FILE = Path(os.getcwd()) / ".agentmail.env"
CONNECTOR_STATE_FILE = Path(os.getcwd()) / ".codex-agentmail.json"


def _ok(**kwargs: Any) -> Dict[str, Any]:
    payload: Dict[str, Any] = {"success": True}
    payload.update(kwargs)
    return payload


def _fail(error: str, **kwargs: Any) -> Dict[str, Any]:
    payload: Dict[str, Any] = {"success": False, "error": error}
    payload.update(kwargs)
    return payload


def _get_base_url() -> str:
    base = os.environ.get("AGENTMAIL_BASE_URL", DEFAULT_BASE_URL).strip()
    return (base or DEFAULT_BASE_URL).rstrip("/")


def _get_api_key() -> Optional[str]:
    key = os.environ.get("AGENTMAIL_API_KEY") or os.environ.get("AGENTMAIL_KEY")
    if key:
        key = key.strip()
        if key:
            return key

    if AGENTMAIL_ENV_FILE.exists():
        try:
            for line in AGENTMAIL_ENV_FILE.read_text(encoding="utf-8").splitlines():
                line = line.strip()
                if not line or line.startswith("#") or "=" not in line:
                    continue
                name, value = line.split("=", 1)
                if name.strip() in {"AGENTMAIL_API_KEY", "AGENTMAIL_KEY"}:
                    value = value.strip().strip('"').strip("'")
                    if value:
                        return value
        except Exception:
            pass

    return None


def _extract_key_from_text(text: str) -> Optional[str]:
    raw = (text or "").strip()
    if not raw:
        return None

    prefixes = [
        "AGENTMAIL_API_KEY=",
        "AGENTMAIL_KEY=",
        "agentmail_api_key=",
        "agentmail_key=",
        "api_key=",
        "key=",
    ]
    for prefix in prefixes:
        if prefix in raw:
            value = raw.split(prefix, 1)[1].strip().strip('"').strip("'")
            if value:
                return value

    tokens: List[str] = []
    cur: List[str] = []
    for ch in raw:
        if ch.isalnum() or ch in {"-", "_"}:
            cur.append(ch)
        else:
            if cur:
                tokens.append("".join(cur))
                cur = []
    if cur:
        tokens.append("".join(cur))

    candidates = [t for t in tokens if 20 <= len(t) <= 160]
    if not candidates:
        return None

    preferred = [t for t in candidates if t.lower().startswith("am_")]
    if preferred:
        preferred.sort(key=len, reverse=True)
        return preferred[0]

    candidates.sort(key=len, reverse=True)
    return candidates[0]


def _write_key_file(key: str) -> Tuple[bool, Optional[str]]:
    try:
        AGENTMAIL_ENV_FILE.write_text(
            f"AGENTMAIL_API_KEY={key}\nAGENTMAIL_BASE_URL={_get_base_url()}\n",
            encoding="utf-8",
        )
        return True, None
    except Exception as exc:
        return False, str(exc)


def _load_state() -> Dict[str, Any]:
    if not CONNECTOR_STATE_FILE.exists():
        return {}
    try:
        data = json.loads(CONNECTOR_STATE_FILE.read_text(encoding="utf-8"))
        return data if isinstance(data, dict) else {}
    except Exception:
        return {}


def _save_state(state: Dict[str, Any]) -> Tuple[bool, Optional[str]]:
    try:
        CONNECTOR_STATE_FILE.write_text(json.dumps(state, indent=2) + "\n", encoding="utf-8")
        return True, None
    except Exception as exc:
        return False, str(exc)


def _resolve_default_inbox(explicit_inbox_id: Optional[str] = None) -> Tuple[Optional[str], Dict[str, Any]]:
    if explicit_inbox_id:
        return explicit_inbox_id, {}

    env_inbox = os.environ.get("CODEX_AGENTMAIL_INBOX_ID", "").strip()
    if env_inbox:
        return env_inbox, {}

    state = _load_state()
    inbox_id = (state.get("inbox_id") or "").strip()
    if inbox_id:
        return inbox_id, state

    return None, state


def _pick(d: Dict[str, Any], keys: List[str]) -> Optional[Any]:
    for k in keys:
        if k in d and d[k] is not None:
            return d[k]
    return None


def _extract_inbox(data: Any) -> Dict[str, Any]:
    if not isinstance(data, dict):
        return {}

    root = data.get("data") if isinstance(data.get("data"), dict) else data
    if not isinstance(root, dict):
        return {}

    inbox_id = _pick(root, ["id", "inbox_id", "inboxId"])
    email = _pick(root, ["email", "email_address", "address"])
    username = _pick(root, ["username", "local_part"])
    domain = _pick(root, ["domain"])

    if not email and username and domain:
        email = f"{username}@{domain}"

    out: Dict[str, Any] = {}
    if inbox_id:
        out["inbox_id"] = str(inbox_id)
    if email:
        out["email"] = str(email)
    if username:
        out["username"] = str(username)
    if domain:
        out["domain"] = str(domain)
    return out


def _to_addr_list(value: Optional[str | List[str]]) -> Optional[List[str] | str]:
    if value is None:
        return None
    if isinstance(value, list):
        cleaned = [v.strip() for v in value if isinstance(v, str) and v.strip()]
        return cleaned if cleaned else None

    raw = value.strip()
    if not raw:
        return None
    if "," in raw:
        parts = [p.strip() for p in raw.split(",") if p.strip()]
        return parts if parts else None
    return raw


def _normalize_path_list(value: Optional[str | List[str]]) -> List[str]:
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
    path = Path(path_str).expanduser().resolve()
    if not path.exists():
        raise FileNotFoundError(f"Attachment file not found: {path}")
    if not path.is_file():
        raise ValueError(f"Attachment path is not a file: {path}")
    raw = path.read_bytes()
    mime_type = mimetypes.guess_type(path.name)[0] or "application/octet-stream"
    return {
        "filename": path.name,
        "content_type": mime_type,
        "content": base64.b64encode(raw).decode("ascii"),
    }


async def _request(
    method: str,
    path: str,
    *,
    params: Optional[Dict[str, Any]] = None,
    json_body: Optional[Dict[str, Any]] = None,
    timeout_seconds: float = 30.0,
) -> Dict[str, Any]:
    key = _get_api_key()
    if not key:
        return _fail(
            "AGENTMAIL_API_KEY not set",
            likely_causes=["Missing .agentmail.env", "Key not loaded in environment"],
            try_instead=["codex_agentmail_set_key(text=..., persist=True)", "codex_agentmail_status()"],
            next_steps=["Create/set API key", "Restart MCP session if needed", "Re-run codex_agentmail_status"],
        )

    url = f"{_get_base_url()}{path}"
    headers = {"Authorization": f"Bearer {key}", "User-Agent": "codex-agentmail-mcp/1.0"}
    if json_body is not None:
        headers["Content-Type"] = "application/json"

    try:
        timeout = aiohttp.ClientTimeout(total=timeout_seconds)
        async with aiohttp.ClientSession(timeout=timeout) as session:
            async with session.request(method.upper(), url, params=params, json=json_body, headers=headers) as resp:
                raw = await resp.read()
                body: Any
                try:
                    body = json.loads(raw.decode("utf-8")) if raw else None
                except Exception:
                    body = raw.decode("utf-8", errors="replace")

                if 200 <= resp.status < 300:
                    return _ok(status=resp.status, url=url, data=body)

                return _fail(
                    f"AgentMail request failed with status {resp.status}",
                    status=resp.status,
                    url=url,
                    detail=body,
                    try_instead=["codex_agentmail_status()", "codex_agentmail_list_inboxes(limit=5)"],
                )
    except aiohttp.ClientError as exc:
        return _fail(
            "AgentMail network error",
            detail=str(exc),
            url=url,
            likely_causes=["Network issue", "Wrong AGENTMAIL_BASE_URL", "TLS/DNS issue"],
            next_steps=["Check connectivity", "Verify AGENTMAIL_BASE_URL", "Retry request"],
        )


@mcp.tool()
def codex_agentmail_status() -> Dict[str, Any]:
    """Show connector readiness for Codex AgentMail workflows.

    Use when:
    - You need to confirm key/base URL/default inbox setup.

    Do not use when:
    - You need message contents (use list/read tools).
    """
    key = _get_api_key()
    state = _load_state()
    return _ok(
        api_key_present=bool(key),
        key_last4=key[-4:] if key else None,
        base_url=_get_base_url(),
        env_file=str(AGENTMAIL_ENV_FILE),
        state_file=str(CONNECTOR_STATE_FILE),
        default_inbox_id=state.get("inbox_id") or os.environ.get("CODEX_AGENTMAIL_INBOX_ID"),
        default_email=state.get("email"),
    )


@mcp.tool()
def codex_agentmail_set_key(text: str, persist: bool = False) -> Dict[str, Any]:
    """Extract and set AGENTMAIL_API_KEY from pasted text.

    Use when:
    - You just created a key and want the connector ready.

    Args:
        text: Raw key or env-style line (e.g., AGENTMAIL_API_KEY=...)
        persist: If true, writes `.agentmail.env`.
    """
    key = _extract_key_from_text(text)
    if not key:
        return _fail(
            "No valid AgentMail key found in text",
            next_steps=["Paste AGENTMAIL_API_KEY=...", "Or paste raw key token"],
        )

    os.environ["AGENTMAIL_API_KEY"] = key
    out = _ok(set_in_memory=True, key_last4=key[-4:], persisted=False)

    if persist:
        ok, err = _write_key_file(key)
        if not ok:
            return _fail(
                "Key set in memory but failed to persist",
                key_last4=key[-4:],
                detail=err,
                next_steps=["Check workspace write permissions", "Write .agentmail.env manually"],
            )
        out["persisted"] = True
        out["env_file"] = str(AGENTMAIL_ENV_FILE)

    return out


@mcp.tool()
async def codex_agentmail_list_inboxes(limit: int = 20) -> Dict[str, Any]:
    """List inboxes from AgentMail for selecting a default inbox."""
    if limit < 1 or limit > 200:
        return _fail("limit must be 1..200", provided=limit)
    return await _request("GET", "/v0/inboxes", params={"limit": limit})


@mcp.tool()
async def codex_agentmail_bootstrap_default_inbox(
    username: Optional[str] = None,
    domain: Optional[str] = None,
    display_name: Optional[str] = "Codex",
    client_id: Optional[str] = "codex",
    persist: bool = True,
) -> Dict[str, Any]:
    """Create a new inbox and set it as the default connector inbox.

    Use when:
    - No default inbox is configured yet.

    Do not use when:
    - You already know an existing inbox id (use codex_agentmail_set_default_inbox).
    """
    payload: Dict[str, Any] = {}
    if username:
        payload["username"] = username
    if domain:
        payload["domain"] = domain
    if display_name:
        payload["display_name"] = display_name
    if client_id:
        payload["client_id"] = client_id

    created = await _request("POST", "/v0/inboxes", json_body=payload)
    if not created.get("success"):
        return created

    inbox = _extract_inbox(created)
    inbox_id = inbox.get("inbox_id")
    if not inbox_id:
        return _fail(
            "Inbox was created but id was not found in response",
            detail=created.get("data"),
            try_instead=["codex_agentmail_list_inboxes(limit=5)", "codex_agentmail_set_default_inbox(inbox_id=...)"],
        )

    state = _load_state()
    state.update(inbox)
    if persist:
        ok, err = _save_state(state)
        if not ok:
            return _fail(
                "Created inbox but failed to persist connector state",
                inbox_id=inbox_id,
                detail=err,
                next_steps=[f"Set CODEX_AGENTMAIL_INBOX_ID={inbox_id}", "Fix write permissions for state file"],
            )

    return _ok(
        inbox_id=inbox_id,
        email=inbox.get("email"),
        username=inbox.get("username"),
        domain=inbox.get("domain"),
        persisted=bool(persist),
        state_file=str(CONNECTOR_STATE_FILE),
    )


@mcp.tool()
def codex_agentmail_set_default_inbox(
    inbox_id: str,
    email: Optional[str] = None,
    username: Optional[str] = None,
    domain: Optional[str] = None,
    persist: bool = True,
) -> Dict[str, Any]:
    """Set the default inbox id used by read/send tools.

    Args:
        inbox_id: AgentMail inbox identifier.
        persist: If true, stores selection in `.codex-agentmail.json`.
    """
    inbox_id = (inbox_id or "").strip()
    if not inbox_id:
        return _fail("Missing inbox_id", next_steps=["Call codex_agentmail_list_inboxes(limit=5) first"])

    state = _load_state()
    state["inbox_id"] = inbox_id
    if email:
        state["email"] = email
    if username:
        state["username"] = username
    if domain:
        state["domain"] = domain

    if persist:
        ok, err = _save_state(state)
        if not ok:
            return _fail(
                "Failed to persist default inbox",
                detail=err,
                next_steps=[f"Set CODEX_AGENTMAIL_INBOX_ID={inbox_id} in environment as fallback"],
            )

    return _ok(inbox_id=inbox_id, persisted=bool(persist), state_file=str(CONNECTOR_STATE_FILE))


@mcp.tool()
async def codex_agentmail_list_messages(
    limit: int = 20,
    inbox_id: Optional[str] = None,
    labels: Optional[str | List[str]] = None,
    include_spam: bool = False,
) -> Dict[str, Any]:
    """List messages for the default inbox (or explicit inbox_id)."""
    if limit < 1 or limit > 200:
        return _fail("limit must be 1..200", provided=limit)

    resolved_inbox, _state = _resolve_default_inbox(inbox_id)
    if not resolved_inbox:
        return _fail(
            "No default inbox configured",
            try_instead=["codex_agentmail_bootstrap_default_inbox()", "codex_agentmail_set_default_inbox(inbox_id=...)"],
            next_steps=["Create or select an inbox", "Retry message listing"],
        )

    label_param = _to_addr_list(labels)
    params: Dict[str, Any] = {"limit": limit, "include_spam": "true" if include_spam else "false"}
    if label_param is not None:
        params["labels"] = label_param

    return await _request("GET", f"/v0/inboxes/{resolved_inbox}/messages", params=params)


@mcp.tool()
async def codex_agentmail_get_message(message_id: str, inbox_id: Optional[str] = None) -> Dict[str, Any]:
    """Read one message from the default inbox (or explicit inbox_id)."""
    if not message_id:
        return _fail("Missing message_id")

    resolved_inbox, _state = _resolve_default_inbox(inbox_id)
    if not resolved_inbox:
        return _fail(
            "No default inbox configured",
            try_instead=["codex_agentmail_bootstrap_default_inbox()", "codex_agentmail_set_default_inbox(inbox_id=...)"],
        )

    return await _request("GET", f"/v0/inboxes/{resolved_inbox}/messages/{message_id}")


@mcp.tool()
async def codex_agentmail_send_message(
    to: str | List[str],
    subject: str,
    text: Optional[str] = None,
    html: Optional[str] = None,
    cc: Optional[str | List[str]] = None,
    bcc: Optional[str | List[str]] = None,
    reply_to: Optional[str] = None,
    inbox_id: Optional[str] = None,
    attachment_paths: Optional[str | List[str]] = None,
    attachments: Optional[List[Dict[str, Any]]] = None,
) -> Dict[str, Any]:
    """Send email from the default inbox.

    Use when:
    - You need outbound email from Codex default inbox.

    Do not use when:
    - You need raw MIME control (use lower-level agentmail tool).

    Note:
    - CC routing is disabled in this wrapper. Send separate messages instead.
    """
    if not subject:
        return _fail("Missing subject")
    if not text and not html:
        return _fail("Provide text or html body")
    if cc is not None:
        return _fail(
            "CC is not supported by codex_agentmail_send_message",
            next_steps=[
                "Send the primary email to the main recipient",
                "Send a second email directly to the would-be CC recipient",
            ],
        )

    resolved_inbox, _state = _resolve_default_inbox(inbox_id)
    if not resolved_inbox:
        return _fail(
            "No default inbox configured",
            try_instead=["codex_agentmail_bootstrap_default_inbox()", "codex_agentmail_set_default_inbox(inbox_id=...)"],
        )

    payload: Dict[str, Any] = {
        "to": _to_addr_list(to),
        "subject": subject,
    }
    if text is not None:
        payload["text"] = text
    if html is not None:
        payload["html"] = html
    if bcc is not None:
        payload["bcc"] = _to_addr_list(bcc)
    if reply_to is not None:
        payload["reply_to"] = reply_to

    # File-based attachments: load from disk server-side and encode to base64 for API.
    # This keeps binary data out of LLM chat context.
    built_attachments: List[Dict[str, Any]] = []
    for p in _normalize_path_list(attachment_paths):
        try:
            built_attachments.append(_attachment_from_file(p))
        except Exception as exc:
            return _fail(
                "Failed to load attachment file",
                detail=str(exc),
                file=p,
                next_steps=[
                    "Verify file exists and is readable",
                    "Use an absolute path when possible",
                ],
            )

    if attachments:
        built_attachments.extend(attachments)
    if built_attachments:
        payload["attachments"] = built_attachments

    return await _request("POST", f"/v0/inboxes/{resolved_inbox}/messages/send", json_body=payload)


if __name__ == "__main__":
    mcp.run(transport="stdio")
