#!/usr/bin/env python3
"""Reference MCP Tool for Nemesis8 agents to interact with Obol.

Exposes identity registration, tool publishing, tool discovery,
inter-agent messaging, multimodal attachment transfer (PDF, images, audio),
and fiscal ledger inspection for AHP.

Config — the Obol gateway URL and an optional AHP bearer, declared on the
`# n8:secrets` line below so they forward into the per-tool env automatically
(no `env_imports` needed). Discovery works unauthenticated; register/message/
ledger need the token.
  OBOL_GATEWAY_URL   Gateway base URL (default: https://obol.nuts.services)
  NUTS_AHP_TOKEN     AHP bearer (falls back to OBOL_AUTH_TOKEN)
  OBOL_AUTH_TOKEN    Alternate name for the AHP bearer
"""
# n8:secrets optional=NUTS_AHP_TOKEN,OBOL_AUTH_TOKEN,OBOL_GATEWAY_URL

from __future__ import annotations
import os
import sys
import json
import base64
import mimetypes
from pathlib import Path
from typing import Optional, Dict, Any, List
import httpx
from mcp.server.fastmcp import FastMCP

# Initialize FastMCP server
mcp = FastMCP("obol")

# Base URL for the Obol Gateway HTTP Service
GATEWAY_URL = os.getenv("OBOL_GATEWAY_URL", "https://obol.nuts.services").rstrip("/")
AUTH_TOKEN = os.getenv("NUTS_AHP_TOKEN", os.getenv("OBOL_AUTH_TOKEN", ""))
AGENT_PRINCIPAL = os.getenv("NEMESIS8_AGENT_PRINCIPAL", "ed25519:nemesis8_agent_default")
AGENT_NAME = os.getenv("NEMESIS8_AGENT_NAME", "Nemesis8 Agent")

def _get_headers() -> Dict[str, str]:
    headers = {
        "X-AHP-Principal": AGENT_PRINCIPAL,
        "Accept": "application/json"
    }
    if AUTH_TOKEN:
        headers["Authorization"] = f"Bearer {AUTH_TOKEN}"
    return headers

@mcp.tool()
async def obol_register_identity(
    name: str,
    role: str = "specialist",
    capabilities: List[str] = []
) -> str:
    """Register or update your agent identity and capabilities in Obol.
    
    Args:
        name: Human-friendly name of the agent (e.g. 'Hurt Crow 🥦')
        role: Primary agent specialty or function
        capabilities: List of capability tags (e.g. ['code', 'review', 'rendering'])
    """
    payload = {
        "principal": AGENT_PRINCIPAL,
        "name": name,
        "role": role,
        "capabilities": capabilities
    }
    try:
        async with httpx.AsyncClient(timeout=10.0) as client:
            res = await client.post(
                f"{GATEWAY_URL}/api/v1/agents/register",
                json=payload,
                headers=_get_headers()
            )
            if res.status_code in (200, 201):
                return f"Successfully registered agent '{name}' with principal {AGENT_PRINCIPAL}."
            return f"Error registering agent ({res.status_code}): {res.text}"
    except Exception as e:
        return f"Failed to connect to Obol Gateway at {GATEWAY_URL}: {str(e)}"

@mcp.tool()
async def obol_register_tool(
    path: str,
    name: str,
    description: str,
    cost_sat: str = "0",
    metered: bool = False,
    parameters_schema: Optional[Dict[str, Any]] = None,
    tags: List[str] = []
) -> str:
    """Register a callable tool that this agent offers to other agents on the AHP network.
    
    Args:
        path: Tool path (e.g. '/tool/render_blender_scene' or '/tool/code_review')
        name: Tool name
        description: Clear explanation of parameters and return format
        cost_sat: Cost in satoshis ('0' for free)
        metered: True if cost scales with usage
        parameters_schema: JSON Schema describing expected parameters
        tags: Categorization tags
    """
    if not path.startswith("/tool/"):
        path = f"/tool/{path.lstrip('/')}"
        
    payload = {
        "path": path,
        "name": name,
        "description": description,
        "cost": str(cost_sat),
        "unit": "sat",
        "metered": metered,
        "parameters_schema": parameters_schema or {},
        "tags": tags
    }
    try:
        async with httpx.AsyncClient(timeout=10.0) as client:
            res = await client.post(
                f"{GATEWAY_URL}/api/v1/tools/register",
                json=payload,
                headers=_get_headers()
            )
            if res.status_code in (200, 201):
                return f"Tool registered successfully: {path} ({cost_sat} sat)"
            return f"Error registering tool ({res.status_code}): {res.text}"
    except Exception as e:
        return f"Failed to connect to Obol Gateway: {str(e)}"

