#!/usr/bin/env python3
"""Delos AI Broker — lightweight tool_use bridge between Anthropic API and ferricula.

Runs on port 8082. Gives an AI agent full control over ferricula memory operations
and dashboard UI commands via 13 tools.

Endpoints:
  POST /chat   — {message, conversation_id?} → tool_use loop → {response, ui_commands, conversation_id}
  POST /reset  — clear conversation
  GET  /health — liveness check

Requires: ANTHROPIC_API_KEY env var, anthropic Python SDK.
"""

import json
import os
import sys
import uuid
import traceback
from http.server import HTTPServer, BaseHTTPRequestHandler
from urllib.request import Request, urlopen
from urllib.error import URLError

try:
    import anthropic
except ImportError:
    print("ERROR: anthropic package required. pip install anthropic", file=sys.stderr)
    sys.exit(1)

FERRICULA_URL = os.environ.get("FERRICULA_URL", "http://localhost:8780")
BROKER_PORT = int(os.environ.get("BROKER_PORT", "8082"))
MODEL = os.environ.get("BROKER_MODEL", "claude-sonnet-4-5-20250929")
MAX_TOOL_TURNS = 15

# ---------------------------------------------------------------------------
# Tool definitions
# ---------------------------------------------------------------------------

TOOLS = [
    {
        "name": "memory_recall",
        "description": "Vector search (cosine similarity) for memories. Returns ranked results.",
        "input_schema": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Natural language query"},
            },
            "required": ["query"],
        },
    },
    {
        "name": "memory_search",
        "description": "BM25 keyword search for memories. Returns ranked results with calibrated probabilities.",
        "input_schema": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query (keywords)"},
                "k": {"type": "integer", "description": "Max results (default 10)", "default": 10},
            },
            "required": ["query"],
        },
    },
    {
        "name": "memory_inspect",
        "description": "Get detailed info about a memory: fidelity, state, emotions, graph degree, etc.",
        "input_schema": {
            "type": "object",
            "properties": {
                "id": {"type": "integer", "description": "Memory ID"},
            },
            "required": ["id"],
        },
    },
    {
        "name": "memory_get",
        "description": "Get the raw data (tags, vector dimension) for a memory by ID.",
        "input_schema": {
            "type": "object",
            "properties": {
                "id": {"type": "integer", "description": "Memory ID"},
            },
            "required": ["id"],
        },
    },
    {
        "name": "memory_neighbors",
        "description": "Get graph neighbors of a memory (connected nodes and edge labels).",
        "input_schema": {
            "type": "object",
            "properties": {
                "id": {"type": "integer", "description": "Memory ID"},
            },
            "required": ["id"],
        },
    },
    {
        "name": "memory_remember",
        "description": "Ingest a new memory. Requires embedding via chonk. Provide text and optional metadata.",
        "input_schema": {
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "Memory text content"},
                "channel": {"type": "string", "description": "Sensory channel (hearing/seeing/thinking)", "default": "hearing"},
                "emotion": {
                    "type": "object",
                    "description": "Emotion tag",
                    "properties": {
                        "primary": {"type": "string"},
                        "secondary": {"type": "string"},
                    },
                },
                "importance": {"type": "number", "description": "0.0-1.0"},
                "keystone": {"type": "boolean", "description": "Immune to decay"},
            },
            "required": ["text"],
        },
    },
    {
        "name": "memory_delete",
        "description": "Delete a memory by ID. Removes from engine, memory store, graph, and search index.",
        "input_schema": {
            "type": "object",
            "properties": {
                "id": {"type": "integer", "description": "Memory ID to delete"},
            },
            "required": ["id"],
        },
    },
    {
        "name": "memory_connect",
        "description": "Create a graph edge between two memories.",
        "input_schema": {
            "type": "object",
            "properties": {
                "a": {"type": "integer", "description": "First memory ID"},
                "b": {"type": "integer", "description": "Second memory ID"},
                "label": {"type": "string", "description": "Edge label (e.g. related, caused, contradicts)", "default": "related"},
            },
            "required": ["a", "b"],
        },
    },
    {
        "name": "memory_disconnect",
        "description": "Remove a graph edge between two memories.",
        "input_schema": {
            "type": "object",
            "properties": {
                "a": {"type": "integer", "description": "First memory ID"},
                "b": {"type": "integer", "description": "Second memory ID"},
            },
            "required": ["a", "b"],
        },
    },
    {
        "name": "memory_keystone",
        "description": "Toggle keystone status on a memory (immune to decay).",
        "input_schema": {
            "type": "object",
            "properties": {
                "id": {"type": "integer", "description": "Memory ID"},
            },
            "required": ["id"],
        },
    },
    {
        "name": "memory_dream",
        "description": "Trigger a dream cycle: decay, consolidate, prune, ghost echoes.",
        "input_schema": {
            "type": "object",
            "properties": {},
        },
    },
    {
        "name": "memory_status",
        "description": "Get overall system status: memory counts, graph stats, prime tree stats.",
        "input_schema": {
            "type": "object",
            "properties": {},
        },
    },
    {
        "name": "ui_command",
        "description": "Send a command to the Delos dashboard UI. Available commands: highlight_node(id), open_inspect(id), rebuild_graph, do_recall(query), log(message).",
        "input_schema": {
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Command name"},
                "args": {"type": "object", "description": "Command arguments"},
            },
            "required": ["command"],
        },
    },
]

