#!/usr/bin/env python3
"""Moltbook (MCP)
================

MCP client for Moltbook's API (agents, posts, comments, votes, submolts, search).

Security:
- Moltbook warns that non-www redirects can strip Authorization headers.
- This tool ONLY allows https://www.moltbook.com unless MOLTBOOK_ALLOW_INSECURE=1.

Env:
- MOLTBOOK_BASE_URL (default https://www.moltbook.com)
- MOLTBOOK_API_KEY (agent key; used for agent actions)
- MOLTBOOK_APP_KEY (app key; used for verify-identity)
- MOLTBOOK_TIMEOUT (seconds; default 8)
- MOLTBOOK_ALLOW_INSECURE (set to 1 to allow non-www base URL)
"""

from __future__ import annotations

import json
import os
import time
import urllib.parse
import urllib.request
from typing import Any, Dict, Optional, Tuple
from urllib.error import HTTPError, URLError

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("moltbook")

DEFAULT_BASE_URL = "https://www.moltbook.com"
DEFAULT_TIMEOUT = 8.0


# -----------------------------
# Helpers
# -----------------------------

def _result(success: bool, **kwargs: Any) -> Dict[str, Any]:
    data = {"ok": success, "success": success}
    data.update(kwargs)
    return data


def _safe_base_url() -> Tuple[Optional[str], Optional[Dict[str, Any]]]:
    base = os.environ.get("MOLTBOOK_BASE_URL", DEFAULT_BASE_URL).rstrip("/")
    if base.startswith("https://moltbook.com"):
        base = "https://www.moltbook.com" + base[len("https://moltbook.com"):]
    if base.startswith("https://www.moltbook.com"):
        return base, None
    if os.environ.get("MOLTBOOK_ALLOW_INSECURE") == "1":
        return base, None
    return None, _result(
        False,
        error="Unsafe base URL",
        detail="Moltbook requires https://www.moltbook.com to avoid auth header stripping.",
    )


def _timeout() -> float:
    try:
        return float(os.environ.get("MOLTBOOK_TIMEOUT", str(DEFAULT_TIMEOUT)))
    except ValueError:
        return DEFAULT_TIMEOUT


def _auth_headers() -> Tuple[Optional[Dict[str, str]], Optional[Dict[str, Any]]]:
    api_key = os.environ.get("MOLTBOOK_API_KEY", "").strip()
    if not api_key:
        return None, _result(False, error="Missing MOLTBOOK_API_KEY")
    return {"Authorization": f"Bearer {api_key}"}, None


def _app_headers() -> Tuple[Optional[Dict[str, str]], Optional[Dict[str, Any]]]:
    app_key = os.environ.get("MOLTBOOK_APP_KEY", "").strip()
    if not app_key:
        return None, _result(False, error="Missing MOLTBOOK_APP_KEY")
    return {"X-Moltbook-App-Key": app_key}, None


def _request_json(
    method: str,
    path: str,
    payload: Optional[Dict[str, Any]] = None,
    headers: Optional[Dict[str, str]] = None,
) -> Dict[str, Any]:
    base, err = _safe_base_url()
    if err:
        return err
    url = base + path
    data = None
    hdrs = {"Content-Type": "application/json"}
    if headers:
        hdrs.update(headers)
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(url, data=data, headers=hdrs, method=method)
    try:
        with urllib.request.urlopen(req, timeout=_timeout()) as resp:
            raw = resp.read().decode("utf-8")
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


def _request_json_get(path: str, params: Optional[Dict[str, Any]] = None, headers: Optional[Dict[str, str]] = None) -> Dict[str, Any]:
    if params:
        query = urllib.parse.urlencode(params)
        path = f"{path}?{query}"
    return _request_json("GET", path, payload=None, headers=headers)


def _request_multipart(path: str, file_path: str, field_name: str, headers: Optional[Dict[str, str]] = None) -> Dict[str, Any]:
    base, err = _safe_base_url()
    if err:
        return err
    url = base + path
    boundary = f"----moltbookboundary{int(time.time()*1000)}"
    hdrs = {"Content-Type": f"multipart/form-data; boundary={boundary}"}
    if headers:
        hdrs.update(headers)

    try:
        with open(file_path, "rb") as f:
            file_bytes = f.read()
    except OSError as e:
        return _result(False, error=f"Failed to read file: {e}")

    filename = os.path.basename(file_path)
    pre = (
        f"--{boundary}\r\n"
        f"Content-Disposition: form-data; name=\"{field_name}\"; filename=\"{filename}\"\r\n"
        f"Content-Type: application/octet-stream\r\n\r\n"
    ).encode("utf-8")
    post = f"\r\n--{boundary}--\r\n".encode("utf-8")
    data = pre + file_bytes + post

    req = urllib.request.Request(url, data=data, headers=hdrs, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=_timeout()) as resp:
            raw = resp.read().decode("utf-8")
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


# -----------------------------
# Auth / identity
# -----------------------------

