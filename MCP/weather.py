#!/usr/bin/env python3
"""
Weather MCP server for concierge and general use.

Provides current conditions, multi-day forecasts, and NWS weather alerts.
Default location is New Braunfels, TX. Uses Open-Meteo (no API key) for
forecasts and NWS weather.gov for US alerts.
"""

from __future__ import annotations

import json
import logging
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional
from urllib import parse as _urlparse
from urllib import request as _urlrequest
from urllib.error import HTTPError, URLError

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("weather")

# ---------------------------------------------------------------------------
# Logging
# ---------------------------------------------------------------------------
_log_dir = Path(__file__).parent / ".mcp-logs"
_log_dir.mkdir(parents=True, exist_ok=True)
logging.basicConfig(
    level=logging.INFO,
    handlers=[
        logging.FileHandler(_log_dir / "weather.log"),
        logging.StreamHandler(sys.stderr),
    ],
    format="%(asctime)s %(levelname)s %(message)s",
)
log = logging.getLogger("weather")

# ---------------------------------------------------------------------------
# Defaults — New Braunfels, TX
# ---------------------------------------------------------------------------
NB_LAT = 29.703
NB_LNG = -98.124
NB_TZ = "America/Chicago"

# WMO weather codes -> human descriptions
_WMO_CODES = {
    0: "Clear sky",
    1: "Mainly clear",
    2: "Partly cloudy",
    3: "Overcast",
    45: "Foggy",
    48: "Depositing rime fog",
    51: "Light drizzle",
    53: "Moderate drizzle",
    55: "Dense drizzle",
    56: "Light freezing drizzle",
    57: "Dense freezing drizzle",
    61: "Slight rain",
    63: "Moderate rain",
    65: "Heavy rain",
    66: "Light freezing rain",
    67: "Heavy freezing rain",
    71: "Slight snowfall",
    73: "Moderate snowfall",
    75: "Heavy snowfall",
    77: "Snow grains",
    80: "Slight rain showers",
    81: "Moderate rain showers",
    82: "Violent rain showers",
    85: "Slight snow showers",
    86: "Heavy snow showers",
    95: "Thunderstorm",
    96: "Thunderstorm with slight hail",
    99: "Thunderstorm with heavy hail",
}


# ---------------------------------------------------------------------------
# HTTP helpers
# ---------------------------------------------------------------------------
def _ok(**kwargs: Any) -> Dict[str, Any]:
    return {"success": True, **kwargs}


def _fail(error: str, **kwargs: Any) -> Dict[str, Any]:
    return {"success": False, "error": error, **kwargs}


def _get(url: str, params: Dict[str, Any], ua: str = "weather-mcp/1.0") -> Dict[str, Any]:
    """GET JSON from a URL with query params."""
    query = _urlparse.urlencode(params, doseq=True)
    full = f"{url}?{query}" if query else url
    req = _urlrequest.Request(full, headers={"User-Agent": ua, "Accept": "application/json"})
    with _urlrequest.urlopen(req, timeout=15) as resp:
        return json.loads(resp.read().decode("utf-8"))


def _get_raw(url: str, ua: str = "weather-mcp/1.0") -> Dict[str, Any]:
    """GET JSON from a bare URL (no params)."""
    req = _urlrequest.Request(url, headers={"User-Agent": ua, "Accept": "application/geo+json"})
    with _urlrequest.urlopen(req, timeout=15) as resp:
        return json.loads(resp.read().decode("utf-8"))


def _f_to_feel(temp_f: float) -> str:
    """Human-readable temperature feel."""
    if temp_f >= 100:
        return "scorching"
    if temp_f >= 90:
        return "hot"
    if temp_f >= 80:
        return "warm"
    if temp_f >= 70:
        return "pleasant"
    if temp_f >= 60:
        return "mild"
    if temp_f >= 50:
        return "cool"
    if temp_f >= 40:
        return "chilly"
    if temp_f >= 32:
        return "cold"
    return "freezing"


def _c_to_f(c: float) -> float:
    return round(c * 9 / 5 + 32, 1)


def _describe_weather(code: int) -> str:
    return _WMO_CODES.get(code, f"Unknown ({code})")


