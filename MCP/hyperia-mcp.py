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

Env:
  HYPERIA_URL   Hyperia sidecar base URL (default http://host.docker.internal:9800)
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

HYPERIA_URL = os.environ.get("HYPERIA_URL", "http://host.docker.internal:9800").rstrip("/")
MCP_URL = HYPERIA_URL + "/mcp"

# Per-pane auth token forwarded from the host (HYPERIA_AGENT_TOKEN, e.g.
# hyp_pane_…). The sidecar gates privileged routes on it — without the Bearer
# header, terminal/pane/web tools return "No identity on this request" (401).
HYPERIA_AGENT_TOKEN = os.environ.get("HYPERIA_AGENT_TOKEN", "").strip()
AUTH_HEADERS = (
    {"Authorization": f"Bearer {HYPERIA_AGENT_TOKEN}"} if HYPERIA_AGENT_TOKEN else None
)


# Global session to be initialized at startup in the main task context
upstream_session: ClientSession | None = None


server = Server("hyperia")


@server.list_tools()
async def list_tools() -> list[types.Tool]:
    # Upstream unavailable (sidecar down / stale token): advertise no tools rather
    # than raising — the agent's session stays healthy, just without Hyperia tools.
    if upstream_session is None:
        return []
    result = await upstream_session.list_tools()
    return list(result.tools)


@server.call_tool()
async def call_tool(name: str, arguments: dict) -> list[types.ContentBlock]:
    if upstream_session is None:
        raise RuntimeError(f"Upstream session to Hyperia at {MCP_URL} is not initialized.")
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
    # success; on ANY failure, fall through and serve with no Hyperia tools
    # (list_tools() returns []). Common causes of failure: the Hyperia sidecar is
    # down/unreachable, or a stale/invalid per-pane HYPERIA_AGENT_TOKEN (relaunch
    # from a live pane to refresh it). The nested `async with` (not an
    # AsyncExitStack) keeps a half-open upstream from raising during cleanup.
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
        print(f"[hyperia-mcp] upstream connect failed at {MCP_URL}: {e}; "
              f"serving with no Hyperia tools", file=sys.stderr)

    upstream_session = None
    await _serve_stdio()


if __name__ == "__main__":
    asyncio.run(main())
