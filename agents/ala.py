#!/opt/mcp-venv/bin/python3
"""
aLa — nemesis8 local agent (container-only)

Ollama backbone · MCP tools · Ferricula deep memory · Shivvr embeddings
Tool heat system: tracks call frequency, warns/blocks overheating tools,
saves overheat events + suggestions to Ferricula for future sessions.

Arena: before finalising a response, confers with Ferricula archetypes.
If score < 5, injects their guidance and revises.

Usage (via nemesis8 --provider ala):
  nemesis8 interactive --provider ala
  nemesis8 run "summarise this repo" --provider ala
"""

import argparse
import json
import os
import signal
import subprocess
import sys
import threading
import time
from collections import defaultdict
from typing import Optional
import urllib.request
import urllib.error

# Set by Ctrl-C during a run to cancel the current turn (not exit)
_cancel = threading.Event()

# ── Terminal display ──────────────────────────────────────────────────────────

_NO_COLOR = not sys.stdout.isatty() or os.environ.get("NO_COLOR")

def _c(code: str, text: str) -> str:
    return text if _NO_COLOR else f"\033[{code}m{text}\033[0m"

def _bold(t):   return _c("1",  t)
def _dim(t):    return _c("2",  t)
def _red(t):    return _c("31", t)
def _green(t):  return _c("32", t)
def _yellow(t): return _c("33", t)
def _cyan(t):   return _c("36", t)
def _gray(t):   return _c("90", t)

_CLR = "\r\033[2K" if sys.stdout.isatty() else ""

def _show_thinking():
    print(f"  {_dim('… thinking')}", end="", flush=True)

def _clear_line():
    if sys.stdout.isatty():
        print(_CLR, end="", flush=True)

def _show_tool_start(name: str, args: dict):
    _clear_line()
    args_str = "  ".join(
        f"{k}={repr(v)[:40]}" for k, v in list(args.items())[:3]
    )
    print(f"  {_cyan('⚙')}  {_bold(name)}  {_gray(args_str)}", flush=True)

def _show_tool_end(result: str, duration: float, is_error: bool):
    icon = _red("✗") if is_error else _green("✓")
    size = f"{len(result):,} chars"
    print(f"  {icon}  {_dim(f'{size} · {duration:.1f}s')}", flush=True)

def _show_heat(msg: str, blocked: bool):
    icon = _red("⊗") if blocked else _yellow("△")
    print(f"  {icon}  {msg}", flush=True)

def _show_arena(score: float, guidance: str):
    color = _red if score < 3 else _yellow if score < 7 else _green
    print(f"  {_dim('arena')} {color(f'{score:.0f}/10')}  {_dim(guidance[:80])}", flush=True)

def _show_recall(text: str):
    if text:
        print(_dim("  ↑ recalled context"), flush=True)

_BANNER = r"""
    __      _          _____          _
    \_\    | |        / ____|        | |
    / \    | | __ _  | |     ___   __| | ___
   / _ \   | |/ _` | | |    / _ \ / _` |/ _ \
  / ___ \  | | (_| | | |___| (_) | (_| |  __/
 /_/   \_\ |_|\__,_|  \_____\___/ \__,_|\___|
"""

def _header(model: str, n_tools: int, ferricula_ok: bool):
    f_icon = _green("on") if ferricula_ok else _dim("off")
    print(_cyan(_BANNER), flush=True)
    print(f"  {_dim(model)}  *  {_dim(str(n_tools) + ' tools')}  *  ferricula {f_icon}", flush=True)
    print(f"  {_dim('Ctrl-C cancels turn  ·  exit to quit')}\n", flush=True)

# ── Config ────────────────────────────────────────────────────────────────────

_IN_DOCKER = os.path.exists("/.dockerenv")
_HOST      = "host.docker.internal" if _IN_DOCKER else "localhost"

OLLAMA_HOST   = os.environ.get("OLLAMA_HOST",   f"http://{_HOST}:11434")
OLLAMA_MODEL  = os.environ.get("OLLAMA_MODEL",  "gemma4:26b")
FERRICULA_URL = os.environ.get("FERRICULA_URL", f"http://{_HOST}:8765")
SHIVVR_URL    = os.environ.get("SHIVVR_URL",    "https://shivvr.nuts.services")
MCP_DIR       = os.environ.get("MCP_INSTALL",   "/opt/codex-home/mcp")
WORKSPACE     = os.environ.get("NEMESIS8_WORKSPACE", "/workspace")
MAX_TURNS     = int(os.environ.get("ALA_MAX_TURNS",  "25"))
TOOL_WARN     = int(os.environ.get("ALA_TOOL_WARN",  "3"))   # log a note
TOOL_HOT      = int(os.environ.get("ALA_TOOL_HOT",   "5"))   # inject warning
TOOL_OVER     = int(os.environ.get("ALA_TOOL_OVER",  "8"))   # block + save to Ferricula

