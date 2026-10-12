#!/usr/bin/env python3
"""
Lume MCP shim — DYNAMIC, AUTO-UPDATING proxy.

A stdio MCP server for providers/agents whose MCP client only speaks stdio
(not HTTP) — antigravity's connector rejects the `httpUrl`/socket form. It does
NOT re-declare Lume's tools; it connects to the in-container `lume serve` MCP
over streamable-HTTP and forwards tools/list and tools/call straight through, so
the tool set (lume_search, lume_sql, ti_query, …) is always whatever the running
server reports.

Same pattern as nemesis8's hyperia-mcp.py / meridian-mcp.py shims (hyperia is
the reference). Clients that speak streamable HTTP natively (codex/claude/
opencode) don't use this file — they get the `lume` registry server directly as
`{ url: "http://127.0.0.1:5863/mcp" }`. `lume serve` runs in THIS container
(supervised by nemesis8-entry), so the endpoint is loopback with no auth.

Env:
  LUME_MCP_URL   full upstream MCP endpoint
                 (default http://127.0.0.1:5863/mcp — the local `lume serve`)
"""

from __future__ import annotations

import asyncio
import os
import sys

import mcp.types as types
from mcp.client.session import ClientSession
from mcp.client.streamable_http import streamablehttp_client
from mcp.server.lowlevel import Server
from mcp.server.stdio import stdio_server

MCP_URL = os.environ.get("LUME_MCP_URL", "http://127.0.0.1:5863/mcp").strip()

# lume serve is loopback-only inside the container; no bearer token.
AUTH_HEADERS = None

# Global session, initialized at startup in the main task context.
upstream_session: ClientSession | None = None

server = Server("lume")


@server.list_tools()
async def list_tools() -> list[types.Tool]:
    # Upstream unavailable (lume serve not up yet / not installed): advertise no
    # tools rather than raising — the agent's session stays healthy, just
    # without Lume tools, and they appear once serve is reachable + re-listed.
    if upstream_session is None:
        return []
    result = await upstream_session.list_tools()
    return list(result.tools)


@server.call_tool()
async def call_tool(name: str, arguments: dict) -> list[types.ContentBlock]:
    if upstream_session is None:
        raise RuntimeError(f"Upstream session to lume at {MCP_URL} is not initialized.")
    result = await upstream_session.call_tool(name, arguments or {})
    return list(result.content)


async def _serve_stdio() -> None:
    async with stdio_server() as (srv_read, srv_write):
        await server.run(srv_read, srv_write, server.create_initialization_options())


async def main() -> None:
    global upstream_session
    # DEGRADE, don't die: if the upstream connect fails we must NOT exit before
    # the MCP handshake (that makes the agent report "MCP startup failed" and
    # aborts its whole MCP setup). Serve stdio INSIDE the connected scope on
    # success; on ANY failure, fall through and serve with no Lume tools.
    try:
        async with streamablehttp_client(MCP_URL, headers=AUTH_HEADERS) as (read, write, _):
            async with ClientSession(read, write) as session:
                await session.initialize()
                upstream_session = session
                await _serve_stdio()
                return
    except asyncio.CancelledError:
        raise  # don't swallow shutdown/cancellation
    except BaseException as e:  # noqa: BLE001 — anyio wraps connect errors in a BaseExceptionGroup
        print(f"[lume-mcp] upstream connect failed at {MCP_URL}: {e}; "
              f"serving with no Lume tools", file=sys.stderr)

    upstream_session = None
    await _serve_stdio()


if __name__ == "__main__":
    asyncio.run(main())