# ---------------------------------------------------------------------------
# Tools
# ---------------------------------------------------------------------------
@mcp.tool()
async def weather_now(
    latitude: float = NB_LAT,
    longitude: float = NB_LNG,
    location_name: str = "New Braunfels, TX",
) -> Dict[str, Any]:
    """Get current weather conditions for a location.

    Use when:
    - Someone asks "what's the weather like?"
    - Need current temperature, wind, conditions
    - Planning outdoor activities today

    Do not use when:
    - Need multi-day forecast (use `weather_forecast` instead)
    - Need weather alerts/warnings (use `weather_alerts` instead)

    Args:
        latitude: Latitude in decimal degrees (default: New Braunfels)
        longitude: Longitude in decimal degrees (default: New Braunfels)
        location_name: Human-readable name for the location

    Returns:
        Current conditions: temperature (F), wind, humidity, conditions description.

    Example:
        weather_now()  # New Braunfels
        weather_now(latitude=30.267, longitude=-97.743, location_name="Austin, TX")
    """
    try:
        data = _get("https://api.open-meteo.com/v1/forecast", {
            "latitude": latitude,
            "longitude": longitude,
            "current": "temperature_2m,relative_humidity_2m,apparent_temperature,precipitation,weather_code,wind_speed_10m,wind_direction_10m,wind_gusts_10m",
            "temperature_unit": "fahrenheit",
            "wind_speed_unit": "mph",
            "timezone": NB_TZ,
        })
        cur = data.get("current", {})
        temp = cur.get("temperature_2m", 0)
        feels = cur.get("apparent_temperature", temp)
        humidity = cur.get("relative_humidity_2m", 0)
        wind = cur.get("wind_speed_10m", 0)
        gusts = cur.get("wind_gusts_10m", 0)
        precip = cur.get("precipitation", 0)
        code = cur.get("weather_code", 0)

        summary = f"{_describe_weather(code)}, {round(temp)}°F"
        if abs(feels - temp) > 3:
            summary += f" (feels like {round(feels)}°F)"

        log.info("weather_now %s -> %s", location_name, summary)
        return _ok(
            location=location_name,
            summary=summary,
            temperature_f=round(temp),
            feels_like_f=round(feels),
            feel=_f_to_feel(temp),
            humidity_pct=humidity,
            conditions=_describe_weather(code),
            wind_mph=round(wind),
            wind_gusts_mph=round(gusts),
            precipitation_in=round(precip, 2),
            time=cur.get("time", ""),
        )
    except (HTTPError, URLError, OSError) as e:
        log.error("weather_now failed: %s", e)
        return _fail(
            f"Could not fetch weather: {e}",
            try_instead=["weather_forecast()"],
            next_steps=["Check internet connectivity", "Try again in a moment"],
        )


