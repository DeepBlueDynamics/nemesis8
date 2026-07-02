#!/usr/bin/env python3
"""
Meridian MCP shim — DYNAMIC, AUTO-UPDATING proxy.

A stdio MCP server for providers/agents whose MCP client only speaks stdio
(not HTTP). It does NOT re-declare Meridian's tools — it connects to the
meridian-sidecar's own MCP server over streamable-HTTP and forwards
tools/list and tools/call straight through. Add/rename/remove a sidecar tool
and this shim reflects it on the next list, with no edits here ever.

Same pattern as nemesis8's hyperia-mcp.py shim (that one is the reference).

Clients that speak streamable HTTP natively don't need this file at all:

    { "mcpServers": { "meridian": {
        "type": "http", "url": "http://127.0.0.1:9124/mcp" } } }

Env:
  MERIDIAN_URL          sidecar base URL
                        (default http://host.docker.internal:9124; use
                        http://127.0.0.1:9124 outside a container)
  MERIDIAN_AGENT_TOKEN  optional Bearer token. v1 of the sidecar is
                        loopback-only with no auth — leave unset. The hook
                        exists because identity/consent is the planned
                        follow-up; setting it early costs nothing.
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

MERIDIAN_URL = os.environ.get("MERIDIAN_URL", "http://host.docker.internal:9124").rstrip("/")
MCP_URL = MERIDIAN_URL + "/mcp"

MERIDIAN_AGENT_TOKEN = os.environ.get("MERIDIAN_AGENT_TOKEN", "").strip()
AUTH_HEADERS = (
    {"Authorization": f"Bearer {MERIDIAN_AGENT_TOKEN}"} if MERIDIAN_AGENT_TOKEN else None
)

# Global session to be initialized at startup in the main task context
upstream_session: ClientSession | None = None

server = Server("meridian")


@server.list_tools()
async def list_tools() -> list[types.Tool]:
    # Upstream unavailable (sidecar down): advertise no tools rather than
    # raising — the agent's session stays healthy, just without Meridian tools.
    if upstream_session is None:
        return []
    result = await upstream_session.list_tools()
    return list(result.tools)


@server.call_tool()
async def call_tool(name: str, arguments: dict) -> list[types.ContentBlock]:
    if upstream_session is None:
        raise RuntimeError(f"Upstream session to Meridian at {MCP_URL} is not initialized.")
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
    # success; on ANY failure, fall through and serve with no Meridian tools
    # (list_tools() returns []). Common cause: meridian-sidecar not running —
    # start it with scripts/run.ps1 in the meridian repo, or
    # sidecar/target/release/meridian-sidecar.exe directly.
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
        print(f"[meridian-mcp] upstream connect failed at {MCP_URL}: {e}; "
              f"serving with no Meridian tools", file=sys.stderr)

    upstream_session = None
    await _serve_stdio()


if __name__ == "__main__":
    asyncio.run(main())