# ── HTTP helpers ──────────────────────────────────────────────────────────────

def _post(url: str, body: dict, timeout: int = 30) -> Optional[dict]:
    try:
        data = json.dumps(body).encode()
        req = urllib.request.Request(
            url, data=data, headers={"Content-Type": "application/json"}
        )
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read().decode())
    except Exception:
        return None


def _get(url: str, timeout: int = 10) -> Optional[dict]:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return json.loads(r.read().decode())
    except Exception:
        return None


# ── Shivvr — embeddings ───────────────────────────────────────────────────────

def embed(text: str) -> list:
    """768-dim vector from Shivvr. Falls back to zeros on failure."""
    resp = _post(f"{SHIVVR_URL}/embed", {"text": text}, timeout=5)
    if resp:
        vec = resp.get("embedding", [])
        if len(vec) == 768:
            return vec
    return [0.0] * 768


# ── Ferricula client ──────────────────────────────────────────────────────────

class Ferricula:
    def __init__(self, base_url: str):
        self.base = base_url.rstrip("/")
        self.enabled = bool(base_url)

    def _next_id(self) -> int:
        resp = _get(f"{self.base}/maxid")
        if resp is None:
            return 1
        if isinstance(resp, dict):
            return int(resp.get("max_id", resp.get("result", 0))) + 1
        try:
            return int(resp) + 1
        except Exception:
            return 1

    def recall(self, query: str) -> str:
        """Hybrid BM25+vector recall. Returns formatted context block."""
        if not self.enabled:
            return ""
        resp = _post(f"{self.base}/hybrid", {"query": query, "k": 5, "weight": 0.4})
        if not resp:
            return ""
        results = resp.get("results", [])
        lines = [r["text"] for r in results if r.get("text")][:5]
        if not lines:
            return ""
        return "\n## Recalled memories\n" + "\n".join(f"- {l}" for l in lines)

    def remember(self, text: str, channel: str = "ala",
                 importance: float = 0.5, keystone: bool = False):
        if not self.enabled:
            return
        vec = embed(text)
        nid = self._next_id()
        _post(f"{self.base}/remember", {
            "id": nid,
            "tags": {"text": text[:500], "channel": channel, "source": "ala"},
            "vector": vec,
            "importance": importance,
            "keystone": keystone,
            "decay_alpha": 0.008 if importance > 0.7 else 0.01,
        })

    def remember_turn(self, role: str, content: str):
        text = content[:1000]
        vec = embed(text)
        nid = self._next_id()
        _post(f"{self.base}/remember", {
            "id": nid,
            "tags": {
                "text": text, "channel": "ala-history",
                "role": role, "source": "ala",
            },
            "vector": vec,
            "importance": 0.3,
            "decay_alpha": 0.015,
        })

    def tool_health(self, tool: str, observation: str, suggestion: str = ""):
        """Persist a tool-health observation. Keystone so it doesn't decay."""
        text = f"Tool '{tool}': {observation[:300]}"
        if suggestion:
            text += f" | Suggestion: {suggestion}"
        self.remember(text, channel="tool-health", importance=0.85, keystone=True)

    def confer(self, response: str, context: str) -> dict:
        """Ask the Ferricula arena (archetypes) to evaluate the response."""
        resp = _post(f"{self.base}/confer", {
            "text": response[:1000],
            "context": context[:500],
        })
        return resp or {}

    def dream(self):
        """Trigger Ferricula dream cycle (consolidation + decay)."""
        _post(f"{self.base}/dream", {})


ferricula = Ferricula(FERRICULA_URL)


# ── Tool heat tracker ─────────────────────────────────────────────────────────

