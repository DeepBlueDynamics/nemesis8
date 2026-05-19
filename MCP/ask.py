#!/usr/bin/env python3
"""MCP: ask

One-shot "get a second opinion" tool. Single function — ask(provider, prompt)
— dispatches to Claude, Gemini, or OpenAI. Use it when you want a fresh take
from a different model without leaving your current session.

Examples (from an agent's perspective):
    ask("claude", "is this approach right?")
    ask("gemini", "spot any bugs in this function?", model="gemini-2.5-pro")
    ask("gpt", "rewrite this for clarity", system="be terse")

Providers: claude | anthropic, gemini | google, gpt | openai | chatgpt.
API keys read from env: ANTHROPIC_API_KEY, GEMINI_API_KEY (or GOOGLE_API_KEY),
OPENAI_API_KEY.
"""

from __future__ import annotations

import logging
import os
from pathlib import Path
from typing import Any, Dict, Optional

from mcp.server.fastmcp import FastMCP

_default_log_root = Path(os.environ.get("CODEX_WORKSPACE_ROOT", "/workspace"))
log_dir = _default_log_root / ".mcp-logs"
log_dir.mkdir(parents=True, exist_ok=True)

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s - %(name)s - %(levelname)s - %(message)s",
    handlers=[logging.FileHandler(log_dir / "ask.log")],
)
logger = logging.getLogger("ask")

mcp = FastMCP("ask")

# Sensible defaults per provider. Override with `model=...` on the tool call.
DEFAULT_MODELS = {
    "claude": "claude-sonnet-4-5",
    "gemini": "gemini-2.5-pro",
    "gpt": "gpt-4o",
}

# Aliases → canonical provider key
PROVIDER_ALIASES = {
    "claude": "claude",
    "anthropic": "claude",
    "gemini": "gemini",
    "google": "gemini",
    "gpt": "gpt",
    "openai": "gpt",
    "chatgpt": "gpt",
}


def _resolve_provider(provider: str) -> Optional[str]:
    return PROVIDER_ALIASES.get(provider.strip().lower())


# ── Provider implementations ──────────────────────────────────────


def _ask_claude(prompt: str, model: str, system: Optional[str], max_tokens: int, temperature: Optional[float]) -> Dict[str, Any]:
    api_key = os.environ.get("ANTHROPIC_API_KEY")
    if not api_key:
        return {"success": False, "error": "ANTHROPIC_API_KEY not set"}

    try:
        import anthropic
    except ImportError as e:
        return {"success": False, "error": f"anthropic package not installed: {e}"}

    client = anthropic.Anthropic(api_key=api_key)
    kwargs: Dict[str, Any] = {
        "model": model,
        "max_tokens": max_tokens,
        "messages": [{"role": "user", "content": prompt}],
    }
    if system:
        kwargs["system"] = system
    if temperature is not None:
        kwargs["temperature"] = temperature

    try:
        msg = client.messages.create(**kwargs)
    except Exception as e:
        return {"success": False, "error": f"anthropic call failed: {e}"}

    text = "".join(block.text for block in msg.content if getattr(block, "type", None) == "text")
    return {
        "success": True,
        "provider": "claude",
        "model": model,
        "response": text,
        "usage": {
            "input_tokens": msg.usage.input_tokens,
            "output_tokens": msg.usage.output_tokens,
        },
    }


