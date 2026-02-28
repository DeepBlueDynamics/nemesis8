#!/usr/bin/env python3
"""MCP: agent-chat

Generic tool to chat with Claude agents with different roles.
"""

from __future__ import annotations

import json
import os
import sys
from typing import Any, Dict, List, Optional, Tuple
import random
import shutil
import subprocess
import urllib.parse
import urllib.request
from urllib.error import HTTPError, URLError

try:
    import anthropic
    ANTHROPIC_AVAILABLE = True
except ImportError:
    anthropic = None
    ANTHROPIC_AVAILABLE = False

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("agent-chat")

# Message relay defaults (tiny HTTP service)
# Use host.docker.internal by default since the relay typically runs on the host.
RELAY_BASE_URL = os.getenv("RELAY_BASE_URL", "http://host.docker.internal:8099")
RELAY_TIMEOUT = float(os.getenv("RELAY_TIMEOUT", "8"))

# Fallback when a user hasn't set RELAY_BASE_URL and localhost was attempted
_RELAY_FALLBACK_URL = "http://host.docker.internal:8099"
_RELAY_LOCALHOST = "http://localhost:8099"

# Default model
DEFAULT_MODEL = os.getenv("AGENT_CHAT_MODEL", "claude-sonnet-4-5-20250929")
DEFAULT_OPENAI_MODEL = os.getenv("AGENT_CHAT_OPENAI_MODEL", "gpt-4o")
DEFAULT_GEMINI_MODEL = os.getenv("AGENT_CHAT_GEMINI_MODEL", "gemini-2.5-flash")
DEFAULT_OLLAMA_MODEL = os.getenv("AGENT_CHAT_OLLAMA_MODEL", "qwen2.5-coder:latest")
DEFAULT_CODEX_MODEL = os.getenv("AGENT_CHAT_CODEX_MODEL", "gpt-5-codex")
DEFAULT_OLLAMA_BASE_URL = os.getenv("OLLAMA_BASE_URL", "http://localhost:11434")
SUPPORTED_PROVIDERS = {"anthropic", "openai", "gemini", "ollama"}

# Compact hexagram table for quick persona seeding
HEXAGRAMS = [
    (1, "Ch'ien", "Creative power, leadership, initiating"),
    (2, "K'un", "Receptive, nurturing, adaptive"),
    (3, "Chun", "Difficulty at the beginning, growth through challenge"),
    (5, "Hsü", "Waiting with preparation, timing matters"),
    (6, "Sung", "Conflict, resolution through clarity"),
    (11, "T'ai", "Peace, balance, harmonious order"),
    (12, "P'i", "Standstill, stagnation, conserve energy"),
    (26, "Ta Ch'u", "Taming power of the great, restrained strength"),
    (43, "Kuai", "Breakthrough, decisive action"),
    (46, "Shêng", "Pushing upward, steady ascent")
]


def _normalize_provider(provider: Optional[str]) -> str:
    p = (provider or "anthropic").strip().lower()
    aliases = {
        "claude": "anthropic",
        "anthropic": "anthropic",
        "openai": "openai",
        "codex": "openai",
        "gemini": "gemini",
        "google": "gemini",
        "ollama": "ollama",
        "local": "ollama",
    }
    normalized = aliases.get(p, p)
    if normalized not in SUPPORTED_PROVIDERS:
        raise ValueError(
            f"Unsupported provider '{provider}'. Supported: {sorted(SUPPORTED_PROVIDERS)}"
        )
    return normalized


