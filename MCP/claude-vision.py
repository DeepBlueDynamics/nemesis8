#!/usr/bin/env python3
"""
MCP: claude-vision

Send image(s) + prompt to Anthropic Claude via the Messages API.
"""

from __future__ import annotations

import base64
import mimetypes
import os
from pathlib import Path
from typing import Any, Dict, List, Optional

try:
    import anthropic
    ANTHROPIC_AVAILABLE = True
except ImportError:
    anthropic = None
    ANTHROPIC_AVAILABLE = False

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("claude-vision")

DEFAULT_MODEL = os.getenv("CLAUDE_VISION_MODEL", "claude-sonnet-4-5-20250929")


def _encode_image(path: Path) -> Dict[str, str]:
    data = path.read_bytes()
    mime, _ = mimetypes.guess_type(path.name)
    mime = mime or "application/octet-stream"
    return {
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": mime,
            "data": base64.b64encode(data).decode("utf-8"),
        },
    }


def _normalize_system(system: Optional[Any]) -> Optional[List[Dict[str, str]]]:
    if system is None:
        return None
    if isinstance(system, str):
        if not system.strip():
            return None
        return [{"type": "text", "text": system}]
    if isinstance(system, list):
        if not system:
            return []
        if all(isinstance(item, str) for item in system):
            return [{"type": "text", "text": item} for item in system]
        if all(isinstance(item, dict) for item in system):
            normalized: List[Dict[str, str]] = []
            for idx, item in enumerate(system):
                if item.get("type") != "text" or "text" not in item:
                    raise ValueError(
                        f"system[{idx}] must be {{'type': 'text', 'text': '...'}}"
                    )
                text = item.get("text")
                if not isinstance(text, str):
                    raise ValueError(f"system[{idx}].text must be a string")
                normalized.append({"type": "text", "text": text})
            return normalized
        raise ValueError("system must be a list of strings or text blocks")
    raise ValueError("system must be a list of text blocks")


@mcp.tool()
async def claude_vision(
    prompt: str,
    image_paths: List[str],
    system: Optional[Any] = None,
    model: Optional[str] = None,
    max_tokens: int = 1024,
) -> Dict[str, object]:
    """
    Send image(s) + prompt to Claude.

    Args:
        prompt: User prompt
        image_paths: List of image file paths (PNG/JPG)
        system: Optional system content blocks (list of {"type": "text", "text": "..."})
        model: Claude model ID
        max_tokens: Output token limit
    """
    if not ANTHROPIC_AVAILABLE:
        return {"success": False, "error": "anthropic_package_missing"}
    api_key = os.environ.get("ANTHROPIC_API_KEY")
    if not api_key:
        return {"success": False, "error": "ANTHROPIC_API_KEY_not_set"}
    if not prompt:
        return {"success": False, "error": "prompt_required"}
    if not image_paths:
        return {"success": False, "error": "image_paths_required"}

    try:
        system_blocks = _normalize_system(system)
    except ValueError as e:
        return {
            "success": False,
            "error": f"invalid_system: {e}",
            "hint": "system must be a list of text blocks; example: [{\"type\": \"text\", \"text\": \"...\"}]",
        }

    parts: List[Dict[str, object]] = [{"type": "text", "text": prompt}]
    for p in image_paths:
        path = Path(p)
        if not path.exists():
            return {"success": False, "error": f"image_not_found: {path}"}
        parts.append(_encode_image(path))

    try:
        client = anthropic.Anthropic(api_key=api_key)
        create_args: Dict[str, object] = {
            "model": model or DEFAULT_MODEL,
            "max_tokens": max_tokens,
            "messages": [{"role": "user", "content": parts}],
        }
        if system_blocks is not None:
            create_args["system"] = system_blocks
        message = client.messages.create(**create_args)
        text = message.content[0].text if message.content else ""
        return {
            "success": True,
            "model": message.model,
            "response": text,
            "usage": {
                "input_tokens": message.usage.input_tokens,
                "output_tokens": message.usage.output_tokens,
            },
        }
    except Exception as e:
        return {"success": False, "error": str(e)}


if __name__ == "__main__":
    mcp.run()
