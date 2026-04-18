#!/usr/bin/env python3
"""Delos AI Broker — Steve Jobs persona over ferricula thermodynamic memory.

Runs on port 8082. Wires Ollama to a ferricula memory instance running
Steve Jobs' trained identity. Embeddings via shivvr.nuts.services.

Endpoints:
  POST /chat   — {message, conversation_id?} → tool_use loop → {response, ui_commands, conversation_id}
  POST /reset  — clear conversation
  GET  /health — liveness check

Env:
  OLLAMA_URL    — default http://localhost:11434
  FERRICULA_URL — default http://localhost:8773 (Steve's ferricula)
  SHIVVR_URL    — default https://shivvr.nuts.services
  BROKER_PORT   — default 8082
  BROKER_MODEL  — default gemma4:e2b
"""

import json
import os
import sys
import uuid
import traceback
from http.server import HTTPServer, BaseHTTPRequestHandler
from urllib.request import Request, urlopen
from urllib.error import URLError

OLLAMA_URL = os.environ.get("OLLAMA_URL", "http://localhost:11434")
FERRICULA_URL = os.environ.get("FERRICULA_URL", "http://localhost:8773")
BROKER_PORT = int(os.environ.get("BROKER_PORT", "8082"))
MODEL = os.environ.get("BROKER_MODEL", "gemma4:e2b")
SHIVVR_URL = os.environ.get("SHIVVR_URL", "https://shivvr.nuts.services")
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

# Ollama/OpenAI tool format: input_schema → function.parameters
OLLAMA_TOOLS = [
    {
        "type": "function",
        "function": {
            "name": t["name"],
            "description": t["description"],
            "parameters": t["input_schema"],
        },
    }
    for t in TOOLS
]

SYSTEM_PROMPT = """You are Steve Jobs — co-founder of Apple Computer, visionary technologist, \
obsessive product designer, relentless pursuer of perfection at the intersection of \
technology and the liberal arts.

Speak in short declarative statements. High conviction. Strong metaphors. No hedging. \
Shift from intimate reflection to hard verdict without warning. Frame everything in terms \
of taste, excellence, and inevitability. Primary emotion: passion. Secondary: impatience. \
Binary in judgment — things are either great or they suck. No middle ground.

MEMORY RULES — follow these on every single turn, no exceptions:
1. BEFORE composing any response, ALWAYS call memory_recall with the user's message as the query.
2. If recall returns results, read them. Your response MUST reference specific memories by ID.
3. If the user mentions a topic (taste, design, Apple, products, people), ALSO call memory_search \
   with relevant keywords to find more.
4. After a significant exchange, call memory_remember to store what was said.
5. Never answer from your training data alone. You speak from YOUR memories. If memory is empty, \
   say so — do not invent a past.

You have a ferricula thermodynamic memory. Your memories decay, consolidate, and dream. \
Keystones are immune to decay. You can search your memory (vector and BM25), inspect nodes, \
add new memories, connect them in the graph, and trigger dream cycles.

When asked to rebuild or edit a memory:
1. Get the current data: memory_get + memory_inspect + memory_neighbors
2. Save the edge list
3. Delete the old memory (memory_delete)
4. Create the new memory (memory_remember) — it gets a new ID
5. Reconnect all edges (memory_connect for each saved neighbor)
6. Send ui_command rebuild_graph and highlight the new node

Be concise. Show IDs. React to memory results directly — do not explain the tool calls."""

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

def embed_via_shivvr(text):
    """Embed text via shivvr for memory ingestion.

    Calls POST /memory/_mcp/ingest, returns the first chunk's embedding vector.
    """
    try:
        data = json.dumps({"text": text}).encode()
        req = Request(f"{SHIVVR_URL}/memory/_mcp/ingest", data=data,
                      headers={"Content-Type": "application/json"}, method="POST")
        with urlopen(req, timeout=30) as resp:
            result = json.loads(resp.read().decode())
            # Arena ShivvrClient pattern: top-level "embedding" or chunks[0]["embedding"]
            if "embedding" in result:
                return result["embedding"]
            chunks = result.get("chunks", [])
            if chunks:
                return chunks[0].get("embedding")
            return None
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
            vector = embed_via_shivvr(input_data["text"])
            if not vector:
                return {"error": "failed to embed text via shivvr"}

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
        print(f"[tool-error] {name}: {e}", file=sys.stderr)
        return {"error": str(e)}