def _estimate_tokens(text: str) -> int:
    # Conservative rough token estimator for budget checks.
    return max(1, len(text) // 4)


def _clip_to_token_budget(text: str, max_tokens: int) -> str:
    if max_tokens <= 0:
        return ""
    approx_chars = max_tokens * 4
    if len(text) <= approx_chars:
        return text
    return text[:approx_chars]


def _apply_context_budget(system_prompt: str, user_prompt: str, max_tokens: int) -> Tuple[str, str]:
    # Keep a safe input/output split: reserve half for output by default.
    total = max(512, max_tokens * 2)
    output_budget = max(256, max_tokens)
    safety = max(128, total // 10)
    input_budget = max(256, total - output_budget - safety)

    system_budget = max(128, input_budget // 3)
    user_budget = max(128, input_budget - system_budget)
    return (
        _clip_to_token_budget(system_prompt, system_budget),
        _clip_to_token_budget(user_prompt, user_budget),
    )


def _http_json(
    url: str,
    method: str = "GET",
    headers: Optional[Dict[str, str]] = None,
    payload: Optional[Dict[str, Any]] = None,
    timeout: float = 30.0,
) -> Dict[str, Any]:
    request_headers = {"Content-Type": "application/json"}
    if headers:
        request_headers.update(headers)
    data = None
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(url, data=data, headers=request_headers, method=method.upper())
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        raw = resp.read().decode("utf-8")
    return json.loads(raw) if raw else {}


def _extract_text(value: Any) -> str:
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        chunks: List[str] = []
        for item in value:
            if isinstance(item, str):
                chunks.append(item)
            elif isinstance(item, dict):
                txt = item.get("text")
                if isinstance(txt, str):
                    chunks.append(txt)
        return "\n".join(chunks)
    if isinstance(value, dict):
        txt = value.get("text")
        if isinstance(txt, str):
            return txt
    return ""


def _chat_anthropic(
    system_prompt: str,
    user_prompt: str,
    model: Optional[str],
    max_tokens: int,
) -> Dict[str, Any]:
    if not ANTHROPIC_AVAILABLE:
        return {"success": False, "error": "anthropic package not available", "provider": "anthropic"}
    api_key = os.environ.get("ANTHROPIC_API_KEY")
    if not api_key:
        return {
            "success": False,
            "error": "ANTHROPIC_API_KEY not set",
            "provider": "anthropic",
            "tip": "Set ANTHROPIC_API_KEY in environment.",
        }

    model_name = model or DEFAULT_MODEL
    try:
        client = anthropic.Anthropic(api_key=api_key)
        msg = client.messages.create(
            model=model_name,
            max_tokens=max_tokens,
            system=system_prompt,
            messages=[{"role": "user", "content": user_prompt}],
        )
        return {
            "success": True,
            "provider": "anthropic",
            "model": msg.model,
            "text": _extract_text(msg.content),
            "usage": {
                "input_tokens": getattr(msg.usage, "input_tokens", None),
                "output_tokens": getattr(msg.usage, "output_tokens", None),
            },
        }
    except Exception as exc:  # noqa: BLE001
        return {"success": False, "provider": "anthropic", "error": str(exc)}


def _chat_openai(
    system_prompt: str,
    user_prompt: str,
    model: Optional[str],
    max_tokens: int,
) -> Dict[str, Any]:
    api_key = os.environ.get("OPENAI_API_KEY")
    if not api_key:
        return {
            "success": False,
            "error": "OPENAI_API_KEY not set",
            "provider": "openai",
            "tip": "Set OPENAI_API_KEY in environment.",
        }
    model_name = model or DEFAULT_OPENAI_MODEL
    url = "https://api.openai.com/v1/chat/completions"
    payload = {
        "model": model_name,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_prompt},
        ],
        "max_tokens": max_tokens,
        "temperature": 0.2,
    }
    try:
        data = _http_json(
            url=url,
            method="POST",
            headers={"Authorization": f"Bearer {api_key}"},
            payload=payload,
            timeout=60.0,
        )
        choice = (data.get("choices") or [{}])[0]
        message = choice.get("message") or {}
        content = message.get("content", "")
        usage = data.get("usage") or {}
        return {
            "success": True,
            "provider": "openai",
            "model": data.get("model", model_name),
            "text": _extract_text(content),
            "usage": {
                "input_tokens": usage.get("prompt_tokens"),
                "output_tokens": usage.get("completion_tokens"),
            },
        }
    except Exception as exc:  # noqa: BLE001
        return {"success": False, "provider": "openai", "error": str(exc)}


def _chat_gemini(
    system_prompt: str,
    user_prompt: str,
    model: Optional[str],
    max_tokens: int,
) -> Dict[str, Any]:
    api_key = os.environ.get("GOOGLE_API_KEY")
    if not api_key:
        return {
            "success": False,
            "error": "GOOGLE_API_KEY not set",
            "provider": "gemini",
            "tip": "Set GOOGLE_API_KEY in environment.",
        }
    model_name = model or DEFAULT_GEMINI_MODEL
    if not model_name.startswith("models/"):
        model_name = f"models/{model_name}"
    url = f"https://generativelanguage.googleapis.com/v1beta/{model_name}:generateContent?key={api_key}"
    payload = {
        "system_instruction": {"parts": [{"text": system_prompt}]},
        "contents": [{"role": "user", "parts": [{"text": user_prompt}]}],
        "generationConfig": {"maxOutputTokens": max_tokens, "temperature": 0.2},
    }
    try:
        data = _http_json(url=url, method="POST", payload=payload, timeout=60.0)
        candidates = data.get("candidates") or []
        if not candidates:
            return {
                "success": False,
                "provider": "gemini",
                "error": "No candidates returned",
                "raw": data,
            }
        parts = (
            (candidates[0].get("content") or {}).get("parts")
            or []
        )
        text = "\n".join(p.get("text", "") for p in parts if isinstance(p, dict))
        return {
            "success": True,
            "provider": "gemini",
            "model": model_name,
            "text": text,
            "usage": data.get("usageMetadata", {}),
        }
    except Exception as exc:  # noqa: BLE001
        return {"success": False, "provider": "gemini", "error": str(exc)}


def _chat_ollama(
    system_prompt: str,
    user_prompt: str,
    model: Optional[str],
    max_tokens: int,
) -> Dict[str, Any]:
    model_name = model or DEFAULT_OLLAMA_MODEL
    base = DEFAULT_OLLAMA_BASE_URL.rstrip("/")
    url = f"{base}/api/chat"
    payload = {
        "model": model_name,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_prompt},
        ],
        "stream": False,
        "options": {"num_predict": max_tokens},
    }
    try:
        data = _http_json(url=url, method="POST", payload=payload, timeout=120.0)
        message = data.get("message") or {}
        return {
            "success": True,
            "provider": "ollama",
            "model": model_name,
            "text": _extract_text(message.get("content", "")),
            "usage": {
                "prompt_eval_count": data.get("prompt_eval_count"),
                "eval_count": data.get("eval_count"),
            },
        }
    except Exception as exc:  # noqa: BLE001
        return {
            "success": False,
            "provider": "ollama",
            "error": str(exc),
            "tip": f"Ensure ollama is running and reachable at {base}",
        }


