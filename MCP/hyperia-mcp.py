#!/usr/bin/env python3
"""
Hyperia MCP shim — DYNAMIC, AUTO-UPDATING proxy.

This is a stdio MCP server for providers/agents whose MCP client only speaks
stdio (not HTTP). Instead of hard-coding Hyperia's tools (the old version
re-declared ~60 @mcp.tool() stubs by hand and rotted every time Hyperia
changed), this proxy:

  1. Connects to Hyperia's OWN MCP server over streamable-HTTP at
     $HYPERIA_URL/mcp  (default http://host.docker.internal:9800/mcp).
  2. Forwards tools/list and tools/call straight through.

So the tool set, schemas, descriptions and behaviour are ALWAYS whatever the
running Hyperia sidecar reports — add/rename/remove a Hyperia tool and this
shim reflects it on the next list, with no edits here ever again.

Resilience (2026-09-24):
  * The bearer token is read from n8's token file before every request, so a
    token Hyperia rotated or re-issued on re-attach is picked up without a
    restart (the env var is only the fallback).
  * The upstream ClientSession is rebuilt when the token changes and after any
    failed or timed-out call: a cancelled tools/call leaves the mcp client
    answering every later request with -32602 "Invalid request parameters",
    which used to wedge a pane's Hyperia tools for the rest of its life.

Env:
  HYPERIA_URL                 Hyperia sidecar base URL (default http://host.docker.internal:9800)
  HYPERIA_AGENT_TOKEN         fallback bearer token (captured at launch)
  NEMESIS8_AGENT_ID           names the live token file /opt/nemesis8/.n8/tokens/<id>
  NEMESIS8_TOKEN_FILE         override that path
  HYPERIA_MCP_CALL_TIMEOUT    seconds per upstream call before reconnecting (default 120)
"""

from __future__ import annotations

import asyncio
import contextlib
import os
import sys

import mcp.types as types
from mcp.client.session import ClientSession
from mcp.client.streamable_http import streamablehttp_client
from mcp.server.lowlevel import Server
from mcp.server.stdio import stdio_server

HYPERIA_URL = os.environ.get("HYPERIA_URL", "http://host.docker.internal:9800").rstrip("/")
MCP_URL = HYPERIA_URL + "/mcp"

_agent_id = os.environ.get("NEMESIS8_AGENT_ID", "").strip()
TOKEN_FILE = os.environ.get("NEMESIS8_TOKEN_FILE", "").strip() or (
    f"/opt/nemesis8/.n8/tokens/{_agent_id}" if _agent_id else ""
)
CALL_TIMEOUT_S = float(os.environ.get("HYPERIA_MCP_CALL_TIMEOUT", "120") or "120")


def _log(msg: str) -> None:
    print(f"[hyperia-mcp] {msg}", file=sys.stderr, flush=True)


def current_token() -> str:
    """The live token: n8's per-agent token file first (n8 rewrites it when
    Hyperia re-attaches or rotates tokens), then the env var from launch."""
    if TOKEN_FILE:
        try:
            with open(TOKEN_FILE, encoding="utf-8") as f:
                tok = f.read().strip()
            if tok:
                return tok
        except OSError:
            pass
    return os.environ.get("HYPERIA_AGENT_TOKEN", "").strip()


class Upstream:
    """One upstream ClientSession, rebuilt on demand: when the token changed,
    or after a call failed / timed out."""

    def __init__(self) -> None:
        self.session: ClientSession | None = None
        self.stack: contextlib.AsyncExitStack | None = None
        self.token: str = ""

    async def ensure(self) -> ClientSession:
        tok = current_token()
        if self.session is not None and tok == self.token:
            return self.session
        if self.session is not None:
            _log("token changed - reconnecting to Hyperia with the new one")
        await self.reset()
        stack = contextlib.AsyncExitStack()
        headers = {"Authorization": f"Bearer {tok}"} if tok else None
        try:
            read, write, _ = await stack.enter_async_context(
                streamablehttp_client(MCP_URL, headers=headers)
            )
            session = await stack.enter_async_context(ClientSession(read, write))
            await asyncio.wait_for(session.initialize(), CALL_TIMEOUT_S)
        except BaseException:
            with contextlib.suppress(BaseException):
                await stack.aclose()
            raise
        self.session, self.stack, self.token = session, stack, tok
        return session

    async def reset(self) -> None:
        stack, self.session, self.stack = self.stack, None, None
        if stack is not None:
            # anyio refuses to exit a cancel scope from a task other than the one
            # that entered it, and a half-open transport can raise on close. Never
            # let that take the shim down - the old transport is just abandoned.
            with contextlib.suppress(BaseException):
                await asyncio.wait_for(stack.aclose(), 5)


upstream = Upstream()
server = Server("hyperia")


async def _call_with_reconnect(op, label: str):
    """Run `op(session)`; on any failure or timeout, rebuild the upstream
    session and retry once. CancelledError (our own shutdown) passes through."""
    last: BaseException | None = None
    for attempt in (1, 2):
        try:
            session = await upstream.ensure()
        except asyncio.CancelledError:
            raise
        except BaseException as e:  # noqa: BLE001 - anyio wraps errors in ExceptionGroups
            last = e
            _log(f"{label}: connect to {MCP_URL} failed (attempt {attempt}): {type(e).__name__}: {e}")
            continue
        try:
            return await asyncio.wait_for(op(session), CALL_TIMEOUT_S)
        except asyncio.CancelledError:
            raise
        except BaseException as e:  # noqa: BLE001
            last = e
            _log(f"{label} failed (attempt {attempt}): {type(e).__name__}: {e}; reconnecting")
            await upstream.reset()
    raise RuntimeError(f"{label} failed after reconnecting to Hyperia at {MCP_URL}: {last}")


@server.list_tools()
async def list_tools() -> list[types.Tool]:
    # Upstream unavailable (sidecar down / bad token): advertise no tools rather
    # than raising - the agent's session stays healthy, just without Hyperia
    # tools - and try again on the next list.
    try:
        result = await _call_with_reconnect(lambda s: s.list_tools(), "tools/list")
    except asyncio.CancelledError:
        raise
    except BaseException as e:  # noqa: BLE001
        _log(f"tools/list unavailable: {e}")
        return []
    return list(result.tools)


@server.call_tool()
async def call_tool(name: str, arguments: dict) -> list[types.ContentBlock]:
    result = await _call_with_reconnect(
        lambda s: s.call_tool(name, arguments or {}), f"tools/call {name}"
    )
    return list(result.content)


async def _serve_stdio() -> None:
    async with stdio_server() as (srv_read, srv_write):
        await server.run(srv_read, srv_write, server.create_initialization_options())


async def main() -> None:
    # DEGRADE, don't die: a failed first connect must NOT exit before the MCP
    # handshake (the agent would report "MCP startup failed" and abort its whole
    # MCP setup). Connect eagerly so tools/list is populated from the start; on
    # failure serve anyway and keep retrying lazily on each list/call.
    try:
        await upstream.ensure()
    except asyncio.CancelledError:
        raise
    except BaseException as e:  # noqa: BLE001
        _log(f"upstream connect failed at {MCP_URL}: {e}; serving with no Hyperia tools until it comes back")
    await _serve_stdio()


if __name__ == "__main__":
    asyncio.run(main())