def _ask_gemini(prompt: str, model: str, system: Optional[str], max_tokens: int, temperature: Optional[float]) -> Dict[str, Any]:
    api_key = os.environ.get("GEMINI_API_KEY") or os.environ.get("GOOGLE_API_KEY")
    if not api_key:
        return {"success": False, "error": "GEMINI_API_KEY (or GOOGLE_API_KEY) not set"}

    try:
        import google.generativeai as genai
    except ImportError as e:
        return {"success": False, "error": f"google-generativeai not installed: {e}"}

    genai.configure(api_key=api_key)
    generation_config: Dict[str, Any] = {"max_output_tokens": max_tokens}
    if temperature is not None:
        generation_config["temperature"] = temperature

    try:
        gmodel = genai.GenerativeModel(
            model_name=model,
            system_instruction=system,
            generation_config=generation_config,
        )
        resp = gmodel.generate_content(prompt)
    except Exception as e:
        return {"success": False, "error": f"gemini call failed: {e}"}

    usage = {}
    if hasattr(resp, "usage_metadata") and resp.usage_metadata is not None:
        usage = {
            "input_tokens": getattr(resp.usage_metadata, "prompt_token_count", None),
            "output_tokens": getattr(resp.usage_metadata, "candidates_token_count", None),
        }

    return {
        "success": True,
        "provider": "gemini",
        "model": model,
        "response": resp.text or "",
        "usage": usage,
    }


def _ask_gpt(prompt: str, model: str, system: Optional[str], max_tokens: int, temperature: Optional[float]) -> Dict[str, Any]:
    api_key = os.environ.get("OPENAI_API_KEY")
    if not api_key:
        return {"success": False, "error": "OPENAI_API_KEY not set"}

    try:
        from openai import OpenAI
    except ImportError as e:
        return {"success": False, "error": f"openai package not installed: {e}"}

    client = OpenAI(api_key=api_key)

    messages = []
    if system:
        messages.append({"role": "system", "content": system})
    messages.append({"role": "user", "content": prompt})

    kwargs: Dict[str, Any] = {
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
    }
    if temperature is not None:
        kwargs["temperature"] = temperature

    try:
        resp = client.chat.completions.create(**kwargs)
    except Exception as e:
        return {"success": False, "error": f"openai call failed: {e}"}

    return {
        "success": True,
        "provider": "gpt",
        "model": model,
        "response": resp.choices[0].message.content or "",
        "usage": {
            "input_tokens": resp.usage.prompt_tokens if resp.usage else None,
            "output_tokens": resp.usage.completion_tokens if resp.usage else None,
        },
    }


# ── MCP tool ──────────────────────────────────────────────────────


@mcp.tool()
async def ask(
    provider: str,
    prompt: str,
    model: Optional[str] = None,
    system: Optional[str] = None,
    max_tokens: int = 2048,
    temperature: Optional[float] = None,
) -> Dict[str, Any]:
    """Ask Claude, Gemini, or OpenAI for a one-shot response. No history is
    carried between calls — each invocation is independent.

    Args:
        provider: Which LLM to query. One of: "claude" / "anthropic",
            "gemini" / "google", or "gpt" / "openai" / "chatgpt".
        prompt: The user message to send.
        model: Optional explicit model name. Defaults to a current model per
            provider (claude-sonnet-4-5, gemini-2.5-pro, gpt-4o).
        system: Optional system prompt / instruction.
        max_tokens: Cap on the response length. Default 2048.
        temperature: Optional sampling temperature.

    Returns:
        Dictionary with {success, provider, model, response, usage} on
        success, or {success: false, error: ...} on failure (missing API key,
        missing SDK, network/auth error). The response field is plain text.

    Example:
        ask("claude", "Is this approach right?", system="Be brief and critical")
        ask("gemini", "Spot any bugs", model="gemini-2.5-flash")
    """
    canonical = _resolve_provider(provider)
    if canonical is None:
        return {
            "success": False,
            "error": f"unknown provider '{provider}'. Use: claude, gemini, or gpt.",
        }

    chosen_model = model or DEFAULT_MODELS[canonical]
    logger.info("ask provider=%s model=%s len=%d", canonical, chosen_model, len(prompt))

    if canonical == "claude":
        return _ask_claude(prompt, chosen_model, system, max_tokens, temperature)
    if canonical == "gemini":
        return _ask_gemini(prompt, chosen_model, system, max_tokens, temperature)
    if canonical == "gpt":
        return _ask_gpt(prompt, chosen_model, system, max_tokens, temperature)

    return {"success": False, "error": f"unhandled provider: {canonical}"}


if __name__ == "__main__":
    mcp.run()
