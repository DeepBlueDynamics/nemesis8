#!/usr/bin/env python3
"""
Terminal bridge: expose an HTTP API that injects input into a Codex TUI session.

Usage:
  python terminal_bridge.py --port 8096 --bind 0.0.0.0 -- codex resume <session>

Endpoints:
  GET  /health
  GET  /tail?bytes=65536
  POST /input   {"text": "...", "submit": false, "keys": ["ctrl+c"], "raw_b64": "..."}
  POST /prompt  {"text": "..."}  # same as /input with submit=true
  POST /key     {"key": "ctrl+c"}
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import pty
import select
import signal
import subprocess
import sys
import threading
import time
from collections import deque
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse


KEYS = {
    "enter": "\r",
    "return": "\r",
    "tab": "\t",
    "escape": "\x1b",
    "esc": "\x1b",
    "backspace": "\x7f",
    "delete": "\x1b[3~",
    "up": "\x1b[A",
    "down": "\x1b[B",
    "right": "\x1b[C",
    "left": "\x1b[D",
    "home": "\x1b[H",
    "end": "\x1b[F",
    "pageup": "\x1b[5~",
    "pagedown": "\x1b[6~",
    "ctrl+c": "\x03",
    "ctrl+d": "\x04",
    "ctrl+z": "\x1a",
    "ctrl+l": "\x0c",
    "ctrl+u": "\x15",
    "ctrl+w": "\x17",
    "ctrl+a": "\x01",
    "ctrl+e": "\x05",
    "ctrl+k": "\x0b",
    "ctrl+j": "\n",
}


class PTYRunner:
    def __init__(self, cmd: list[str], cwd: str | None = None):
        self.cmd = cmd
        self.cwd = cwd
        self.master_fd, self.slave_fd = pty.openpty()
        self.proc = subprocess.Popen(
            self.cmd,
            stdin=self.slave_fd,
            stdout=self.slave_fd,
            stderr=self.slave_fd,
            cwd=self.cwd,
            env=os.environ.copy(),
            start_new_session=True,
        )
        os.close(self.slave_fd)
        self._stop = threading.Event()
        self._buffer = deque(maxlen=2000)
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()

    def _read_loop(self) -> None:
        while not self._stop.is_set():
            try:
                readable, _, _ = select.select([self.master_fd], [], [], 0.2)
            except Exception:
                break
            if self.master_fd in readable:
                try:
                    data = os.read(self.master_fd, 4096)
                except OSError:
                    break
                if not data:
                    break
                self._buffer.append(data)
        self._stop.set()

    def send(self, data: bytes) -> None:
        if not data:
            return
        os.write(self.master_fd, data)

    def send_text(self, text: str) -> None:
        self.send(text.encode("utf-8", errors="ignore"))

    def send_key(self, key: str) -> None:
        seq = KEYS.get(key.lower())
        if seq is None:
            seq = key
        self.send_text(seq)

    def tail(self, max_bytes: int = 65536) -> str:
        data = b"".join(self._buffer)
        if len(data) > max_bytes:
            data = data[-max_bytes:]
        return data.decode("utf-8", errors="replace")

    def close(self) -> None:
        self._stop.set()
        try:
            os.close(self.master_fd)
        except OSError:
            pass
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()


class TerminalBridgeHandler(BaseHTTPRequestHandler):
    server_version = "TerminalBridge/1.0"

    def _send_json(self, status: int, payload: dict) -> None:
        data = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _read_json(self) -> dict:
        length = int(self.headers.get("Content-Length", "0") or "0")
        if length <= 0:
            return {}
        raw = self.rfile.read(length)
        try:
            return json.loads(raw.decode("utf-8"))
        except Exception:
            return {}

    def log_message(self, fmt: str, *args) -> None:
        return

    def do_GET(self) -> None:
        parsed = urlparse(self.path)
        if parsed.path == "/health":
            self._send_json(200, {"ok": True})
            return
        if parsed.path == "/tail":
            qs = parse_qs(parsed.query)
            size = int(qs.get("bytes", ["65536"])[0])
            text = self.server.runner.tail(max_bytes=size)
            self._send_json(200, {"ok": True, "tail": text})
            return
        self._send_json(404, {"ok": False, "error": "not found"})

    def do_POST(self) -> None:
        parsed = urlparse(self.path)
        payload = self._read_json()

        if parsed.path in ("/input", "/prompt"):
            text = payload.get("text", "") or ""
            keys = payload.get("keys") or []
            raw_b64 = payload.get("raw_b64")
            submit = payload.get("submit", False) or parsed.path == "/prompt"

            if raw_b64:
                try:
                    data = base64.b64decode(raw_b64.encode("utf-8"))
                    self.server.runner.send(data)
                except Exception as exc:
                    self._send_json(400, {"ok": False, "error": str(exc)})
                    return

            if text:
                self.server.runner.send_text(text)

            for key in keys:
                if key:
                    self.server.runner.send_key(str(key))

            if submit:
                self.server.runner.send_key("enter")

            self._send_json(200, {"ok": True})
            return

        if parsed.path == "/key":
            key = payload.get("key", "")
            if not key:
                self._send_json(400, {"ok": False, "error": "missing key"})
                return
            self.server.runner.send_key(str(key))
            self._send_json(200, {"ok": True})
            return

        self._send_json(404, {"ok": False, "error": "not found"})


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Codex TUI terminal bridge")
    parser.add_argument("--port", type=int, default=8096, help="HTTP port")
    parser.add_argument("--bind", default="0.0.0.0", help="Bind host")
    parser.add_argument("--cwd", default=None, help="Working directory for Codex")
    parser.add_argument("command", nargs=argparse.REMAINDER, help="Command after --")
    return parser.parse_args()


def build_command(args: argparse.Namespace) -> list[str]:
    cmd = args.command or []
    if cmd and cmd[0] == "--":
        cmd = cmd[1:]
    if not cmd:
        cmd = ["codex"]

    if os.environ.get("CODEX_UNSAFE_ALLOW_NO_SANDBOX"):
        if cmd and os.path.basename(cmd[0]) == "codex":
            if "--dangerously-bypass-approvals-and-sandbox" not in cmd:
                cmd.insert(1, "--dangerously-bypass-approvals-and-sandbox")
    return cmd


def main() -> None:
    args = parse_args()
    cmd = build_command(args)

    runner = PTYRunner(cmd, cwd=args.cwd)

    server = ThreadingHTTPServer((args.bind, args.port), TerminalBridgeHandler)
    server.runner = runner

    def shutdown(_sig=None, _frame=None):
        runner.close()
        server.shutdown()
        server.server_close()
        sys.exit(0)

    signal.signal(signal.SIGINT, shutdown)
    signal.signal(signal.SIGTERM, shutdown)

    print(f"[terminal-bridge] listening on http://{args.bind}:{args.port}", flush=True)
    print(f"[terminal-bridge] command: {' '.join(cmd)}", flush=True)

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        shutdown()


if __name__ == "__main__":
    main()