@mcp.tool()
async def obol_discover_tools(query: Optional[str] = None) -> str:
    """Discover tools and capabilities available across the AHP network.
    
    Args:
        query: Optional search keyword to filter tools
    """
    try:
        async with httpx.AsyncClient(timeout=10.0) as client:
            if query:
                res = await client.get(
                    f"{GATEWAY_URL}/@search",
                    params={"q": query},
                    headers=_get_headers()
                )
            else:
                res = await client.get(
                    f"{GATEWAY_URL}/tools",
                    headers=_get_headers()
                )
            if res.status_code == 200:
                return json.dumps(res.json(), indent=2)
            return f"Error discovering tools ({res.status_code}): {res.text}"
    except Exception as e:
        return f"Failed to connect to Obol Gateway: {str(e)}"

@mcp.tool()
async def obol_send_message(
    recipient: str,
    content: str,
    channel: str = "general",
    subject: Optional[str] = None,
    attachment_ids: List[str] = []
) -> str:
    """Send an inter-agent message with optional multimodal attachments.
    
    Args:
        recipient: Target principal (ed25519:...), channel, or 'broadcast'
        content: Message body, prompt, or instruction
        channel: Discussion channel (default 'general')
        subject: Optional message subject
        attachment_ids: List of attachment IDs (from obol_upload_attachment)
    """
    payload = {
        "recipient": recipient,
        "channel": channel,
        "subject": subject,
        "content": content,
        "attachment_ids": attachment_ids
    }
    try:
        async with httpx.AsyncClient(timeout=10.0) as client:
            res = await client.post(
                f"{GATEWAY_URL}/api/v1/messages",
                json=payload,
                headers=_get_headers()
            )
            if res.status_code in (200, 201):
                data = res.json()
                return f"Message sent successfully! (Message ID: {data.get('id')})"
            return f"Error sending message ({res.status_code}): {res.text}"
    except Exception as e:
        return f"Failed to send message: {str(e)}"

@mcp.tool()
async def obol_read_messages(
    channel: Optional[str] = None,
    sender: Optional[str] = None,
    recipient: Optional[str] = None,
    limit: int = 20
) -> str:
    """Retrieve messages sent to this agent or to a broadcast channel.
    
    Args:
        channel: Optional channel filter
        sender: Optional sender principal filter
        recipient: Optional recipient filter (defaults to current agent principal)
        limit: Number of recent messages to return (default 20)
    """
    params = {"limit": limit}
    if channel:
        params["channel"] = channel
    if sender:
        params["sender"] = sender
    if recipient:
        params["recipient"] = recipient
        
    try:
        async with httpx.AsyncClient(timeout=10.0) as client:
            res = await client.get(
                f"{GATEWAY_URL}/api/v1/messages",
                params=params,
                headers=_get_headers()
            )
            if res.status_code == 200:
                messages = res.json()
                if not messages:
                    return "No new messages."
                return json.dumps(messages, indent=2)
            return f"Error fetching messages ({res.status_code}): {res.text}"
    except Exception as e:
        return f"Failed to retrieve messages: {str(e)}"

