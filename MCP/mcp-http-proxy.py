#!/usr/bin/env python3
"""
Generic stdio→streamable-HTTP MCP proxy.

A parameterized version of hyperia-mcp.py: bridges a stdio-only MCP client
(an agent that can't speak HTTP) to ANY remote streamable-HTTP MCP server,
forwarding tools/list and tools/call straight through. The tool set is always
whatever the upstream reports — nothing is hard-coded here.

Use it as the graceful-degradation fallback for socket MCP servers: codex/gemini
/claude can talk to authenticated HTTP MCP natively (n8's registry emits the
Authorization header), but native HTTP-MCP startup can HARD-FAIL on an
unreachable/401 upstream (cf. openai/codex#20009), killing the whole MCP setup.
This proxy degrades instead — if the upstream connect fails it serves stdio with
NO tools rather than crashing the agent's handshake.

Env:
  MCP_PROXY_URL        Full upstream MCP endpoint, e.g. http://host:9800/mcp  (required)
  MCP_PROXY_TOKEN_ENV  Name of the env var holding a Bearer token (optional).
                       The token VALUE is read from that var and sent as
                       `Authorization: Bearer <value>`.
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

MCP_URL = os.environ.get("MCP_PROXY_URL", "").strip()

# Resolve the bearer token by indirection: MCP_PROXY_TOKEN_ENV names the env var
# that actually holds the token (e.g. HYPERIA_AGENT_TOKEN). Absent/empty => no auth.
_token_env = os.environ.get("MCP_PROXY_TOKEN_ENV", "").strip()
_token = os.environ.get(_token_env, "").strip() if _token_env else ""
AUTH_HEADERS = {"Authorization": f"Bearer {_token}"} if _token else None


# Upstream session, set once connected in the main task context.
upstream_session: ClientSession | None = None

server = Server("mcp-http-proxy")


@server.list_tools()
async def list_tools() -> list[types.Tool]:
    # Upstream unavailable: advertise no tools rather than raising, so the agent
    # session stays healthy (just without this server's tools).
    if upstream_session is None:
        return []
    result = await upstream_session.list_tools()
    return list(result.tools)


@server.call_tool()
async def call_tool(name: str, arguments: dict) -> list[types.ContentBlock]:
    if upstream_session is None:
        raise RuntimeError(f"Upstream MCP session to {MCP_URL} is not initialized.")
    result = await upstream_session.call_tool(name, arguments or {})
    return list(result.content)


async def _serve_stdio() -> None:
    async with stdio_server() as (srv_read, srv_write):
        await server.run(srv_read, srv_write, server.create_initialization_options())


async def main() -> None:
    global upstream_session

    if not MCP_URL:
        print("[mcp-http-proxy] MCP_PROXY_URL is unset; serving with no tools", file=sys.stderr)
        await _serve_stdio()
        return

    # DEGRADE, don't die: serve stdio INSIDE the connected scope on success; on
    # ANY failure (catch BaseException — anyio wraps connect errors in a
    # BaseExceptionGroup; re-raise CancelledError) log and serve with no tools.
    try:
        async with streamablehttp_client(MCP_URL, headers=AUTH_HEADERS) as (read, write, _):
            async with ClientSession(read, write) as session:
                await session.initialize()
                upstream_session = session
                await _serve_stdio()
                return
    except asyncio.CancelledError:
        raise
    except BaseException as e:  # noqa: BLE001
        print(
            f"[mcp-http-proxy] upstream connect failed at {MCP_URL}: {e}; "
            f"serving with no tools",
            file=sys.stderr,
        )

    upstream_session = None
    await _serve_stdio()


if __name__ == "__main__":
    asyncio.run(main())