@mcp.tool()
async def weather_forecast(
    latitude: float = NB_LAT,
    longitude: float = NB_LNG,
    location_name: str = "New Braunfels, TX",
    days: int = 5,
) -> Dict[str, Any]:
    """Get multi-day weather forecast with daily highs, lows, and conditions.

    Use when:
    - Someone asks about weather this week/weekend
    - Planning a trip or outdoor event
    - Need to know if rain is coming

    Do not use when:
    - Only need current conditions (use `weather_now` instead)
    - Need hourly detail (use `weather_hourly` instead)
    - Need weather alerts (use `weather_alerts` instead)

    Args:
        latitude: Latitude (default: New Braunfels)
        longitude: Longitude (default: New Braunfels)
        location_name: Human-readable location name
        days: Number of forecast days, 1-14 (default: 5)

    Returns:
        Daily forecast array with date, high/low temps, conditions, precipitation chance.

    Example:
        weather_forecast()  # 5-day NB forecast
        weather_forecast(days=7)  # full week
    """
    days = max(1, min(14, days))
    try:
        data = _get("https://api.open-meteo.com/v1/forecast", {
            "latitude": latitude,
            "longitude": longitude,
            "daily": "weather_code,temperature_2m_max,temperature_2m_min,apparent_temperature_max,apparent_temperature_min,precipitation_sum,precipitation_probability_max,wind_speed_10m_max,wind_gusts_10m_max,sunrise,sunset,uv_index_max",
            "temperature_unit": "fahrenheit",
            "wind_speed_unit": "mph",
            "precipitation_unit": "inch",
            "timezone": NB_TZ,
            "forecast_days": days,
        })
        daily = data.get("daily", {})
        dates = daily.get("time", [])
        forecast = []
        for i, date in enumerate(dates):
            hi = daily["temperature_2m_max"][i]
            lo = daily["temperature_2m_min"][i]
            code = daily["weather_code"][i]
            precip_chance = daily.get("precipitation_probability_max", [0] * len(dates))[i]
            precip_sum = daily.get("precipitation_sum", [0] * len(dates))[i]
            wind = daily.get("wind_speed_10m_max", [0] * len(dates))[i]
            uv = daily.get("uv_index_max", [0] * len(dates))[i]

            day_summary = f"{_describe_weather(code)}, {round(hi)}°/{round(lo)}°F"
            if precip_chance and precip_chance > 20:
                day_summary += f", {precip_chance}% rain"

            forecast.append({
                "date": date,
                "summary": day_summary,
                "high_f": round(hi),
                "low_f": round(lo),
                "conditions": _describe_weather(code),
                "precip_chance_pct": precip_chance or 0,
                "precip_inches": round(precip_sum, 2) if precip_sum else 0,
                "wind_max_mph": round(wind) if wind else 0,
                "uv_index": round(uv, 1) if uv else 0,
                "sunrise": daily.get("sunrise", [""])[i] if daily.get("sunrise") else "",
                "sunset": daily.get("sunset", [""])[i] if daily.get("sunset") else "",
            })

        log.info("weather_forecast %s %d days", location_name, days)
        return _ok(
            location=location_name,
            days=len(forecast),
            forecast=forecast,
        )
    except (HTTPError, URLError, OSError) as e:
        log.error("weather_forecast failed: %s", e)
        return _fail(
            f"Could not fetch forecast: {e}",
            try_instead=["weather_now()"],
            next_steps=["Try fewer days", "Check connectivity"],
        )


@mcp.tool()
async def weather_hourly(
    latitude: float = NB_LAT,
    longitude: float = NB_LNG,
    location_name: str = "New Braunfels, TX",
    hours: int = 24,
) -> Dict[str, Any]:
    """Get hourly weather forecast with temperature, rain chance, and wind.

    Use when:
    - Someone needs hour-by-hour detail for today or tomorrow
    - Planning around specific times (morning vs afternoon)
    - Need to know when rain starts/stops

    Do not use when:
    - Only need a general daily overview (use `weather_forecast` instead)
    - Only need current conditions (use `weather_now` instead)

    Args:
        latitude: Latitude (default: New Braunfels)
        longitude: Longitude (default: New Braunfels)
        location_name: Human-readable location name
        hours: Number of hours to forecast, 1-48 (default: 24)

    Returns:
        Hourly array with time, temperature, conditions, precipitation probability.

    Example:
        weather_hourly()  # next 24h in NB
        weather_hourly(hours=12)  # next 12h
    """
    hours = max(1, min(48, hours))
    try:
        data = _get("https://api.open-meteo.com/v1/forecast", {
            "latitude": latitude,
            "longitude": longitude,
            "hourly": "temperature_2m,apparent_temperature,precipitation_probability,precipitation,weather_code,wind_speed_10m,wind_gusts_10m,relative_humidity_2m",
            "temperature_unit": "fahrenheit",
            "wind_speed_unit": "mph",
            "precipitation_unit": "inch",
            "timezone": NB_TZ,
            "forecast_hours": hours,
        })
        hourly = data.get("hourly", {})
        times = hourly.get("time", [])[:hours]
        result = []
        for i, t in enumerate(times):
            temp = hourly["temperature_2m"][i]
            code = hourly["weather_code"][i]
            precip_prob = hourly.get("precipitation_probability", [0] * len(times))[i]
            result.append({
                "time": t,
                "temp_f": round(temp),
                "conditions": _describe_weather(code),
                "precip_chance_pct": precip_prob or 0,
                "wind_mph": round(hourly.get("wind_speed_10m", [0] * len(times))[i]),
                "humidity_pct": hourly.get("relative_humidity_2m", [0] * len(times))[i],
            })

        log.info("weather_hourly %s %dh", location_name, hours)
        return _ok(location=location_name, hours=len(result), hourly=result)
    except (HTTPError, URLError, OSError) as e:
        log.error("weather_hourly failed: %s", e)
        return _fail(f"Could not fetch hourly: {e}")