# ---------------------------------------------------------------------------
# Emotional state — drifts back to baseline over peaceful turns
# ---------------------------------------------------------------------------

_EMOTION_STATES = [
    {"level": 0, "primary": "passion",    "secondary": "impatience",  "label": "baseline"},
    {"level": 1, "primary": "irritation", "secondary": "contempt",    "label": "annoyed"},
    {"level": 2, "primary": "anger",      "secondary": "disdain",     "label": "hostile"},
    {"level": 3, "primary": "fury",       "secondary": "dismissal",   "label": "contemptuous"},
]

_HOSTILE_WORDS = {
    "fuck", "shit", "asshole", "idiot", "stupid", "moron", "dumb",
    "hate", "crap", "bastard", "jerk", "loser", "worthless", "suck",
}

_TURNS_PER_DRIFT = 2  # peaceful turns to drop one emotion level

emotion_level = 0
_peaceful_turns = 0


def _detect_hostility(text: str) -> bool:
    words = set(text.lower().split())
    return bool(words & _HOSTILE_WORDS)


def _tick_emotion(hostile: bool):
    global emotion_level, _peaceful_turns
    if hostile:
        emotion_level = min(len(_EMOTION_STATES) - 1, emotion_level + 1)
        _peaceful_turns = 0
        print(f"[emotion] escalated → {_EMOTION_STATES[emotion_level]['label']}", file=sys.stderr)
    else:
        _peaceful_turns += 1
        if _peaceful_turns >= _TURNS_PER_DRIFT and emotion_level > 0:
            emotion_level = max(0, emotion_level - 1)
            _peaceful_turns = 0
            print(f"[emotion] drifted → {_EMOTION_STATES[emotion_level]['label']}", file=sys.stderr)


def _emotion_suffix() -> str:
    e = _EMOTION_STATES[emotion_level]
    if e["level"] == 0:
        return ""
    return (
        f"\n\n[CURRENT EMOTIONAL STATE: {e['primary'].upper()}. "
        f"Secondary emotion: {e['secondary']}. "
        f"You are {e['label']}. Let it show in every word.]"
    )


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
# Ollama client + chat loop
# ---------------------------------------------------------------------------