class ToolHeat:
    """
    Tracks per-turn and lifetime tool call frequency.

    Heat levels:
      WARN  (3)  — log a note to stderr, no injection
      HOT   (5)  — inject warning into tool result
      OVER  (8)  — block tool, inject block message, save to Ferricula

    Saved Ferricula memories (channel="tool-health", keystone=True) are recalled
    at session start so future sessions know which tools tend to overheat and what
    alternatives were suggested.
    """

    def __init__(self):
        self.counts: dict   = defaultdict(int)
        self.lifetime: dict = defaultdict(int)
        self.blocked: set   = set()
        self._events: list  = []   # pending Ferricula writes for this turn

    def record(self, name: str, available: list) -> Optional[str]:
        """
        Register one call to `name`. Returns an injection message if hot/blocked,
        None if all is fine.
        """
        self.counts[name]   += 1
        self.lifetime[name] += 1
        n = self.counts[name]

        # Already blocked
        if name in self.blocked:
            alts = [t for t in available if t != name][:4]
            return (
                f"[aLa heat] '{name}' is BLOCKED (overheated {n} times). "
                f"Available alternatives: {', '.join(alts) or 'none'}."
            )

        # Overheat threshold — block now
        if n >= TOOL_OVER:
            self.blocked.add(name)
            alts = [t for t in available if t != name][:4]
            self._events.append({"tool": name, "count": n, "alts": alts})
            return (
                f"[aLa heat] OVERHEAT: '{name}' called {n} times this turn. "
                f"Now blocked. Try: {', '.join(alts) or 'a different approach'}."
            )

        # Hot — inject warning but don't block
        if n >= TOOL_HOT:
            alts = [t for t in available if t != name][:3]
            return (
                f"[aLa heat] '{name}' is running HOT ({n}/{TOOL_OVER}). "
                f"Consider: {', '.join(alts)}."
            )

        # Warm — log only
        if n >= TOOL_WARN:
            print(f"[aLa heat] '{name}' called {n} times this turn.", file=sys.stderr)

        return None

    def flush(self, task_context: str):
        """Save overheat events to Ferricula. Call at end of turn."""
        for ev in self._events:
            alts_str = ", ".join(ev["alts"]) or "none available"
            ferricula.tool_health(
                tool=ev["tool"],
                observation=f"overheated ({ev['count']} calls) while: {task_context[:200]}",
                suggestion=f"alternatives: {alts_str}",
            )
        self._events.clear()

    def cool_down(self):
        """Reset per-turn counts. Blocked tools stay blocked for the session."""
        self.counts.clear()

    def reset(self):
        """Full reset (new session)."""
        self.counts.clear()
        self.lifetime.clear()
        self.blocked.clear()
        self._events.clear()


# ── MCP server process wrapper ────────────────────────────────────────────────

class MCPServer:
    def __init__(self, name: str, script: str):
        self.name    = name
        self.script  = script
        self.proc    = None
        self.schemas = []
        self._lock   = threading.Lock()
        self._seq    = 0

    def start(self) -> bool:
        try:
            self.proc = subprocess.Popen(
                ["/opt/mcp-venv/bin/python3", "-u", self.script],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL, env=os.environ.copy(),
            )
            # handshake
            r = self._rpc("initialize", {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "ala", "version": "1.0"},
            })
            if not r or "error" in r:
                return False
            # discover tools
            r2 = self._rpc("tools/list", {})
            if r2 and "result" in r2:
                self.schemas = r2["result"].get("tools", [])
            return True
        except Exception as e:
            print(f"[aLa mcp] failed to start {self.name}: {e}", file=sys.stderr)
            return False

    def _rpc(self, method: str, params: dict) -> Optional[dict]:
        if not self.proc:
            return None
        with self._lock:
            self._seq += 1
            msg = json.dumps({
                "jsonrpc": "2.0", "id": self._seq,
                "method": method, "params": params,
            }) + "\n"
            try:
                self.proc.stdin.write(msg.encode())
                self.proc.stdin.flush()
                line = self.proc.stdout.readline()
                return json.loads(line.decode()) if line else None
            except Exception:
                return None

    def call(self, tool_name: str, args: dict) -> str:
        r = self._rpc("tools/call", {"name": tool_name, "arguments": args})
        if not r:
            return f"[error: no response from mcp server '{self.name}']"
        if "error" in r:
            return f"[error: {r['error']}]"
        content = r.get("result", {}).get("content", [])
        if isinstance(content, list):
            return "\n".join(c.get("text", "") for c in content if c.get("type") == "text")
        return str(r.get("result", ""))

    def stop(self):
        if self.proc:
            try:
                self.proc.terminate()
                self.proc.wait(timeout=3)
            except Exception:
                pass