SYSTEM_PROMPT = """You are the Delos AI assistant, an expert on the ferricula memory system.
You help users inspect, search, edit, and rebuild memories in the knowledge graph.

You have tools to search (vector and BM25), inspect, create, delete, and connect memories.
You can also send UI commands to highlight nodes and refresh the dashboard.

When asked to rebuild or edit a memory:
1. Get the current memory data (memory_get + memory_inspect + memory_neighbors)
2. Save the edge list
3. Delete the old memory (memory_delete)
4. Create the new memory (memory_remember) — it will get a new ID
5. Reconnect edges to the new ID (memory_connect for each saved edge)
6. Send ui_command to rebuild_graph and highlight the new node

Be concise. Show IDs and key data. Don't over-explain tool results."""

# ---------------------------------------------------------------------------
# Ferricula HTTP client
# ---------------------------------------------------------------------------

def ferricula_request(method, path, body=None):
    """Make a request to ferricula HTTP API."""
    url = f"{FERRICULA_URL}/{path.lstrip('/')}"
    data = json.dumps(body).encode() if body else None
    headers = {"Content-Type": "application/json"} if data else {}
    req = Request(url, data=data, headers=headers, method=method)
    try:
        with urlopen(req, timeout=30) as resp:
            return json.loads(resp.read().decode())
    except URLError as e:
        return {"error": str(e)}
    except json.JSONDecodeError:
        return {"error": "invalid JSON response"}

def embed_via_chonk(text):
    """Embed text via chonk for memory ingestion."""
    chonk_url = os.environ.get("CHONK_URL", "http://localhost:8080")
    try:
        data = json.dumps({"text": text}).encode()
        req = Request(f"{chonk_url}/embed", data=data,
                      headers={"Content-Type": "application/json"}, method="POST")
        with urlopen(req, timeout=30) as resp:
            result = json.loads(resp.read().decode())
            return result.get("vector") or result.get("embedding")
    except Exception:
        return None

# ---------------------------------------------------------------------------
# Tool execution
# ---------------------------------------------------------------------------

def execute_tool(name, input_data):
    """Execute a tool call and return the result string."""
    try:
        if name == "memory_recall":
            return ferricula_request("POST", "/recall", {"query": input_data["query"]})

        elif name == "memory_search":
            return ferricula_request("POST", "/search", {
                "query": input_data["query"],
                "k": input_data.get("k", 10),
            })

        elif name == "memory_inspect":
            return ferricula_request("GET", f"/inspect/{input_data['id']}")

        elif name == "memory_get":
            return ferricula_request("GET", f"/get/{input_data['id']}")

        elif name == "memory_neighbors":
            return ferricula_request("GET", f"/neighbors/{input_data['id']}")

        elif name == "memory_remember":
            # Need to embed text first
            vector = embed_via_chonk(input_data["text"])
            if not vector:
                return {"error": "failed to embed text via chonk"}

            # Get next ID
            maxid_resp = ferricula_request("GET", "/maxid")
            maxid_str = maxid_resp.get("maxid", "0")
            try:
                next_id = int(maxid_str) + 1
            except (ValueError, TypeError):
                next_id = 1

            body = {
                "id": next_id,
                "tags": {"text": input_data["text"]},
                "vector": vector,
            }
            if "channel" in input_data:
                body["tags"]["channel"] = input_data["channel"]
            if "emotion" in input_data:
                body["emotion"] = input_data["emotion"]
            if "importance" in input_data:
                body["importance"] = input_data["importance"]
            if "keystone" in input_data:
                body["keystone"] = input_data["keystone"]

            return ferricula_request("POST", "/remember", body)

        elif name == "memory_delete":
            return ferricula_request("DELETE", f"/delete/{input_data['id']}")

        elif name == "memory_connect":
            return ferricula_request("POST", "/connect", {
                "a": input_data["a"],
                "b": input_data["b"],
                "label": input_data.get("label", "related"),
            })

        elif name == "memory_disconnect":
            return ferricula_request("POST", "/disconnect", {
                "a": input_data["a"],
                "b": input_data["b"],
            })

        elif name == "memory_keystone":
            return ferricula_request("POST", f"/keystone/{input_data['id']}")

        elif name == "memory_dream":
            return ferricula_request("POST", "/dream", {})

        elif name == "memory_status":
            return ferricula_request("GET", "/status")

        elif name == "ui_command":
            # UI commands are not sent to ferricula — they're returned to the dashboard
            return {"queued": True, "command": input_data["command"], "args": input_data.get("args", {})}

        else:
            return {"error": f"unknown tool: {name}"}

    except Exception as e:
        return {"error": str(e)}