@mcp.tool()
async def weather_alerts(
    latitude: float = NB_LAT,
    longitude: float = NB_LNG,
    location_name: str = "New Braunfels, TX",
) -> Dict[str, Any]:
    """Get active NWS weather alerts and warnings for a US location.

    Use when:
    - Someone asks about severe weather, storms, or warnings
    - Need to check if any alerts are active
    - Flash flood, tornado, or heat advisory checks

    Do not use when:
    - Location is outside the United States (NWS only covers US)
    - Need forecast data (use `weather_forecast` instead)

    Args:
        latitude: Latitude (default: New Braunfels)
        longitude: Longitude (default: New Braunfels)
        location_name: Human-readable location name

    Returns:
        Active alerts array with event type, severity, headline, and description.
        Empty array if no active alerts.

    Example:
        weather_alerts()  # NB alerts
        weather_alerts(latitude=29.42, longitude=-98.49, location_name="San Antonio, TX")
    """
    try:
        # NWS alerts API — point-based
        url = f"https://api.weather.gov/alerts/active?point={latitude},{longitude}"
        data = _get_raw(url, ua="nbtx.ai weather-mcp/1.0 (concierge)")

        features = data.get("features", [])
        alerts = []
        for f in features:
            props = f.get("properties", {})
            alerts.append({
                "event": props.get("event", "Unknown"),
                "severity": props.get("severity", "Unknown"),
                "urgency": props.get("urgency", "Unknown"),
                "headline": props.get("headline", ""),
                "description": (props.get("description", "") or "")[:500],
                "instruction": (props.get("instruction", "") or "")[:300],
                "expires": props.get("expires", ""),
            })

        log.info("weather_alerts %s -> %d active", location_name, len(alerts))
        if not alerts:
            return _ok(
                location=location_name,
                alert_count=0,
                alerts=[],
                summary="No active weather alerts.",
            )
        return _ok(
            location=location_name,
            alert_count=len(alerts),
            alerts=alerts,
            summary=f"{len(alerts)} active alert(s): {', '.join(a['event'] for a in alerts)}",
        )
    except (HTTPError, URLError, OSError) as e:
        log.error("weather_alerts failed: %s", e)
        return _fail(
            f"Could not fetch alerts: {e}",
            detail="NWS API may be slow or location outside US coverage",
            try_instead=["weather_now() for general conditions"],
            next_steps=["Check if location is within US", "Retry in a moment"],
        )


@mcp.tool()
async def weather_geocode(
    name: str,
    count: int = 3,
) -> Dict[str, Any]:
    """Look up coordinates for a location name.

    Use when:
    - Need lat/lon for a city or place name before calling other weather tools
    - User mentions a location by name that isn't New Braunfels

    Do not use when:
    - Already have coordinates
    - Location is New Braunfels (default coords already set)

    Args:
        name: Location name (city, address, landmark)
        count: Max results to return, 1-5 (default: 3)

    Returns:
        Array of matching locations with name, lat, lon, country, state.

    Example:
        weather_geocode(name="Gruene, TX")
        weather_geocode(name="San Antonio")
    """
    count = max(1, min(5, count))
    try:
        data = _get("https://geocoding-api.open-meteo.com/v1/search", {
            "name": name,
            "count": count,
            "language": "en",
            "format": "json",
        })
        results = data.get("results", []) or []
        locations = []
        for r in results:
            locations.append({
                "name": r.get("name", ""),
                "latitude": r.get("latitude"),
                "longitude": r.get("longitude"),
                "country": r.get("country", ""),
                "state": r.get("admin1", ""),
                "timezone": r.get("timezone", ""),
            })

        log.info("weather_geocode '%s' -> %d results", name, len(locations))
        if not locations:
            return _fail(
                f"No locations found for '{name}'",
                next_steps=["Try a more specific name", "Include state/country"],
            )
        return _ok(query=name, count=len(locations), locations=locations)
    except (HTTPError, URLError, OSError) as e:
        log.error("weather_geocode failed: %s", e)
        return _fail(f"Geocoding failed: {e}")


if __name__ == "__main__":
    mcp.run(transport="stdio")