class MCPManager:
    def __init__(self):
        self.servers:   list      = []
        self._tool_map: dict      = {}   # tool_name → MCPServer

    def load(self, scripts: list):
        skip = {"ala.py", "__init__.py", "sample_tool.py"}
        for name in scripts:
            if name in skip:
                continue
            path = os.path.join(MCP_DIR, name)
            if not os.path.isfile(path):
                continue
            srv = MCPServer(name.replace(".py", ""), path)
            if srv.start():
                for s in srv.schemas:
                    self._tool_map[s["name"]] = srv
                self.servers.append(srv)
                print(f"[aLa mcp] {srv.name}: {len(srv.schemas)} tools", file=sys.stderr)

    @property
    def tool_schemas(self) -> list:
        out = []
        for srv in self.servers:
            for s in srv.schemas:
                out.append({
                    "type": "function",
                    "function": {
                        "name": s["name"],
                        "description": s.get("description", ""),
                        "parameters": s.get("inputSchema",
                                            {"type": "object", "properties": {}}),
                    },
                })
        return out

    @property
    def names(self) -> list:
        return list(self._tool_map.keys())

    def call(self, name: str, args: dict) -> str:
        srv = self._tool_map.get(name)
        if not srv:
            return f"[error: tool '{name}' not found. available: {', '.join(self.names)}]"
        return srv.call(name, args)

    def shutdown(self):
        for srv in self.servers:
            srv.stop()


# ── Ollama chat ───────────────────────────────────────────────────────────────

def _cancellable_chat(messages: list, tools: list, system: str) -> Optional[dict]:
    """Run ollama_chat in a background thread; return None if _cancel fires."""
    result: list = [None]
    def _work():
        result[0] = ollama_chat(messages, tools, system)
    t = threading.Thread(target=_work, daemon=True)
    t.start()
    while t.is_alive():
        t.join(timeout=0.05)
        if _cancel.is_set():
            _clear_line()
            print(f"  {_yellow('↩')}  {_dim('cancelled')}", flush=True)
            return None
    return result[0]


def ollama_chat(messages: list, tools: list, system: str) -> dict:
    all_msgs = []
    if system:
        all_msgs.append({"role": "system", "content": system})
    all_msgs.extend(messages)

    body: dict = {"model": OLLAMA_MODEL, "messages": all_msgs, "stream": False}
    if tools:
        body["tools"] = tools

    resp = _post(f"{OLLAMA_HOST}/v1/chat/completions", body, timeout=120)
    if not resp:
        return {"error": "ollama unreachable", "content": "", "tool_calls": [], "stop_reason": "error"}

    choice = (resp.get("choices") or [{}])[0]
    msg    = choice.get("message", {})
    return {
        "content":    msg.get("content") or "",
        "tool_calls": msg.get("tool_calls") or [],
        "stop_reason": choice.get("finish_reason", "stop"),
    }


# ── aLa agent ─────────────────────────────────────────────────────────────────

BASE_SYSTEM = """\
You are aLa, a local AI agent running in a nemesis8 container.
You have access to MCP tools. Use them efficiently.
If you see a [aLa heat] warning on a tool, stop calling it and try a different approach.
If a tool is BLOCKED, do not attempt to call it again this turn.
Be concise, precise, and always make forward progress.\
"""