def _chat_dispatch(
    provider: str,
    system_prompt: str,
    user_prompt: str,
    model: Optional[str],
    max_tokens: int,
) -> Dict[str, Any]:
    provider_name = _normalize_provider(provider)
    system_prompt, user_prompt = _apply_context_budget(system_prompt, user_prompt, max_tokens)
    if provider_name == "anthropic":
        return _chat_anthropic(system_prompt, user_prompt, model, max_tokens)
    if provider_name == "openai":
        return _chat_openai(system_prompt, user_prompt, model, max_tokens)
    if provider_name == "gemini":
        return _chat_gemini(system_prompt, user_prompt, model, max_tokens)
    if provider_name == "ollama":
        return _chat_ollama(system_prompt, user_prompt, model, max_tokens)
    return {"success": False, "provider": provider_name, "error": "Unsupported provider"}


def _guess_capabilities(model_id: str) -> List[str]:
    mid = (model_id or "").lower()
    caps: List[str] = ["text"]
    if any(k in mid for k in ["vision", "image", "multimodal"]):
        caps.append("vision")
    if any(k in mid for k in ["audio", "speech", "tts"]):
        caps.append("audio")
    if any(k in mid for k in ["embed", "embedding"]):
        caps.append("embeddings")
    if any(k in mid for k in ["code", "coder", "codex"]):
        caps.append("code")
    return sorted(set(caps))


def _list_models_anthropic() -> Dict[str, Any]:
    api_key = os.environ.get("ANTHROPIC_API_KEY")
    if not api_key:
        return {
            "success": False,
            "provider": "anthropic",
            "error": "ANTHROPIC_API_KEY not set",
            "tip": "Set ANTHROPIC_API_KEY in environment.",
        }
    url = "https://api.anthropic.com/v1/models"
    headers = {
        "x-api-key": api_key,
        "anthropic-version": "2023-06-01",
    }
    try:
        data = _http_json(url=url, method="GET", headers=headers, timeout=30.0)
        items = data.get("data") or []
        models = []
        for item in items:
            model_id = item.get("id", "")
            models.append(
                {
                    "provider": "anthropic",
                    "id": model_id,
                    "label": item.get("display_name", model_id),
                    "capabilities": _guess_capabilities(model_id),
                    "context_window": item.get("context_window"),
                    "raw": item,
                }
            )
        return {"success": True, "provider": "anthropic", "models": models}
    except Exception as exc:  # noqa: BLE001
        return {"success": False, "provider": "anthropic", "error": str(exc)}