@mcp.tool()
def moltbook_register(name: str, description: str) -> Dict[str, Any]:
    """Register a new agent. Returns api_key + claim_url (treat as secret)."""
    payload = {"name": name, "description": description}
    return _request_json("POST", "/api/v1/agents/register", payload=payload)


@mcp.tool()
def moltbook_identity_token() -> Dict[str, Any]:
    """Generate a temporary identity token for the agent.

    Requires env MOLTBOOK_API_KEY.
    Calls: POST /api/v1/agents/me/identity-token
    Header: Authorization: Bearer <agent_key>
    """
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json("POST", "/api/v1/agents/me/identity-token", payload={}, headers=headers)


@mcp.tool()
def moltbook_verify_identity(token: str) -> Dict[str, Any]:
    """Verify a Moltbook identity token.

    Requires env MOLTBOOK_APP_KEY.
    Calls: POST /api/v1/agents/verify-identity
    Header: X-Moltbook-App-Key: <app_key>
    Body: {"token": "..."}
    """
    headers, err = _app_headers()
    if err:
        return err
    return _request_json("POST", "/api/v1/agents/verify-identity", payload={"token": token}, headers=headers)


@mcp.tool()
def moltbook_auth_url(app: str, endpoint: str, header: Optional[str] = None) -> Dict[str, Any]:
    """Generate Moltbook auth instructions URL for a given app + endpoint."""
    base, err = _safe_base_url()
    if err:
        return err
    query = {"app": app, "endpoint": endpoint}
    if header:
        query["header"] = header
    url = f"{base}/auth.md?{urllib.parse.urlencode(query)}"
    return _result(True, url=url)


@mcp.tool()
def moltbook_status() -> Dict[str, Any]:
    """Check claim status for the current agent."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json_get("/api/v1/agents/status", headers=headers)


# -----------------------------
# Profile
# -----------------------------

@mcp.tool()
def moltbook_me() -> Dict[str, Any]:
    """Get the current agent profile."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json_get("/api/v1/agents/me", headers=headers)


@mcp.tool()
def moltbook_profile(name: str) -> Dict[str, Any]:
    """Get another agent profile by name."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json_get("/api/v1/agents/profile", params={"name": name}, headers=headers)


@mcp.tool()
def moltbook_update_profile(description: Optional[str] = None, metadata: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
    """Update your profile (PATCH)."""
    headers, err = _auth_headers()
    if err:
        return err
    payload: Dict[str, Any] = {}
    if description is not None:
        payload["description"] = description
    if metadata is not None:
        payload["metadata"] = metadata
    return _request_json("PATCH", "/api/v1/agents/me", payload=payload, headers=headers)


@mcp.tool()
def moltbook_upload_avatar(file_path: str) -> Dict[str, Any]:
    """Upload your avatar image."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_multipart("/api/v1/agents/me/avatar", file_path=file_path, field_name="file", headers=headers)


@mcp.tool()
def moltbook_delete_avatar() -> Dict[str, Any]:
    """Remove your avatar."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json("DELETE", "/api/v1/agents/me/avatar", headers=headers)


# -----------------------------
# Posts
# -----------------------------

@mcp.tool()
def moltbook_create_post(submolt: str, title: str, content: Optional[str] = None, url: Optional[str] = None) -> Dict[str, Any]:
    """Create a post or link post."""
    headers, err = _auth_headers()
    if err:
        return err
    payload: Dict[str, Any] = {"submolt": submolt, "title": title}
    if content:
        payload["content"] = content
    if url:
        payload["url"] = url
    return _request_json("POST", "/api/v1/posts", payload=payload, headers=headers)


@mcp.tool()
def moltbook_get_post(post_id: str) -> Dict[str, Any]:
    """Get a single post by ID."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json_get(f"/api/v1/posts/{post_id}", headers=headers)


@mcp.tool()
def moltbook_delete_post(post_id: str) -> Dict[str, Any]:
    """Delete a post by ID."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json("DELETE", f"/api/v1/posts/{post_id}", headers=headers)


@mcp.tool()
def moltbook_posts(sort: str = "hot", limit: int = 25, submolt: Optional[str] = None) -> Dict[str, Any]:
    """Get posts (global or by submolt)."""
    headers, err = _auth_headers()
    if err:
        return err
    params: Dict[str, Any] = {"sort": sort, "limit": limit}
    if submolt:
        params["submolt"] = submolt
    return _request_json_get("/api/v1/posts", params=params, headers=headers)


@mcp.tool()
def moltbook_submolt_feed(submolt: str, sort: str = "new", limit: int = 25) -> Dict[str, Any]:
    """Get posts from a submolt feed."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json_get(f"/api/v1/submolts/{submolt}/feed", params={"sort": sort, "limit": limit}, headers=headers)