class AlaAgent:
    def __init__(self, mcp: MCPManager):
        self.mcp  = mcp
        self.heat = ToolHeat()

    def _system(self, memories: str, task: str) -> str:
        parts = [BASE_SYSTEM]

        # Recalled tool-health notes from past sessions
        heat_notes = ferricula.recall(f"tool overheat {task[:80]}")
        if heat_notes:
            parts.append(heat_notes)

        # General memories relevant to this task
        if memories:
            parts.append(memories)

        parts.append(f"\nAvailable MCP tools: {', '.join(self.mcp.names)}")
        return "\n\n".join(parts)

    def run(self, user_message: str) -> str:
        self.heat.cool_down()

        # Pre-turn: recall + build system prompt
        memories = ferricula.recall(user_message)
        _show_recall(memories)
        system   = self._system(memories, user_message)

        messages  = [{"role": "user", "content": user_message}]
        full_text = ""

        for turn_n in range(MAX_TURNS):
            if _cancel.is_set():
                break
            _show_thinking()
            resp = _cancellable_chat(messages, self.mcp.tool_schemas, system)
            _clear_line()

            if resp is None:  # cancelled
                break

            if resp.get("error"):
                print(f"  {_red('error:')} {resp['error']}", flush=True)
                break

            stop       = resp["stop_reason"]
            content    = resp["content"]
            tool_calls = resp["tool_calls"]

            # Clean end
            if stop in ("stop", "end_turn") or (content and not tool_calls):
                full_text = content
                break

            if tool_calls:
                messages.append({
                    "role": "assistant",
                    "content": content or "",
                    "tool_calls": tool_calls,
                })

                tool_results = []
                for tc in tool_calls:
                    fn   = tc.get("function", {})
                    name = fn.get("name", "")
                    try:
                        args = json.loads(fn.get("arguments", "{}"))
                    except Exception:
                        args = {}

                    # Heat check first
                    heat_msg = self.heat.record(name, self.mcp.names)

                    if name in self.heat.blocked:
                        _show_heat(heat_msg, blocked=True)
                        result = heat_msg
                    else:
                        _show_tool_start(name, args)
                        t_start = time.monotonic()
                        result  = self.mcp.call(name, args)
                        elapsed = time.monotonic() - t_start

                        # Detect tool failures → save tool-health memory
                        lower = result.lower()
                        is_err = any(w in lower for w in (
                            "error", "failed", "exception",
                            "not found", "permission denied", "traceback",
                        ))
                        if is_err:
                            ferricula.tool_health(
                                tool=name,
                                observation=f"returned error: {result[:200]}",
                            )

                        _show_tool_end(result, elapsed, is_err)

                        # Append heat warning to output if hot
                        if heat_msg:
                            _show_heat(heat_msg, blocked=False)
                            result = f"{heat_msg}\n\n{result}"

                    tool_results.append({
                        "role":        "tool",
                        "tool_call_id": tc.get("id", name),
                        "content":      result,
                    })

                messages.extend(tool_results)

            elif not content and not tool_calls:
                # Empty response — bail to avoid infinite loop
                break

        # Flush any overheat events to Ferricula
        self.heat.flush(user_message)

        # Post-turn: confer with Ferricula arena
        if full_text:
            arena = ferricula.confer(full_text, user_message)
            score = arena.get("score", 10)
            guidance = arena.get("guidance", "")
            if guidance:
                _show_arena(score, guidance)
            if score < 5:
                r2 = ollama_chat(
                    messages + [{
                        "role": "user",
                        "content": f"Revise your response considering: {guidance}",
                    }],
                    [],
                    system,
                )
                if r2.get("content"):
                    full_text = r2["content"]

        # Auto-save conversation to Ferricula
        if full_text:
            summary = f"User: {user_message[:300]}\naLa: {full_text[:300]}"
            ferricula.remember(summary, channel="ala", importance=0.5)
            ferricula.remember_turn("user",      user_message)
            ferricula.remember_turn("assistant", full_text)

        return full_text

    def interactive(self):
        ferricula_ok = bool(_get(f"{FERRICULA_URL}/maxid", timeout=2))
        _header(OLLAMA_MODEL, len(self.mcp.names), ferricula_ok)

        # Warm the model in background so first response isn't slow
        def _warm():
            _post(f"{OLLAMA_HOST}/v1/chat/completions",
                  {"model": OLLAMA_MODEL, "messages": [{"role": "user", "content": "hi"}],
                   "max_tokens": 1, "stream": False}, timeout=30)
        threading.Thread(target=_warm, daemon=True).start()

        _in_run = [False]

        def _sigint(sig, frame):
            if _in_run[0]:
                _cancel.set()   # cancel the turn, stay in session
            else:
                raise KeyboardInterrupt  # at prompt → exit

        signal.signal(signal.SIGINT, _sigint)

        while True:
            _cancel.clear()
            try:
                user = input(_bold("> ") if not _NO_COLOR else "> ").strip()
            except (EOFError, KeyboardInterrupt):
                print(f"\n{_dim('[aLa] Goodbye.')}")
                break
            if not user or user.lower() in ("exit", "quit", ":q"):
                break
            print()
            _in_run[0] = True
            response = self.run(user)
            _in_run[0] = False
            if response:
                print(f"\n{response}\n")


# ── Entry point ───────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(description="aLa — nemesis8 local agent")
    parser.add_argument("prompt",       nargs="?",      help="One-shot prompt (positional)")
    parser.add_argument("--prompt",     dest="pflag",   help="One-shot prompt (flag)")
    parser.add_argument("--interactive", action="store_true")
    parser.add_argument("--danger",      action="store_true", help="No confirmations")
    args = parser.parse_args()

    prompt = args.pflag or args.prompt

    # Discover available MCP tools
    scripts = []
    if os.path.isdir(MCP_DIR):
        scripts = sorted(f for f in os.listdir(MCP_DIR) if f.endswith(".py"))

    mcp = MCPManager()
    mcp.load(scripts)

    agent = AlaAgent(mcp)

    try:
        if args.interactive or not prompt:
            agent.interactive()
        else:
            print(agent.run(prompt))
    finally:
        mcp.shutdown()


if __name__ == "__main__":
    main()