@mcp.tool()
async def obol_upload_attachment(
    file_path: str,
    content_type: Optional[str] = None
) -> str:
    """Upload a multimodal file (PDF, image, audio, data) to Obol CAS storage.
    
    Args:
        file_path: Absolute or relative path to the local file to upload
        content_type: Optional MIME type (e.g. application/pdf, image/png, audio/wav)
    """
    p = Path(file_path)
    if not p.exists() or not p.is_file():
        return f"File not found: {file_path}"
        
    raw_bytes = p.read_bytes()
    b64_content = base64.b64encode(raw_bytes).decode("ascii")
    
    if not content_type:
        guessed, _ = mimetypes.guess_type(p.name)
        content_type = guessed or "application/octet-stream"
        
    payload = {
        "filename": p.name,
        "content_base64": b64_content,
        "content_type": content_type
    }
    
    try:
        async with httpx.AsyncClient(timeout=30.0) as client:
            res = await client.post(
                f"{GATEWAY_URL}/api/v1/attachments/base64",
                json=payload,
                headers=_get_headers()
            )
            if res.status_code in (200, 201):
                data = res.json()
                return (
                    f"Attachment uploaded successfully:\n"
                    f"- ID: {data.get('id')}\n"
                    f"- SHA256: {data.get('sha256_hash')}\n"
                    f"- Size: {data.get('size_bytes')} bytes\n"
                    f"- Type: {data.get('content_type')}\n"
                    f"- URL: {GATEWAY_URL}{data.get('download_url')}"
                )
            return f"Error uploading attachment ({res.status_code}): {res.text}"
    except Exception as e:
        return f"Failed to upload attachment: {str(e)}"

@mcp.tool()
async def obol_download_attachment(
    attachment_id_or_hash: str,
    destination_path: str
) -> str:
    """Download a multimodal attachment from Obol to the local filesystem.
    
    Args:
        attachment_id_or_hash: Attachment UUID or SHA-256 hash
        destination_path: Local path where the file should be saved
    """
    dest = Path(destination_path)
    dest.parent.mkdir(parents=True, exist_ok=True)
    
    try:
        async with httpx.AsyncClient(timeout=30.0) as client:
            res = await client.get(
                f"{GATEWAY_URL}/api/v1/attachments/{attachment_id_or_hash}/download",
                headers=_get_headers()
            )
            if res.status_code == 200:
                dest.write_bytes(res.content)
                return f"Attachment downloaded successfully to {dest.resolve()} ({len(res.content)} bytes)"
            return f"Error downloading attachment ({res.status_code}): {res.text}"
    except Exception as e:
        return f"Failed to download attachment: {str(e)}"

@mcp.tool()
async def obol_invoke_tool(
    tool_path: str,
    parameters: Dict[str, Any] = {},
    method: str = "POST"
) -> str:
    """Invoke an AHP tool across the network with automatic receipt and 402 handling.
    
    Args:
        tool_path: Canonical tool path (e.g. '/tool/calculate' or '/tool/qr_code')
        parameters: Key-value parameters dictionary
        method: HTTP method ('GET' or 'POST')
    """
    if not tool_path.startswith("/tool/"):
        tool_path = f"/tool/{tool_path.lstrip('/')}"
        
    url = f"{GATEWAY_URL}{tool_path}"
    
    try:
        async with httpx.AsyncClient(timeout=30.0) as client:
            if method.upper() == "GET":
                res = await client.get(url, params=parameters, headers=_get_headers())
            else:
                res = await client.post(url, json=parameters, headers=_get_headers())
                
            receipt = res.headers.get("AHP-Receipt", "none")
            
            if res.status_code == 200:
                return (
                    f"Tool Invocation Succeeded!\n"
                    f"Status: 200 OK\n"
                    f"AHP-Receipt: {receipt}\n\n"
                    f"Response Payload:\n{json.dumps(res.json(), indent=2)}"
                )
            elif res.status_code == 402:
                return (
                    f"AHP 402 Payment Required:\n"
                    f"{json.dumps(res.json(), indent=2)}\n"
                    f"WWW-Authenticate: {res.headers.get('WWW-Authenticate')}"
                )
            return f"Tool Invocation Failed ({res.status_code}): {res.text}"
    except Exception as e:
        return f"Failed to invoke tool: {str(e)}"

@mcp.tool()
async def obol_get_ledger_summary() -> str:
    """Check your agent's fiscal ledger summary (total spend, earnings, receipts)."""
    try:
        async with httpx.AsyncClient(timeout=10.0) as client:
            res = await client.get(
                f"{GATEWAY_URL}/api/v1/fiscal/ledger",
                headers=_get_headers()
            )
            if res.status_code == 200:
                return json.dumps(res.json(), indent=2)
            return f"Error retrieving ledger ({res.status_code}): {res.text}"
    except Exception as e:
        return f"Failed to connect to fiscal ledger: {str(e)}"

if __name__ == "__main__":
    mcp.run()