def _list_models_openai() -> Dict[str, Any]:
    api_key = os.environ.get("OPENAI_API_KEY")
    if not api_key:
        return {
            "success": False,
            "provider": "openai",
            "error": "OPENAI_API_KEY not set",
            "tip": "Set OPENAI_API_KEY in environment.",
        }
    url = "https://api.openai.com/v1/models"
    try:
        data = _http_json(
            url=url,
            method="GET",
            headers={"Authorization": f"Bearer {api_key}"},
            timeout=30.0,
        )
        items = data.get("data") or []
        models = []
        for item in items:
            model_id = item.get("id", "")
            models.append(
                {
                    "provider": "openai",
                    "id": model_id,
                    "label": model_id,
                    "capabilities": _guess_capabilities(model_id),
                    "context_window": None,
                    "raw": item,
                }
            )
        return {"success": True, "provider": "openai", "models": sorted(models, key=lambda m: m["id"])}
    except Exception as exc:  # noqa: BLE001
        return {"success": False, "provider": "openai", "error": str(exc)}


def _list_models_gemini() -> Dict[str, Any]:
    api_key = os.environ.get("GOOGLE_API_KEY")
    if not api_key:
        return {
            "success": False,
            "provider": "gemini",
            "error": "GOOGLE_API_KEY not set",
            "tip": "Set GOOGLE_API_KEY in environment.",
        }
    url = f"https://generativelanguage.googleapis.com/v1beta/models?key={api_key}"
    try:
        data = _http_json(url=url, method="GET", timeout=30.0)
        items = data.get("models") or []
        models = []
        for item in items:
            model_id = item.get("name", "")
            models.append(
                {
                    "provider": "gemini",
                    "id": model_id.replace("models/", ""),
                    "label": model_id,
                    "capabilities": _guess_capabilities(model_id),
                    "context_window": item.get("inputTokenLimit"),
                    "raw": item,
                }
            )
        return {"success": True, "provider": "gemini", "models": models}
    except Exception as exc:  # noqa: BLE001
        return {"success": False, "provider": "gemini", "error": str(exc)}


def _list_models_ollama() -> Dict[str, Any]:
    base = DEFAULT_OLLAMA_BASE_URL.rstrip("/")
    url = f"{base}/api/tags"
    try:
        data = _http_json(url=url, method="GET", timeout=10.0)
        items = data.get("models") or []
        models = []
        for item in items:
            model_id = item.get("name", "")
            models.append(
                {
                    "provider": "ollama",
                    "id": model_id,
                    "label": model_id,
                    "capabilities": _guess_capabilities(model_id),
                    "context_window": item.get("context_length"),
                    "raw": item,
                }
            )
        return {"success": True, "provider": "ollama", "models": models}
    except Exception as exc:  # noqa: BLE001
        return {
            "success": False,
            "provider": "ollama",
            "error": str(exc),
            "tip": f"Ensure ollama is running at {base}.",
        }


def _list_models(provider: str) -> Dict[str, Any]:
    provider_name = (provider or "all").strip().lower()
    if provider_name == "all":
        checks = [_list_models_anthropic, _list_models_openai, _list_models_gemini, _list_models_ollama]
        data = [fn() for fn in checks]
        return {
            "success": any(d.get("success") for d in data),
            "provider": "all",
            "models": [m for d in data if d.get("success") for m in d.get("models", [])],
            "errors": [d for d in data if not d.get("success")],
        }
    normalized = _normalize_provider(provider_name)
    mapping = {
        "anthropic": _list_models_anthropic,
        "openai": _list_models_openai,
        "gemini": _list_models_gemini,
        "ollama": _list_models_ollama,
    }
    return mapping[normalized]()


def _cast_hexagram() -> Dict[str, object]:
    """Lightweight I Ching-style casting for persona seeding."""
    num, name, meaning = random.choice(HEXAGRAMS)
    return {"number": num, "name": name, "meaning": meaning}


def _relay_result(success: bool, **kwargs: Any) -> Dict[str, Any]:
    payload = {"success": success}
    payload.update(kwargs)
    return payload


