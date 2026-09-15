"""Tool handlers for the Nemesis8 Hermes plugin.

Handles communication with the Nemesis8 control plane gateway over its REST API,
adhering to the existing n8 gateway contract. Returns JSON strings, gracefully
handles offline gateway conditions, performs input validation, and accepts **kwargs
for Hermes forward compatibility.
"""

import json
import logging
import os
import socket
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Dict, Optional

logger = logging.getLogger(__name__)

DEFAULT_GATEWAY = "http://host.docker.internal:9801"
DEFAULT_TIMEOUT_SECS = 15.0


def resolve_default_gateway() -> str:
    """Resolve the host gateway alias for Docker or Podman runtime environments.

    Mirrors the candidate resolution order in n8gw and nemesis8-entry:
    checks host.docker.internal, host.containers.internal, and falls back
    to DEFAULT_GATEWAY.
    """
    for alias in ("host.docker.internal", "host.containers.internal"):
        try:
            sockaddr = socket.getaddrinfo(alias, 9801, socket.AF_UNSPEC, socket.SOCK_STREAM)
            if sockaddr:
                return f"http://{alias}:9801"
        except (socket.gaierror, OSError):
            continue
    return DEFAULT_GATEWAY


def get_gateway_url() -> str:
    """Return the configured or discovered base URL of the Nemesis8 gateway."""
    url = os.environ.get("GATEWAY_URL", "").strip()
    if url:
        return url.rstrip("/")
    return resolve_default_gateway().rstrip("/")


def validate_gateway_url(url: str) -> Optional[str]:
    """Validate configured gateway URL. Returns error description if invalid, None if valid."""
    if not url or not isinstance(url, str):
        return "GATEWAY_URL is empty or not a string"
    try:
        parsed = urllib.parse.urlparse(url)
        if parsed.scheme not in ("http", "https") or not parsed.netloc:
            return f"Invalid GATEWAY_URL '{url}': must include http:// or https:// scheme and valid host"
    except Exception as e:
        return f"Invalid GATEWAY_URL '{url}': {e}"
    return None


def get_auth_token() -> Optional[str]:
    """Return the optional Bearer authentication token from the environment."""
    token = os.environ.get("NEMESIS8_AUTH_TOKEN", "").strip()
    return token if token else None


def _down_response(gateway_url: str) -> Dict[str, Any]:
    """Return a calm, structured offline message when the gateway is unreachable."""
    return {
        "status": "gateway_not_running",
        "gateway_url": gateway_url,
        "message": (
            f"The nemesis8 gateway is not running at {gateway_url} — so there's "
            "nothing to manage yet (no error, just not up). Start it with "
            "`n8 serve --background`, or from the control room: Gateway ▸ Start. "
            "Set GATEWAY_URL if it's on a different host/port."
        ),
    }


def _gateway_request(
    method: str,
    path: str,
    body: Optional[Dict[str, Any]] = None,
    timeout: float = DEFAULT_TIMEOUT_SECS,
) -> Dict[str, Any]:
    """Execute an HTTP request against the Nemesis8 gateway.

    Catches network errors gracefully and formats responses as dictionaries.
    """
    base = get_gateway_url()
    url_err = validate_gateway_url(base)
    if url_err:
        return {"status": "error", "error": url_err}

    url = f"{base}{path}"

    headers = {
        "Accept": "application/json",
        "User-Agent": "nemesis8-hermes-plugin/0.1.0",
    }
    token = get_auth_token()
    if token:
        headers["Authorization"] = f"Bearer {token}"

    data_bytes = None
    if body is not None:
        headers["Content-Type"] = "application/json"
        data_bytes = json.dumps(body).encode("utf-8")

    req = urllib.request.Request(url, data=data_bytes, headers=headers, method=method)

    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            resp_text = resp.read().decode("utf-8", errors="replace")
            if not resp_text.strip():
                # Match n8gw: a successful response may have no JSON body.
                return {"ok": True}
            try:
                parsed = json.loads(resp_text)
                if isinstance(parsed, dict):
                    return parsed
                return {"status": "ok", "data": parsed}
            except json.JSONDecodeError:
                return {
                    "status": "error",
                    "error": f"Invalid non-JSON response from gateway: {resp_text[:400]}",
                }

    except urllib.error.HTTPError as e:
        err_body = ""
        try:
            err_body = e.read().decode("utf-8", errors="replace")[:400]
        except Exception:
            pass
        return {
            "status": "error",
            "http_code": e.code,
            "error": f"HTTP {e.code}: {err_body or e.reason}",
        }

    except (urllib.error.URLError, socket.timeout, ConnectionError, OSError) as e:
        logger.debug("Nemesis8 gateway offline (%s): %s", url, e)
        return _down_response(base)

    except Exception as e:
        logger.error("Unexpected error contacting Nemesis8 gateway: %s", e)
        return {
            "status": "error",
            "error": f"Internal client error: {e}",
        }


def gateway_status(args: Optional[Dict[str, Any]] = None, **kwargs) -> str:
    """Check the status of the Nemesis8 gateway."""
    res = _gateway_request("GET", "/status")
    return json.dumps(res)


def agent_list(args: Optional[Dict[str, Any]] = None, **kwargs) -> str:
    """List all agents registered in the Nemesis8 control plane."""
    res = _gateway_request("GET", "/agents")
    return json.dumps(res)


def agent_spawn(args: Optional[Dict[str, Any]] = None, **kwargs) -> str:
    """Spawn a new agent container to execute a prompt."""
    if not isinstance(args, dict):
        return json.dumps({
            "status": "error",
            "error": "Missing or invalid arguments object: expected dict",
        })

    prompt = args.get("prompt")
    if not isinstance(prompt, str) or not prompt.strip():
        return json.dumps({
            "status": "error",
            "error": "Missing or empty required string parameter: 'prompt'",
        })

    payload: Dict[str, Any] = {"prompt": prompt.strip()}
    for key in ("provider", "model", "workspace"):
        if key in args and args[key] is not None:
            val = args[key]
            if not isinstance(val, str) or not val.strip():
                return json.dumps({
                    "status": "error",
                    "error": f"Invalid type for parameter '{key}': expected non-empty string",
                })
            payload[key] = val.strip()

    res = _gateway_request("POST", "/agents/spawn", body=payload)
    return json.dumps(res)


def agent_stop(args: Optional[Dict[str, Any]] = None, **kwargs) -> str:
    """Stop an agent container by its ID."""
    if not isinstance(args, dict):
        return json.dumps({
            "status": "error",
            "error": "Missing or invalid arguments object: expected dict",
        })

    agent_id = args.get("id")
    if not isinstance(agent_id, str) or not agent_id.strip():
        return json.dumps({
            "status": "error",
            "error": "Missing or empty required string parameter: 'id'",
        })

    agent_id = agent_id.strip()
    quoted_id = urllib.parse.quote(agent_id, safe="")
    res = _gateway_request("POST", f"/agents/{quoted_id}/kill", body={})
    return json.dumps(res)


def agent_kill(args: Optional[Dict[str, Any]] = None, **kwargs) -> str:
    """Alias for agent_stop."""
    return agent_stop(args, **kwargs)
