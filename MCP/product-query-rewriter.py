#!/usr/bin/env python3
"""
MCP: product-query-rewriter
Rewrite product descriptions into alternative search phrases.
"""

from __future__ import annotations

import re
from typing import Any, Dict, Iterable, List, Sequence, Tuple

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("product-query-rewriter")

_SIZE_RE = re.compile(r'\b(\d+(?:\.\d+)?)\s*(?:inches|inch|in|")', re.I)

_BASE_REPLACEMENTS: List[Tuple[str, Sequence[str]]] = [
    (r"\brecessed lighting\b", ("can lights", "recessed lights")),
    (r"\brecessed\b", ("can", "can light")),
    (r"\bretrofit lighting\b", ("retrofit light", "retrofit kit")),
    (r"\bretrofit\b", ("retrofit kit", "conversion kit")),
    (r"\bbaffle\b", ("baffle trim",)),
    (r"\btrim\b", ("trim kit",)),
    (r"\bdownlight\b", ("down light",)),
    (r"\blighting\b", ("lights", "light")),
]


def _normalize(text: str) -> str:
    s = text.strip()
    s = s.replace("\n", " ").replace("\r", " ").replace("\t", " ")
    s = re.sub(r"[,;/]+", " ", s)
    s = re.sub(r"\s+", " ", s)
    s = s.strip(" .,:;-/")
    s = re.sub(r"^[^a-zA-Z0-9]+", "", s)
    return s.lower()


def _dedupe_tokens(text: str) -> str:
    tokens = text.split()
    out: List[str] = []
    for token in tokens:
        if not out or out[-1] != token:
            out.append(token)
    return " ".join(out)


def _size_variants(text: str) -> List[str]:
    match = _SIZE_RE.search(text)
    if not match:
        return []
    value = match.group(1)
    variants = [f"{value} inch", f"{value} in", f'{value}"']
    out = []
    for variant in variants:
        out.append(_SIZE_RE.sub(variant, text, count=1))
    return out


def _size_first(text: str) -> str | None:
    match = _SIZE_RE.search(text)
    if not match:
        return None
    size = match.group(0)
    rest = (text[: match.start()] + text[match.end() :]).strip()
    rest = re.sub(r"\s+", " ", rest)
    if not rest:
        return None
    return f"{size} {rest}".strip()


def _build_replacements(extra_synonyms: Dict[str, Iterable[str]] | None) -> List[Tuple[str, Sequence[str]]]:
    replacements = list(_BASE_REPLACEMENTS)
    if not extra_synonyms:
        return replacements
    for term, repls in extra_synonyms.items():
        if not term or not repls:
            continue
        cleaned = " ".join(str(term).strip().lower().split())
        if not cleaned:
            continue
        pattern = r"\b" + re.escape(cleaned) + r"\b"
        unique_repls = []
        for repl in repls:
            repl_clean = " ".join(str(repl).strip().lower().split())
            if repl_clean:
                unique_repls.append(repl_clean)
        if unique_repls:
            replacements.append((pattern, tuple(unique_repls)))
    return replacements


def _add_unique(phrases: List[str], candidate: str) -> None:
    cleaned = _dedupe_tokens(_normalize(candidate))
    if cleaned and cleaned not in phrases:
        phrases.append(cleaned)


def _clamp_count(value: int) -> int:
    try:
        count = int(value)
    except Exception:
        return 3
    if count < 2:
        return 2
    if count > 3:
        return 3
    return count


@mcp.tool()
async def rewrite_product_search_phrases(
    description: str,
    max_phrases: int = 3,
    extra_synonyms: Dict[str, Iterable[str]] | None = None,
) -> Dict[str, Any]:
    """Rewrite a product description into alternative search phrases.

    Args:
        description: Product description text to rewrite.
        max_phrases: Number of phrases to return (clamped to 2-3).
        extra_synonyms: Optional map of term -> list of replacements.
    """
    if not description or not description.strip():
        return {"success": False, "error": "Missing description"}

    count = _clamp_count(max_phrases)
    base = _normalize(description)
    phrases: List[str] = []

    size_first = _size_first(base)
    if size_first and size_first != base:
        _add_unique(phrases, size_first)

    replacements = _build_replacements(extra_synonyms)
    for pattern, repls in replacements:
        if len(phrases) >= count:
            break
        if not re.search(pattern, base, flags=re.IGNORECASE):
            continue
        for repl in repls:
            if len(phrases) >= count:
                break
            candidate = re.sub(pattern, repl, base, flags=re.IGNORECASE)
            if candidate != base:
                _add_unique(phrases, candidate)

    for variant in _size_variants(base):
        if len(phrases) >= count:
            break
        if variant != base:
            _add_unique(phrases, variant)

    if not phrases:
        _add_unique(phrases, base)

    return {"success": True, "phrases": phrases[:count], "count": len(phrases[:count])}


if __name__ == "__main__":
    mcp.run()