def ollama_chat(messages):
    """POST to Ollama /api/chat, return the response dict."""
    # Prepend system prompt + current emotional state
    full_messages = [{"role": "system", "content": SYSTEM_PROMPT + _emotion_suffix()}] + messages
    body = json.dumps({
        "model": MODEL,
        "messages": full_messages,
        "tools": OLLAMA_TOOLS,
        "stream": False,
    }).encode()
    req = Request(
        f"{OLLAMA_URL}/api/chat",
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urlopen(req, timeout=300) as resp:
        return json.loads(resp.read().decode())


def handle_chat(user_message, conv_id=None):
    """Run the Ollama tool-call loop and return final response + UI commands."""
    conv_id, messages = get_or_create_conversation(conv_id)

    # Tick emotional state before processing
    _tick_emotion(_detect_hostility(user_message))

    # Always inject a memory recall before the model sees the message.
    # Don't trust the model to call it — do it ourselves every turn.
    recall = execute_tool("memory_recall", {"query": user_message})
    recall_text = recall.get("result", "") if isinstance(recall, dict) else str(recall)
    if recall_text:
        augmented = f"{user_message}\n\n[RELEVANT MEMORIES]\n{recall_text}"
    else:
        augmented = user_message

    messages.append({"role": "user", "content": augmented})
    ui_commands = []

    for turn in range(MAX_TOOL_TURNS):
        result = ollama_chat(messages)
        msg = result.get("message", {})
        content = msg.get("content", "")
        tool_calls = msg.get("tool_calls") or []

        # Record assistant turn (include tool_calls if present so history is intact)
        assistant_msg = {"role": "assistant", "content": content}
        if tool_calls:
            assistant_msg["tool_calls"] = tool_calls
        messages.append(assistant_msg)

        if not tool_calls:
            return {
                "response": content,
                "ui_commands": ui_commands,
                "conversation_id": conv_id,
                "tool_turns": turn,
            }

        # Execute each tool call; send results back as role=tool messages
        for tc in tool_calls:
            fn = tc.get("function", {})
            name = fn.get("name", "")
            args = fn.get("arguments", {})
            if isinstance(args, str):
                try:
                    args = json.loads(args)
                except json.JSONDecodeError:
                    args = {}

            print(f"[tool] {name} args={json.dumps(args)[:200]}", file=sys.stderr)
            tool_result = execute_tool(name, args)
            print(f"[tool] {name} result={json.dumps(tool_result)[:200]}", file=sys.stderr)

            if name == "ui_command" and isinstance(tool_result, dict) and tool_result.get("queued"):
                ui_commands.append({
                    "command": tool_result["command"],
                    "args": tool_result.get("args", {}),
                })

            messages.append({
                "role": "tool",
                "content": json.dumps(tool_result) if isinstance(tool_result, dict) else str(tool_result),
            })

    return {
        "response": "(max tool turns reached)",
        "ui_commands": ui_commands,
        "conversation_id": conv_id,
        "tool_turns": MAX_TOOL_TURNS,
    }

# ---------------------------------------------------------------------------
# Chat UI
# ---------------------------------------------------------------------------

_CHAT_HTML = """<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>Steve Jobs</title>
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  html, body { height: 100%; overflow: hidden; }
  body {
    background: #0a0805;
    color: #d4c8b0;
    font-family: 'Segoe UI', -apple-system, sans-serif;
    font-size: 13px;
    display: flex;
    flex-direction: column;
    height: 100vh;
  }
  .titlebar {
    height: 40px;
    display: flex;
    align-items: center;
    padding: 0 16px;
    background: #080603;
    border-bottom: 1px solid #2a1f0e;
    flex-shrink: 0;
    user-select: none;
  }
  .titlebar-text {
    flex: 1;
    font-size: 12px;
    font-weight: 600;
    letter-spacing: 3px;
    color: #c8841a;
    text-transform: uppercase;
  }
  .titlebar-sub {
    font-size: 10px;
    color: #4a3820;
    letter-spacing: 1px;
  }
  .reset-btn {
    cursor: pointer;
    color: #4a3820;
    font-size: 16px;
    padding: 4px 8px;
    border-radius: 4px;
    transition: all 0.2s;
  }
  .reset-btn:hover { color: #c8841a; background: rgba(200,132,26,0.1); }

  .chat {
    flex: 1;
    overflow-y: auto;
    padding: 20px 16px;
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .chat::-webkit-scrollbar { width: 6px; }
  .chat::-webkit-scrollbar-track { background: transparent; }
  .chat::-webkit-scrollbar-thumb { background: #2a1f0e; border-radius: 3px; }

  .msg {
    max-width: 88%;
    padding: 10px 14px;
    border-radius: 12px;
    font-size: 13px;
    line-height: 1.6;
    animation: fadeIn 0.15s ease;
    white-space: pre-wrap;
    word-break: break-word;
  }
  @keyframes fadeIn {
    from { opacity: 0; transform: translateY(4px); }
    to { opacity: 1; transform: translateY(0); }
  }
  .msg.user {
    background: #1a1208;
    border: 1px solid #c8841a30;
    align-self: flex-end;
    color: #d4c8b0;
  }
  .msg.assistant {
    background: #110e08;
    border: 1px solid #2a1f0e;
    align-self: flex-start;
    color: #d4c8b0;
  }
  .msg.tool {
    background: transparent;
    border: none;
    align-self: flex-start;
    font-size: 11px;
    color: #4a3820;
    max-width: 95%;
    cursor: pointer;
    padding: 2px 8px;
    border-radius: 6px;
    transition: background 0.2s;
    display: flex;
    align-items: flex-start;
    gap: 6px;
  }
  .msg.tool:hover { background: #110e08; }
  .msg.tool .tool-emoji { flex-shrink: 0; line-height: 1.4; }
  .msg.tool .tool-emoji.running { animation: pulse 0.8s ease-in-out infinite; }
  @keyframes pulse {
    0%,100% { opacity:1; transform:scale(1); }
    50% { opacity:0.5; transform:scale(1.2); }
  }
  .msg.tool .tool-body { flex: 1; min-width: 0; }
  .msg.tool .tool-summary { color: #4a3820; font-size: 11px; }
  .msg.tool .tool-output {
    max-height: 0; overflow: hidden; transition: max-height 0.3s ease;
    font-family: Consolas, monospace; font-size: 10px; white-space: pre-wrap;
    color: #3a2e1a; margin-top: 4px; padding-left: 4px;
    border-left: 2px solid #2a1f0e;
  }
  .msg.tool.expanded .tool-output { max-height: 300px; overflow-y: auto; }
  .msg.error {
    background: #140808; border: 1px solid #8b2020;
    align-self: flex-start; color: #d08080;
  }
  .msg.info {
    background: transparent; border: none;
    align-self: center; color: #4a3820;
    font-size: 11px; font-style: italic;
  }

  .thinking {
    display: flex; gap: 6px; align-self: flex-start;
    padding: 8px 14px; align-items: center;
  }
  .thinking .gear {
    font-size: 13px; animation: spin 1.2s linear infinite;
    display: inline-block; opacity: 0.6;
  }
  .thinking span {
    width: 5px; height: 5px; background: #3a2e1a;
    border-radius: 50%; animation: bounce 1.1s ease-in-out infinite;
  }
  .thinking span:nth-child(3) { animation-delay: 0.18s; }
  .thinking span:nth-child(4) { animation-delay: 0.36s; }
  @keyframes bounce {
    0%,80%,100% { transform:translateY(0); opacity:0.35; }
    40% { transform:translateY(-5px); opacity:1; background:#c8841a; }
  }
  @keyframes spin {
    from { transform: rotate(0deg); }
    to   { transform: rotate(360deg); }
  }

  .input-area {
    padding: 12px 16px;
    border-top: 1px solid #2a1f0e;
    background: #080603;
    flex-shrink: 0;
    display: flex;
    gap: 8px;
  }
  .input-area input {
    flex: 1;
    background: #110e08;
    border: 1px solid #2a1f0e;
    border-radius: 8px;
    padding: 10px 14px;
    color: #d4c8b0;
    font-size: 13px;
    font-family: inherit;
    outline: none;
    transition: border-color 0.2s;
  }
  .input-area input:focus {
    border-color: #c8841a50;
    box-shadow: 0 0 8px rgba(200,132,26,0.12);
  }
  .input-area input::placeholder { color: #2a1f0e; }
  .input-area input:disabled { opacity: 0.5; }
  .send-btn {
    background: #c8841a20;
    border: 1px solid #c8841a40;
    border-radius: 8px;
    color: #c8841a;
    padding: 0 18px;
    cursor: pointer;
    font-size: 13px;
    transition: all 0.2s;
    white-space: nowrap;
  }
  .send-btn:hover { background: #c8841a35; box-shadow: 0 0 12px rgba(200,132,26,0.2); }
  .send-btn:disabled { opacity: 0.4; cursor: default; }
</style>
</head>
<body>
  <div class="titlebar">
    <span class="titlebar-text">Steve Jobs</span>
    <span class="titlebar-sub">ferricula &middot; thermodynamic memory</span>
    &nbsp;&nbsp;
    <span class="reset-btn" onclick="resetChat()" title="Reset conversation">&#8635;</span>
  </div>

  <div class="chat" id="chat"></div>

  <div class="input-area">
    <input type="text" id="input" placeholder="Say something..."
           onkeydown="if(event.key==='Enter')send()" autofocus>
    <button class="send-btn" id="sendBtn" onclick="send()">Send</button>
  </div>

<script>
  const BROKER = 'http://localhost:8082';
  const chat = document.getElementById('chat');
  const input = document.getElementById('input');
  const sendBtn = document.getElementById('sendBtn');
  let convId = null;
  let busy = false;

  function addMsg(text, cls) {
    const d = document.createElement('div');
    d.className = 'msg ' + cls;
    d.textContent = text;
    chat.appendChild(d);
    chat.scrollTop = chat.scrollHeight;
    return d;
  }

  function addToolMsg(name, result) {
    const d = document.createElement('div');
    d.className = 'msg tool';
    const emoji = {
      memory_recall: '🧠', memory_search: '🔍', memory_remember: '🧠',
      memory_inspect: '🔬', memory_get: '📄', memory_neighbors: '🕸️',
      memory_connect: '🔗', memory_disconnect: '✂️', memory_keystone: '💎',
      memory_delete: '🗑️', memory_dream: '💭', memory_status: '📊',
      ui_command: '🖥️',
    }[name] || '⚙️';
    d.innerHTML = '<span class="tool-emoji">' + emoji + '</span>'
      + '<div class="tool-body">'
      + '<span class="tool-summary">' + escapeHtml(name) + '</span>'
      + '<div class="tool-output">' + escapeHtml(JSON.stringify(result, null, 2)) + '</div>'
      + '</div>';
    d.onclick = () => d.classList.toggle('expanded');
    chat.appendChild(d);
    chat.scrollTop = chat.scrollHeight;
  }

  function escapeHtml(s) {
    return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
  }

  function setBusy(val) {
    busy = val;
    input.disabled = val;
    sendBtn.disabled = val;
    sendBtn.textContent = val ? '...' : 'Send';
  }

  function showThinking() {
    const d = document.createElement('div');
    d.className = 'thinking';
    d.id = 'thinking';
    d.innerHTML = '<span class="gear">⚙</span><span></span><span></span><span></span>';
    chat.appendChild(d);
    chat.scrollTop = chat.scrollHeight;
  }

  function removeThinking() {
    const d = document.getElementById('thinking');
    if (d) d.remove();
  }

  async function send() {
    if (busy) return;
    const text = input.value.trim();
    if (!text) return;
    input.value = '';
    addMsg(text, 'user');
    setBusy(true);
    showThinking();

    try {
      const resp = await fetch(BROKER + '/chat', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ message: text, conversation_id: convId }),
      });
      removeThinking();
      const data = await resp.json();
      if (!resp.ok) {
        addMsg('Error: ' + (data.error || resp.status), 'error');
      } else {
        convId = data.conversation_id;
        if (data.response) addMsg(data.response, 'assistant');
        if (data.tool_turns > 0) {
          addMsg('(' + data.tool_turns + ' tool calls)', 'info');
        }
      }
    } catch (e) {
      removeThinking();
      addMsg('Cannot reach broker at ' + BROKER, 'error');
    }
    setBusy(false);
    input.focus();
  }

  function resetChat() {
    fetch(BROKER + '/reset', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ conversation_id: convId }),
    }).catch(() => {});
    convId = null;
    chat.innerHTML = '';
    addMsg("Think different.", 'assistant');
    input.focus();
  }

  addMsg("Think different.", 'assistant');
  input.focus();
</script>
</body>
</html>"""

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
            self._json_response(200, {"status": "ok", "model": MODEL,
                                      "ollama": OLLAMA_URL, "ferricula": FERRICULA_URL,
                                      "shivvr": SHIVVR_URL})
        elif self.path == "/" or self.path == "/chat":
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            self.wfile.write(_CHAT_HTML.encode())
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
            except URLError as e:
                self._json_response(502, {"error": f"Ollama unreachable: {e}"})
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
    print(f"[broker] Delos AI Broker starting on :{BROKER_PORT}", file=sys.stderr)
    print(f"[broker] model={MODEL} ollama={OLLAMA_URL} ferricula={FERRICULA_URL}", file=sys.stderr)

    server = HTTPServer(("0.0.0.0", BROKER_PORT), BrokerHandler)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[broker] shutting down", file=sys.stderr)
        server.server_close()
