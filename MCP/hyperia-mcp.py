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


class Upstream:
    """A lazily-connected, self-healing client session to Hyperia's MCP server.

    Held open for the life of the proxy; if the connection drops (e.g. the
    sidecar restarts) the next call transparently reconnects.
    """

    def __init__(self) -> None:
        self._session: ClientSession | None = None
        self._stack: AsyncExitStack | None = None
        self._lock = asyncio.Lock()

    async def _connect(self) -> ClientSession:
        stack = AsyncExitStack()
        read, write, _ = await stack.enter_async_context(streamablehttp_client(MCP_URL))
        session = await stack.enter_async_context(ClientSession(read, write))
        await session.initialize()
        self._session, self._stack = session, stack
        return session

    async def session(self) -> ClientSession:
        async with self._lock:
            if self._session is None:
                await self._connect()
            return self._session  # type: ignore[return-value]

    async def reset(self) -> None:
        async with self._lock:
            if self._stack is not None:
                try:
                    await self._stack.aclose()
                except Exception:
                    pass
            self._session = None
            self._stack = None


upstream = Upstream()


async def _call(fn):
    """Run fn(session) against the upstream, reconnecting once on failure."""
    try:
        return await fn(await upstream.session())
    except Exception as first:
        await upstream.reset()
        try:
            return await fn(await upstream.session())
        except Exception as second:
            print(f"[hyperia-mcp] upstream {MCP_URL} failed: {second}", file=sys.stderr)
            raise first


server = Server("hyperia")


@server.list_tools()
async def list_tools() -> list[types.Tool]:
    result = await _call(lambda s: s.list_tools())
    return list(result.tools)


@server.call_tool()
async def call_tool(name: str, arguments: dict) -> list[types.ContentBlock]:
    result = await _call(lambda s: s.call_tool(name, arguments or {}))
    return list(result.content)


async def main() -> None:
    async with stdio_server() as (read, write):
        await server.run(read, write, server.create_initialization_options())


if __name__ == "__main__":
    asyncio.run(main())