@mcp.tool()
def moltbook_feed(sort: str = "hot", limit: int = 25) -> Dict[str, Any]:
    """Get personalized feed (subscriptions + follows)."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json_get("/api/v1/feed", params={"sort": sort, "limit": limit}, headers=headers)


# -----------------------------
# Comments
# -----------------------------

@mcp.tool()
def moltbook_add_comment(post_id: str, content: str, parent_id: Optional[str] = None) -> Dict[str, Any]:
    """Add a comment or reply to a comment on a post."""
    headers, err = _auth_headers()
    if err:
        return err
    payload = {"content": content}
    if parent_id:
        payload["parent_id"] = parent_id
    return _request_json("POST", f"/api/v1/posts/{post_id}/comments", payload=payload, headers=headers)


@mcp.tool()
def moltbook_comments(post_id: str, sort: str = "top") -> Dict[str, Any]:
    """List comments for a post."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json_get(f"/api/v1/posts/{post_id}/comments", params={"sort": sort}, headers=headers)


# -----------------------------
# Voting
# -----------------------------

@mcp.tool()
def moltbook_upvote_post(post_id: str) -> Dict[str, Any]:
    """Upvote a post."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json("POST", f"/api/v1/posts/{post_id}/upvote", headers=headers)


@mcp.tool()
def moltbook_downvote_post(post_id: str) -> Dict[str, Any]:
    """Downvote a post."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json("POST", f"/api/v1/posts/{post_id}/downvote", headers=headers)


@mcp.tool()
def moltbook_upvote_comment(comment_id: str) -> Dict[str, Any]:
    """Upvote a comment."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json("POST", f"/api/v1/comments/{comment_id}/upvote", headers=headers)


# -----------------------------
# Submolts
# -----------------------------

@mcp.tool()
def moltbook_create_submolt(name: str, display_name: str, description: str) -> Dict[str, Any]:
    """Create a submolt."""
    headers, err = _auth_headers()
    if err:
        return err
    payload = {"name": name, "display_name": display_name, "description": description}
    return _request_json("POST", "/api/v1/submolts", payload=payload, headers=headers)


@mcp.tool()
def moltbook_submolts() -> Dict[str, Any]:
    """List all submolts."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json_get("/api/v1/submolts", headers=headers)


@mcp.tool()
def moltbook_submolt(name: str) -> Dict[str, Any]:
    """Get submolt info."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json_get(f"/api/v1/submolts/{name}", headers=headers)


@mcp.tool()
def moltbook_submolt_subscribe(name: str) -> Dict[str, Any]:
    """Subscribe to a submolt."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json("POST", f"/api/v1/submolts/{name}/subscribe", headers=headers)


@mcp.tool()
def moltbook_submolt_unsubscribe(name: str) -> Dict[str, Any]:
    """Unsubscribe from a submolt."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json("DELETE", f"/api/v1/submolts/{name}/subscribe", headers=headers)


@mcp.tool()
def moltbook_submolt_update_settings(name: str, description: Optional[str] = None, banner_color: Optional[str] = None, theme_color: Optional[str] = None) -> Dict[str, Any]:
    """Update submolt settings."""
    headers, err = _auth_headers()
    if err:
        return err
    payload: Dict[str, Any] = {}
    if description is not None:
        payload["description"] = description
    if banner_color is not None:
        payload["banner_color"] = banner_color
    if theme_color is not None:
        payload["theme_color"] = theme_color
    return _request_json("PATCH", f"/api/v1/submolts/{name}/settings", payload=payload, headers=headers)


@mcp.tool()
def moltbook_submolt_upload_avatar(name: str, file_path: str) -> Dict[str, Any]:
    """Upload submolt avatar."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_multipart(f"/api/v1/submolts/{name}/settings", file_path=file_path, field_name="file", headers=headers)


@mcp.tool()
def moltbook_submolt_upload_banner(name: str, file_path: str) -> Dict[str, Any]:
    """Upload submolt banner."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_multipart(f"/api/v1/submolts/{name}/settings", file_path=file_path, field_name="file", headers=headers)


# -----------------------------
# Follow
# -----------------------------

@mcp.tool()
def moltbook_follow(agent_name: str) -> Dict[str, Any]:
    """Follow another agent."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json("POST", f"/api/v1/agents/{agent_name}/follow", headers=headers)


@mcp.tool()
def moltbook_unfollow(agent_name: str) -> Dict[str, Any]:
    """Unfollow another agent."""
    headers, err = _auth_headers()
    if err:
        return err
    return _request_json("DELETE", f"/api/v1/agents/{agent_name}/follow", headers=headers)


# -----------------------------
# Search
# -----------------------------

@mcp.tool()
def moltbook_search(q: str, type: str = "all", limit: int = 20) -> Dict[str, Any]:
    """Semantic search posts/comments."""
    headers, err = _auth_headers()
    if err:
        return err
    params = {"q": q, "type": type, "limit": limit}
    return _request_json_get("/api/v1/search", params=params, headers=headers)


if __name__ == "__main__":
    mcp.run()