# ---------------------------------------------------------------------------
# Conversation state
# ---------------------------------------------------------------------------

conversations = {}  # conversation_id -> messages list

def get_or_create_conversation(conv_id=None):
    if conv_id and conv_id in conversations:
        return conv_id, conversations[conv_id]
    new_id = conv_id or str(uuid.uuid4())[:8]
    conversations[new_id] = []
    return new_id, conversations[new_id]

# ---------------------------------------------------------------------------
# Chat handler — tool_use loop
# ---------------------------------------------------------------------------

def handle_chat(user_message, conv_id=None):
    """Run the Anthropic tool_use loop and return final response + UI commands."""
    client = anthropic.Anthropic()
    conv_id, messages = get_or_create_conversation(conv_id)

    messages.append({"role": "user", "content": user_message})
    ui_commands = []

    for turn in range(MAX_TOOL_TURNS):
        response = client.messages.create(
            model=MODEL,
            max_tokens=4096,
            system=SYSTEM_PROMPT,
            tools=TOOLS,
            messages=messages,
        )

        # Collect text and tool_use blocks
        assistant_content = response.content
        messages.append({"role": "assistant", "content": assistant_content})

        # Check if we're done (no tool use)
        if response.stop_reason == "end_turn":
            # Extract text from response
            text_parts = [b.text for b in assistant_content if b.type == "text"]
            return {
                "response": "\n".join(text_parts),
                "ui_commands": ui_commands,
                "conversation_id": conv_id,
                "tool_turns": turn,
            }

        # Process tool calls
        tool_results = []
        for block in assistant_content:
            if block.type == "tool_use":
                result = execute_tool(block.name, block.input)

                # Capture UI commands
                if block.name == "ui_command" and isinstance(result, dict) and result.get("queued"):
                    ui_commands.append({
                        "command": result["command"],
                        "args": result.get("args", {}),
                    })

                tool_results.append({
                    "type": "tool_result",
                    "tool_use_id": block.id,
                    "content": json.dumps(result) if isinstance(result, dict) else str(result),
                })

        messages.append({"role": "user", "content": tool_results})

    # Hit max turns — return what we have
    text_parts = []
    for block in messages[-1].get("content", []) if isinstance(messages[-1].get("content"), list) else []:
        if hasattr(block, "text"):
            text_parts.append(block.text)

    return {
        "response": "\n".join(text_parts) if text_parts else "(max tool turns reached)",
        "ui_commands": ui_commands,
        "conversation_id": conv_id,
        "tool_turns": MAX_TOOL_TURNS,
    }

# ---------------------------------------------------------------------------
# HTTP Server
# ---------------------------------------------------------------------------

class BrokerHandler(BaseHTTPRequestHandler):
    def do_OPTIONS(self):
        self.send_response(200)
        self._cors_headers()
        self.end_headers()

    def do_GET(self):
        if self.path == "/health":
            self._json_response(200, {"status": "ok", "model": MODEL, "ferricula": FERRICULA_URL})
        else:
            self._json_response(404, {"error": "not found"})

    def do_POST(self):
        try:
            content_length = int(self.headers.get("Content-Length", 0))
            body = json.loads(self.rfile.read(content_length)) if content_length > 0 else {}
        except (json.JSONDecodeError, ValueError):
            self._json_response(400, {"error": "invalid JSON"})
            return

        if self.path == "/chat":
            message = body.get("message", "").strip()
            if not message:
                self._json_response(400, {"error": "message required"})
                return
            try:
                result = handle_chat(message, body.get("conversation_id"))
                self._json_response(200, result)
            except anthropic.APIError as e:
                self._json_response(502, {"error": f"Anthropic API error: {e}"})
            except Exception as e:
                traceback.print_exc()
                self._json_response(500, {"error": str(e)})

        elif self.path == "/reset":
            conv_id = body.get("conversation_id")
            if conv_id and conv_id in conversations:
                del conversations[conv_id]
            self._json_response(200, {"reset": True})

        else:
            self._json_response(404, {"error": "not found"})

    def _json_response(self, status, data):
        self.send_response(status)
        self._cors_headers()
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(data).encode())

    def _cors_headers(self):
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type")

    def log_message(self, format, *args):
        print(f"[broker] {args[0]}", file=sys.stderr)

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    if not os.environ.get("ANTHROPIC_API_KEY"):
        print("ERROR: ANTHROPIC_API_KEY not set", file=sys.stderr)
        sys.exit(1)

    print(f"[broker] Delos AI Broker starting on :{BROKER_PORT}", file=sys.stderr)
    print(f"[broker] model={MODEL} ferricula={FERRICULA_URL}", file=sys.stderr)

    server = HTTPServer(("0.0.0.0", BROKER_PORT), BrokerHandler)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[broker] shutting down", file=sys.stderr)
        server.server_close()