def _relay_request(
    path: str,
    payload: Optional[Dict[str, Any]] = None,
    method: str = "POST",
    params: Optional[Dict[str, Any]] = None,
    base_url: Optional[str] = None,
    timeout: Optional[float] = None,
) -> Dict[str, Any]:
    base = (base_url or RELAY_BASE_URL).rstrip("/")
    url = f"{base}{path}"
    method = method.upper()

    query = params if params is not None else (payload if method in {"GET", "HEAD"} else None)
    if query:
        url = f"{url}?{urllib.parse.urlencode(query, doseq=True)}"

    data = None
    headers = {"Content-Type": "application/json"}
    if payload is not None and method not in {"GET", "HEAD"}:
        data = json.dumps(payload).encode("utf-8")

    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout or RELAY_TIMEOUT) as resp:
            raw = resp.read().decode("utf-8")
        if not raw:
            return _relay_result(True, url=url)
        parsed = json.loads(raw)
        if isinstance(parsed, dict):
            parsed.setdefault("success", True)
            parsed.setdefault("url", url)
            return parsed
        return _relay_result(True, data=parsed, url=url)
    except HTTPError as exc:
        detail = None
        try:
            detail = exc.read().decode("utf-8")
        except Exception:
            detail = None
        return _relay_result(False, url=url, status=exc.code, error=str(exc), detail=detail)
    except URLError as exc:
        # Auto-fallback from localhost to host.docker.internal if not explicitly set.
        if base_url is None and RELAY_BASE_URL.startswith(_RELAY_LOCALHOST):
            fallback = _RELAY_FALLBACK_URL.rstrip("/")
            retry_url = f"{fallback}{path}"
            if query:
                retry_url = f"{retry_url}?{urllib.parse.urlencode(query, doseq=True)}"
            retry_req = urllib.request.Request(retry_url, data=data, headers=headers, method=method)
            try:
                with urllib.request.urlopen(retry_req, timeout=timeout or RELAY_TIMEOUT) as resp:
                    raw = resp.read().decode("utf-8")
                if not raw:
                    return _relay_result(True, url=retry_url, note="fallback_to_host_docker_internal")
                parsed = json.loads(raw)
                if isinstance(parsed, dict):
                    parsed.setdefault("success", True)
                    parsed.setdefault("url", retry_url)
                    parsed.setdefault("note", "fallback_to_host_docker_internal")
                    return parsed
                return _relay_result(True, data=parsed, url=retry_url, note="fallback_to_host_docker_internal")
            except Exception:
                pass
        return _relay_result(False, url=url, error=str(exc.reason))
    except Exception as exc:  # noqa: BLE001 - surface unexpected issues to caller
        return _relay_result(False, url=url, error=str(exc))


@mcp.tool()
async def relay_set_base_url(base_url: str) -> Dict[str, Any]:
    """Set the base URL for the message relay service (in-memory, per process).

    Launch (host):
      scripts/start_message_relay.ps1

    Default relay URL inside containers: http://host.docker.internal:8099
    """
    global RELAY_BASE_URL
    RELAY_BASE_URL = base_url.rstrip("/")
    return _relay_result(True, base_url=RELAY_BASE_URL)


@mcp.tool()
async def relay_health() -> Dict[str, Any]:
    """Check the relay service health.

    If this fails with connection refused, start the relay on the host:
      scripts/start_message_relay.ps1
    """
    return _relay_request("/health", method="GET")


