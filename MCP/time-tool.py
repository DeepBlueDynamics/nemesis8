#!/usr/bin/env python3
"""
Time Tool (MCP)
================

Exposes simple date/time utilities via MCP, following the style used
by other servers in this repo (FastMCP over stdio).

Tools:
- time_now(timezone?, location?, format?)
- time_convert(datetime, from_timezone, to_timezone, format?)
- time_list_timezones(query?, limit?)

Env/config:
- TIME_TOOL_ENABLE_NETWORK=1 to allow optional network lookups (not used by default)
- TIME_TOOL_CITY_DB=/path/to/cities.json for extra city→timezone mappings (optional)
"""

from __future__ import annotations

import json
import os
import math
from typing import Any, Dict, List, Optional, Tuple, Union
from datetime import datetime, timezone, timedelta

try:
    # Python 3.9+
    from zoneinfo import ZoneInfo, available_timezones
except Exception:  # pragma: no cover - fallback only
    ZoneInfo = None  # type: ignore
    available_timezones = lambda: set()  # type: ignore

try:
    # Python 3.11+
    from email.utils import format_datetime as rfc2822_format
except Exception:  # pragma: no cover
    rfc2822_format = None

from mcp.server.fastmcp import FastMCP


mcp = FastMCP("time-tool")


# ------------------------------
# Helpers
# ------------------------------
def _load_city_db() -> Dict[str, str]:
    """Load optional JSON mapping of city names → IANA timezones.

    Example schema: { "paris": "Europe/Paris", "nyc": "America/New_York" }
    """
    path = os.environ.get("TIME_TOOL_CITY_DB")
    if not path:
        return {}
    try:
        with open(path, "r", encoding="utf-8") as f:
            data = json.load(f)
        out = {}
        for k, v in data.items():
            if isinstance(k, str) and isinstance(v, str):
                out[k.strip().lower()] = v.strip()
        return out
    except Exception:
        return {}


def _clean(s: str) -> str:
    return " ".join(s.lower().replace("_", " ").replace("/", " ").split())


def _resolve_timezone(
    *, timezone_name: Optional[str] = None, location: Optional[str] = None
) -> Tuple[Optional[str], Optional[str], List[str]]:
    """Resolve an IANA timezone.

    Returns: (timezone, error, suggestions)
    - timezone: IANA name if resolved
    - error: message if not resolved
    - suggestions: close matches
    """
    tzs = list(sorted(available_timezones())) if available_timezones else []

    if timezone_name:
        tz = timezone_name.strip()
        if tz in tzs:
            return tz, None, []
        # Try case-insensitive match
        for cand in tzs:
            if cand.lower() == tz.lower():
                return cand, None, []
        # Suggest close endings (last path segment)
        last = tz.split("/")[-1].lower()
        sugg = [t for t in tzs if t.lower().endswith("/" + last)]
        return None, f"Invalid timezone: {timezone_name}", sugg[:10]

    if location:
        loc = location.strip()
        norm = _clean(loc)

        # 1) Check optional city DB
        city_db = _load_city_db()
        if city_db:
            exact = city_db.get(norm) or city_db.get(loc.lower())
            if exact:
                return exact, None, []

        # 2) Fuzzy match against IANA city names
        candidates = []
        for t in tzs:
            area_city = t.split("/")
            tail = area_city[-1]
            whole = _clean(t)
            tail_norm = _clean(tail)
            if norm == tail_norm or norm == whole:
                return t, None, []
            if norm in tail_norm or norm in whole:
                candidates.append(t)

        if candidates:
            # Prefer those where tail startswith norm
            starts = [c for c in candidates if _clean(c.split("/")[-1]).startswith(norm)]
            ordered = starts + [c for c in candidates if c not in starts]
            return ordered[0], None, ordered[1:11]

        return None, f"Ambiguous or unknown location: {location}", []

    return None, "Provide either 'timezone' or 'location'", []


