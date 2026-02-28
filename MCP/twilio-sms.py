#!/usr/bin/env python3
"""Twilio SMS (MCP)
=================

Minimal Twilio SMS client for MCP.

Credentials (env):
- TWILIO_ACCOUNT_SID
- TWILIO_AUTH_TOKEN
- TWILIO_API_KEY_SID (optional)
- TWILIO_API_KEY_SECRET (optional)
- TWILIO_DEFAULT_FROM (optional default sender)

Notes:
- Uses API Key auth if API key vars are set; otherwise uses auth token.
- Returns no secrets, ever.
"""

from __future__ import annotations

import os
from typing import Any, Dict, Optional

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("twilio-sms")


def _result(success: bool, **kwargs: Any) -> Dict[str, Any]:
    data = {"ok": success, "success": success}
    data.update(kwargs)
    return data


def _get_client():
    try:
        from twilio.rest import Client
    except Exception as exc:  # pragma: no cover
        return None, _result(False, error=f"twilio package not installed: {exc}")

    account_sid = os.environ.get("TWILIO_ACCOUNT_SID", "").strip()
    auth_token = os.environ.get("TWILIO_AUTH_TOKEN", "").strip()
    api_key_sid = os.environ.get("TWILIO_API_KEY_SID", "").strip()
    api_key_secret = os.environ.get("TWILIO_API_KEY_SECRET", "").strip()

    if api_key_sid and api_key_secret and account_sid:
        client = Client(api_key_sid, api_key_secret, account_sid)
        return client, None

    if account_sid and auth_token:
        client = Client(account_sid, auth_token)
        return client, None

    return None, _result(
        False,
        error="Missing Twilio credentials",
        detail="Set TWILIO_ACCOUNT_SID + TWILIO_AUTH_TOKEN (or API key + secret).",
    )


@mcp.tool()
def twilio_health() -> Dict[str, Any]:
    """Check whether Twilio credentials are present (no secrets returned)."""
    has_sid = bool(os.environ.get("TWILIO_ACCOUNT_SID"))
    has_token = bool(os.environ.get("TWILIO_AUTH_TOKEN"))
    has_key = bool(os.environ.get("TWILIO_API_KEY_SID"))
    has_secret = bool(os.environ.get("TWILIO_API_KEY_SECRET"))
    has_from = bool(os.environ.get("TWILIO_DEFAULT_FROM"))
    return _result(
        True,
        has_account_sid=has_sid,
        has_auth_token=has_token,
        has_api_key=(has_key and has_secret),
        has_default_from=has_from,
    )


@mcp.tool()
def twilio_send_sms(to: str, body: str, from_: Optional[str] = None) -> Dict[str, Any]:
    """Send an SMS message.

    Args:
      to: E.164 recipient number (e.g., +15551234567)
      body: message text
      from_: optional sender number (E.164). Falls back to TWILIO_DEFAULT_FROM.
    """
    client, err = _get_client()
    if err:
        return err

    sender = (from_ or os.environ.get("TWILIO_DEFAULT_FROM", "")).strip()
    if not sender:
        return _result(False, error="Missing sender", detail="Provide from_ or TWILIO_DEFAULT_FROM.")

    try:
        msg = client.messages.create(to=to, from_=sender, body=body)
    except Exception as exc:
        return _result(False, error=f"Twilio send failed: {exc}")

    return _result(
        True,
        sid=msg.sid,
        status=msg.status,
        to=msg.to,
        from_=msg.from_,
        body=msg.body,
        date_sent=str(msg.date_sent),
        error_code=msg.error_code,
        error_message=msg.error_message,
    )


@mcp.tool()
def twilio_list_messages(limit: int = 20) -> Dict[str, Any]:
    """List recent SMS messages."""
    client, err = _get_client()
    if err:
        return err
    try:
        messages = client.messages.list(limit=limit)
    except Exception as exc:
        return _result(False, error=f"Twilio list failed: {exc}")

    items = []
    for m in messages:
        items.append({
            "sid": m.sid,
            "to": m.to,
            "from": m.from_,
            "body": m.body,
            "status": m.status,
            "date_sent": str(m.date_sent),
            "error_code": m.error_code,
            "error_message": m.error_message,
        })
    return _result(True, messages=items)


@mcp.tool()
def twilio_list_numbers(limit: int = 50) -> Dict[str, Any]:
    """List incoming phone numbers on the Twilio account."""
    client, err = _get_client()
    if err:
        return err
    try:
        numbers = client.incoming_phone_numbers.list(limit=limit)
    except Exception as exc:
        return _result(False, error=f"Twilio list numbers failed: {exc}")

    items = []
    for n in numbers:
        items.append({
            "sid": n.sid,
            "phone_number": n.phone_number,
            "friendly_name": n.friendly_name,
            "sms_enabled": bool(n.capabilities.get("sms")) if n.capabilities else None,
        })
    return _result(True, numbers=items)


if __name__ == "__main__":
    mcp.run()
