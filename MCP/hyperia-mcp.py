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
from contextlib import AsyncExitStack

import mcp.types as types
from mcp.client.session import ClientSession
from mcp.client.streamable_http import streamablehttp_client
from mcp.server.lowlevel import Server
from mcp.server.stdio import stdio_server

HYPERIA_URL = os.environ.get("HYPERIA_URL", "http://host.docker.internal:9800").rstrip("/")
MCP_URL = HYPERIA_URL + "/mcp"


# Global session to be initialized at startup in the main task context
upstream_session: ClientSession | None = None


server = Server("hyperia")


@server.list_tools()
async def list_tools() -> list[types.Tool]:
    if upstream_session is None:
        raise RuntimeError(f"Upstream session to Hyperia at {MCP_URL} is not initialized.")
    result = await upstream_session.list_tools()
    return list(result.tools)


@server.call_tool()
async def call_tool(name: str, arguments: dict) -> list[types.ContentBlock]:
    if upstream_session is None:
        raise RuntimeError(f"Upstream session to Hyperia at {MCP_URL} is not initialized.")
    result = await upstream_session.call_tool(name, arguments or {})
    return list(result.content)


async def main() -> None:
    global upstream_session
    async with AsyncExitStack() as stack:
        try:
            read, write, _ = await stack.enter_async_context(streamablehttp_client(MCP_URL))
            upstream_session = await stack.enter_async_context(ClientSession(read, write))
            await upstream_session.initialize()
        except Exception as e:
            print(f"[hyperia-mcp] Failed to connect to Hyperia upstream at {MCP_URL}: {e}", file=sys.stderr)
            sys.exit(1)

        async with stdio_server() as (srv_read, srv_write):
            await server.run(srv_read, srv_write, server.create_initialization_options())


if __name__ == "__main__":
    asyncio.run(main())