def _format_dt(dt: datetime, fmt: str) -> str:
    fmt = (fmt or "iso").lower()
    if fmt == "iso":
        return dt.isoformat()
    if fmt == "unix":
        return str(int(dt.timestamp()))
    if fmt == "rfc2822":
        if rfc2822_format:
            return rfc2822_format(dt)
        # Best-effort fallback
        return dt.strftime("%a, %d %b %Y %H:%M:%S %z")
    if fmt == "human":
        # Example: Friday, 12:34 PM JST (UTC+09:00)
        abbr = dt.tzname() or ""
        offset = dt.utcoffset() or timedelta(0)
        sign = "+" if offset >= timedelta(0) else "-"
        total = abs(int(offset.total_seconds()))
        hh, mm = divmod(total // 60, 60)
        return dt.strftime(f"%A, %I:%M %p {abbr} (UTC{sign}{hh:02d}:{mm:02d})").lstrip("0")
    # Default to ISO
    return dt.isoformat()


def _tz_fields(dt: datetime) -> Dict[str, Any]:
    offset = dt.utcoffset() or timedelta(0)
    sign = "+" if offset >= timedelta(0) else "-"
    total = abs(int(offset.total_seconds()))
    hh, mm = divmod(total // 60, 60)
    return {
        "datetime_iso": dt.isoformat(),
        "unix": int(dt.timestamp()),
        "timezone": str(dt.tzinfo) if dt.tzinfo else "UTC",
        "utc_offset": f"UTC{sign}{hh:02d}:{mm:02d}",
        "abbr": dt.tzname() or "",
        "day_of_week": dt.strftime("%A"),
        "day_of_year": int(dt.strftime("%j")),
        "is_dst": bool((dt.dst() or timedelta(0)).total_seconds() != 0),
    }


def _parse_input_datetime(value: Union[str, int, float]) -> Tuple[Optional[datetime], Optional[str]]:
    """Parse ISO 8601 string (supports trailing 'Z') or unix seconds."""
    if isinstance(value, (int, float)):
        try:
            return datetime.fromtimestamp(float(value), tz=timezone.utc), None
        except Exception as e:  # pragma: no cover
            return None, f"Invalid unix timestamp: {e}"
    if isinstance(value, str):
        s = value.strip()
        # Try unix-like number string
        if s.isdigit() or (s.startswith("-") and s[1:].isdigit()):
            try:
                return datetime.fromtimestamp(float(s), tz=timezone.utc), None
            except Exception as e:
                return None, f"Invalid unix timestamp: {e}"
        # Normalize Z → +00:00 for fromisoformat
        if s.endswith("Z"):
            s = s[:-1] + "+00:00"
        try:
            dt = datetime.fromisoformat(s)
            # If naive, assume UTC
            if dt.tzinfo is None:
                dt = dt.replace(tzinfo=timezone.utc)
            return dt, None
        except Exception as e:
            return None, f"Invalid datetime string: {e}"
    return None, "Unsupported datetime type"


def _get_zone(tzname: str) -> ZoneInfo:
    if ZoneInfo is None:  # pragma: no cover
        raise RuntimeError("zoneinfo not available")
    return ZoneInfo(tzname)


# ------------------------------
# Tools
# ------------------------------
@mcp.tool()
async def time_now(
    timezone: Optional[str] = None,
    location: Optional[str] = None,
    format: str = "iso",
) -> Dict[str, Any]:
    """Current date/time for a location or IANA timezone.

    Returns fields plus a 'formatted' string shaped per 'format'.
    """
    tz, err, sugg = _resolve_timezone(timezone_name=timezone, location=location)
    if not tz:
        out: Dict[str, Any] = {"success": False, "error": err or "Failed to resolve timezone"}
        if sugg:
            out["suggestions"] = sugg
        return out

    now = datetime.now(tz=_get_zone(tz))
    fields = _tz_fields(now)
    fields["formatted"] = _format_dt(now, format)
    fields["success"] = True
    return fields


@mcp.tool()
async def time_convert(
    datetime_value: Union[str, int, float],
    from_timezone: str,
    to_timezone: str,
    format: str = "iso",
) -> Dict[str, Any]:
    """Convert an input timestamp between timezones.

    'datetime_value' may be ISO 8601 string or unix seconds.
    """
    src_tz, err1, _ = _resolve_timezone(timezone_name=from_timezone)
    if not src_tz:
        return {"success": False, "error": err1 or "Invalid source timezone"}
    dst_tz, err2, _ = _resolve_timezone(timezone_name=to_timezone)
    if not dst_tz:
        return {"success": False, "error": err2 or "Invalid target timezone"}

    dt, perr = _parse_input_datetime(datetime_value)
    if not dt:
        return {"success": False, "error": perr or "Invalid datetime"}

    # Normalize to source tz
    dt_src = dt.astimezone(_get_zone(src_tz))
    # Convert to target
    dt_dst = dt_src.astimezone(_get_zone(dst_tz))

    out_in = _tz_fields(dt_src)
    out_out = _tz_fields(dt_dst)
    out_out["formatted"] = _format_dt(dt_dst, format)

    return {
        "success": True,
        "input": out_in,
        "output": out_out,
    }


@mcp.tool()
async def time_list_timezones(query: Optional[str] = None, limit: int = 50) -> Dict[str, Any]:
    """Discover IANA timezone names with optional substring filter."""
    tzs = list(sorted(available_timezones())) if available_timezones else []
    if query:
        q = query.strip().lower()
        tzs = [t for t in tzs if q in t.lower()]
    try:
        lim = max(1, min(int(limit), 500))
    except Exception:
        lim = 50
    return {"success": True, "timezones": tzs[:lim], "count": len(tzs[:lim])}


if __name__ == "__main__":
    mcp.run(transport="stdio")