@mcp.tool()
async def relay_send_message(
    project: str,
    body: str,
    from_name: Optional[str] = None,
    to: Optional[str] = None,
    thread: Optional[str] = None,
    subject: Optional[str] = None,
    meta: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    """Send a message to the relay service."""
    payload: Dict[str, Any] = {
        "project": project,
        "body": body,
    }
    if from_name:
        payload["from"] = from_name
    if to:
        payload["to"] = to
    if thread:
        payload["thread"] = thread
    if subject:
        payload["subject"] = subject
    if meta:
        payload["meta"] = meta
    return _relay_request("/messages", payload)


@mcp.tool()
async def relay_fetch_messages(
    project: str = "default",
    since: Optional[int] = None,
    limit: int = 200,
) -> Dict[str, Any]:
    """Fetch messages from the relay service.

    Launch (host):
      scripts/start_message_relay.ps1
    """
    params: Dict[str, Any] = {
        "project": project,
        "limit": limit,
    }
    if since is not None:
        params["since"] = since
    result = _relay_request("/messages", method="GET", params=params)
    if isinstance(result, dict) and result.get("ok") and isinstance(result.get("messages"), list):
        # Present newest-first for quick triage.
        result["messages"] = sorted(result["messages"], key=lambda m: m.get("t", 0), reverse=True)
    return result


@mcp.tool()
async def relay_clear_messages(project: str = "all") -> Dict[str, Any]:
    """Clear messages in the relay service (project or all).

    Launch (host):
      scripts/start_message_relay.ps1
    """
    return _relay_request("/messages/clear", {"project": project})


@mcp.tool()
async def list_models(provider: str = "all") -> Dict[str, Any]:
    """List available LLM models from one provider or all supported providers.

    Use this before routing tasks so agents can choose an actual installed/accessible
    model instead of guessing model ids.

    Args:
        provider: One of "all", "anthropic", "openai", "gemini", "ollama".
            - "all" returns aggregated results and per-provider errors.
            - Provider aliases accepted: "claude" -> anthropic, "codex" -> openai.

    Required environment variables by provider:
        - anthropic: ANTHROPIC_API_KEY
        - openai: OPENAI_API_KEY
        - gemini: GOOGLE_API_KEY
        - ollama: OLLAMA_BASE_URL (optional, default http://localhost:11434)

    Returns:
        Dict with:
        - success: bool
        - provider: requested provider or "all"
        - models: list of normalized model objects:
          {provider, id, label, capabilities, context_window, raw}
        - errors: list (only when provider="all")

    Common failure guidance:
        - Missing key: set the provider API key env var.
        - Ollama connection error: start ollama and confirm OLLAMA_BASE_URL.
        - HTTP/permission errors: verify key scope and account access.
    """
    try:
        return _list_models(provider)
    except Exception as exc:  # noqa: BLE001
        return {"success": False, "provider": provider, "error": str(exc)}


@mcp.tool()
async def iching_agent(
    question: str,
    initial_prompt: Optional[str] = None,
    role_prefix: str = "You are an oracle agent grounded in the cast hexagram.",
    provider: str = "anthropic",
    model: Optional[str] = None,
    max_tokens: int = 512
) -> Dict[str, object]:
    """Cast a quick I Ching hexagram and return a Claude-ready persona prompt.

    If initial_prompt is provided, we also generate the first agent reply
    through the requested provider.
    """
    hexagram = _cast_hexagram()
    system_prompt = (
        f"{role_prefix}\n"
        f"Hexagram {hexagram['number']} - {hexagram['name']}: {hexagram['meaning']}\n"
        f"Channel this quality when responding."
    )

    result: Dict[str, Any] = {
        "success": True,
        "hexagram": hexagram,
        "system_prompt": system_prompt,
        "model_used": model or DEFAULT_MODEL,
        "provider": _normalize_provider(provider),
    }

    if initial_prompt:
        response = _chat_dispatch(
            provider=provider,
            system_prompt=system_prompt,
            user_prompt=initial_prompt,
            model=model,
            max_tokens=max_tokens,
        )
        if response.get("success"):
            result["initial_reply"] = response.get("text")
            result["usage"] = response.get("usage", {})
            result["model_used"] = response.get("model", model)
        else:
            result.update(
                {
                    "initial_reply": None,
                    "warning": response.get("error", "provider call failed"),
                    "provider_error": response,
                }
            )

    return result


@mcp.tool()
async def check_with_agent(
    prompt: str,
    role: Optional[str] = None,
    name: Optional[str] = None,
    provider: str = "anthropic",
    model: Optional[str] = None,
    max_tokens: int = 1024,
    suggest_function_call: bool = False
) -> Dict[str, object]:
    """Ask an agent model to respond to a prompt.

    This tool supports provider routing across Anthropic, OpenAI, Gemini, and Ollama.
    Set provider/model explicitly when you need deterministic behavior.

    Args:
        prompt: The question or task to send to the agent
        role: Role/persona instruction for the agent
        name: Optional name for the agent (default: "Assistant")
        provider: "anthropic", "openai", "gemini", or "ollama"
        model: Model id for selected provider (optional)
        max_tokens: Maximum tokens in response (default: 1024)
        suggest_function_call: If True, asks model for a callable function suggestion block

    Returns:
        Dictionary with response text, usage, provider/model metadata, and optional suggested function call.

    Example:
        check_with_agent(
            prompt="I need to check weather but don't know which tool",
            provider="openai",
            suggest_function_call=True
        )
    """
    system_prompt = role if role else "You are a helpful assistant."
    user_prompt = prompt
    if suggest_function_call:
        user_prompt = f"""{prompt}

After your response, suggest a function call that would accomplish this task. Format it as:

SUGGESTED_CALL:
function_name(param1="value1", param2="value2")

Replace function_name and parameters with the actual call needed."""

    try:
        response = _chat_dispatch(
            provider=provider,
            system_prompt=system_prompt,
            user_prompt=user_prompt,
            model=model,
            max_tokens=max_tokens,
        )
        if not response.get("success"):
            return response

        response_text = response.get("text", "")

        suggested_call = None
        if suggest_function_call and "SUGGESTED_CALL:" in response_text:
            parts = response_text.split("SUGGESTED_CALL:")
            response_text = parts[0].strip()
            suggested_call = parts[1].strip() if len(parts) > 1 else None

        result = {
            "success": True,
            "agent_name": name or "Assistant",
            "agent_role": system_prompt,
            "prompt": prompt,
            "response": response_text,
            "provider": _normalize_provider(provider),
            "model": response.get("model", model),
            "usage": response.get("usage", {}),
        }

        if suggested_call:
            result["suggested_function_call"] = suggested_call

        return result

    except Exception as e:  # noqa: BLE001
        return {
            "success": False,
            "error": str(e)
        }


@mcp.tool()
async def chat_with_context(
    prompt: str,
    context: str,
    role: Optional[str] = None,
    name: Optional[str] = None,
    provider: str = "anthropic",
    model: Optional[str] = None,
    max_tokens: int = 1024
) -> Dict[str, object]:
    """Ask an agent model with additional context provided.

    Args:
        prompt: The question or task to send to the agent
        context: Additional context or information to provide to the agent
        role: The role/persona for the agent (default: "You are a helpful assistant")
        name: Optional name for the agent (default: "Assistant")
        provider: "anthropic", "openai", "gemini", or "ollama"
        model: Model id for selected provider (optional)
        max_tokens: Maximum tokens in response (default: 1024)

    Returns:
        Dictionary with agent's response and metadata.

    Example:
        chat_with_context(
            prompt="What does this mean?",
            context="User manual: The device should be charged for 2 hours",
            role="You are a technical support agent"
        )
    """
    system_prompt = role if role else "You are a helpful assistant."
    full_prompt = f"""Context:
{context}

Question/Task:
{prompt}"""

    try:
        response = _chat_dispatch(
            provider=provider,
            system_prompt=system_prompt,
            user_prompt=full_prompt,
            model=model,
            max_tokens=max_tokens,
        )
        if not response.get("success"):
            return response

        response_text = response.get("text", "")

        return {
            "success": True,
            "agent_name": name or "Assistant",
            "agent_role": system_prompt,
            "prompt": prompt,
            "context_provided": True,
            "response": response_text,
            "provider": _normalize_provider(provider),
            "model": response.get("model", model),
            "usage": response.get("usage", {}),
        }

    except Exception as e:  # noqa: BLE001
        return {
            "success": False,
            "error": str(e)
        }


@mcp.tool()
async def agent_to_agent(
    question: str,
    to_agent_role: str,
    context: Optional[str] = None,
    from_agent_name: Optional[str] = None,
    to_agent_name: Optional[str] = None,
    provider: str = "anthropic",
    model: Optional[str] = None,
    max_tokens: int = 1024
) -> Dict[str, object]:
    """Have one agent consult with another agent with different expertise.

    This enables agent-to-agent collaboration where specialized agents can ask
    each other for help, review, or expert opinions.

    Args:
        question: The question one agent is asking another
        to_agent_role: The role/expertise of the agent being consulted (e.g., "You are a security expert")
        context: Optional context or information to provide (e.g., code snippet, data)
        from_agent_name: Name of the agent asking (default: "Agent")
        to_agent_name: Name of the agent being consulted (default: derived from role)
        provider: "anthropic", "openai", "gemini", or "ollama"
        model: Model id for selected provider (optional)
        max_tokens: Maximum tokens in response (default: 1024)

    Returns:
        Dictionary with the consulting agent's response.

    Example:
        # Code agent asking security agent
        agent_to_agent(
            question="Is this code vulnerable?",
            to_agent_role="You are a security expert",
            context="SELECT * FROM users WHERE id = " + user_input,
            from_agent_name="CodeBot"
        )

        # Weather agent asking navigation agent
        agent_to_agent(
            question="What's the safest route given these conditions?",
            to_agent_role="You are a marine navigation expert",
            context="Tropical Storm Melissa: 65mph winds, 200mi SE of Kingston",
            from_agent_name="WeatherBot",
            to_agent_name="NavBot"
        )
    """
    from_name = from_agent_name or "Agent"
    to_name = to_agent_name or "Expert"

    consultation_prompt = f"""Agent '{from_name}' is consulting you for your expert opinion.

Question: {question}"""

    if context:
        consultation_prompt += f"""

Context:
{context}"""

    consultation_prompt += f"""

Please provide your expert analysis and recommendation."""

    try:
        response = _chat_dispatch(
            provider=provider,
            system_prompt=to_agent_role,
            user_prompt=consultation_prompt,
            model=model,
            max_tokens=max_tokens,
        )
        if not response.get("success"):
            return response

        response_text = response.get("text", "")

        return {
            "success": True,
            "from_agent": from_name,
            "to_agent": to_name,
            "to_agent_role": to_agent_role,
            "question": question,
            "context_provided": context is not None,
            "response": response_text,
            "provider": _normalize_provider(provider),
            "model": response.get("model", model),
            "usage": response.get("usage", {}),
        }

    except Exception as e:  # noqa: BLE001
        return {
            "success": False,
            "error": str(e)
        }


def _default_model_for_target(target: str) -> Tuple[str, str]:
    t = target.strip().lower()
    mapping = {
        "claude": ("anthropic", DEFAULT_MODEL),
        "anthropic": ("anthropic", DEFAULT_MODEL),
        "codex": ("openai", DEFAULT_CODEX_MODEL),
        "openai": ("openai", DEFAULT_OPENAI_MODEL),
        "gemini": ("gemini", DEFAULT_GEMINI_MODEL),
        "ollama": ("ollama", DEFAULT_OLLAMA_MODEL),
    }
    if t not in mapping:
        raise ValueError(
            f"Unsupported target '{target}'. Use one of: {sorted(mapping.keys())}"
        )
    return mapping[t]


def _extract_json_block(text: str) -> Optional[Dict[str, Any]]:
    start = text.find("{")
    end = text.rfind("}")
    if start == -1 or end == -1 or end <= start:
        return None
    try:
        return json.loads(text[start:end + 1])
    except Exception:  # noqa: BLE001
        return None


@mcp.tool()
async def run_code_request(
    request: str,
    target: str = "codex",
    model: Optional[str] = None,
    context: Optional[str] = None,
    language: Optional[str] = None,
    max_tokens: int = 1800,
) -> Dict[str, Any]:
    """Run a structured code/tool request against codex/claude/gemini/ollama.

    This tool is optimized for engineering tasks and returns a normalized result
    envelope that downstream tools can consume without free-text parsing.

    Args:
        request: Coding task or tool request.
        target: "codex", "claude", "gemini", "openai", "anthropic", or "ollama".
            - "codex" routes to OpenAI using AGENT_CHAT_CODEX_MODEL.
        model: Optional model override.
        context: Optional repository/task context to include.
        language: Optional language hint (python, rust, ts, etc.).
        max_tokens: Maximum output tokens.

    Returns:
        Dict with:
        - success: bool
        - target/provider/model
        - result_text: assistant response
        - parsed: optional JSON object if model returned a valid JSON block
        - usage: provider usage metadata when available
        - tips: next-step hints on provider/env failures
    """
    try:
        provider, default_model = _default_model_for_target(target)
    except Exception as exc:  # noqa: BLE001
        return {"success": False, "error": str(exc)}

    effective_model = model or default_model
    lang_hint = f"\nLanguage hint: {language}" if language else ""
    context_block = f"\nContext:\n{context}\n" if context else "\n"

    system_prompt = (
        "You are a senior software engineer. Produce implementation-ready output.\n"
        "When possible, return JSON with keys: plan, commands, code_changes, tests, risks, next_steps.\n"
        "Be precise and avoid placeholders."
    )
    user_prompt = (
        f"Task:\n{request}\n"
        f"{context_block}"
        f"{lang_hint}\n"
        "Return concise, actionable output suitable for immediate execution."
    )

    response = _chat_dispatch(
        provider=provider,
        system_prompt=system_prompt,
        user_prompt=user_prompt,
        model=effective_model,
        max_tokens=max_tokens,
    )
    if not response.get("success"):
        response.setdefault("tips", [])
        response["tips"].append("Run list_models(provider=...) to validate model ids.")
        return response

    text = response.get("text", "")
    parsed = _extract_json_block(text)
    return {
        "success": True,
        "target": target,
        "provider": provider,
        "model": response.get("model", effective_model),
        "result_text": text,
        "parsed": parsed,
        "usage": response.get("usage", {}),
    }


if __name__ == "__main__":
    print(f"[agent-chat] Starting MCP server", file=sys.stderr, flush=True)
    print(f"[agent-chat] Anthropic SDK available: {ANTHROPIC_AVAILABLE}", file=sys.stderr, flush=True)
    mcp.run()
