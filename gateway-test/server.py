#!/usr/bin/env python3
"""Minimal HTTP server for testing nemesis8's --publish port forwarding.
Binds 0.0.0.0:8080 so Docker's port proxy can reach it from the host."""
import http.server
import socket
import socketserver
import datetime
import os
import sys

PORT = int(os.environ.get("PORT", "8080"))

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        host = socket.gethostname()
        try:
            ip = socket.gethostbyname(host)
        except Exception:
            ip = "?"
        now = datetime.datetime.now().isoformat(timespec="seconds")
        if self.path == "/health":
            body = b'{"status":"ok"}'
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        html = f"""<!doctype html>
<html><head><meta charset="utf-8">
<title>n8 publish test</title>
<style>
 body {{ font-family: system-ui, sans-serif; max-width: 640px; margin: 3rem auto; padding: 0 1rem; }}
 h1 {{ color: #2a6; }}
 dt {{ font-weight: bold; color: #555; }}
 dd {{ margin-left: 1rem; margin-bottom: .5rem; }}
 .ok {{ color: #2a6; font-weight: bold; }}
</style></head><body>
<h1>n8 --publish test server</h1>
<p class="ok">If you can read this, nemesis8's <code>--publish</code> port forwarding works.</p>
<dl>
 <dt>Container hostname</dt><dd>{host}</dd>
 <dt>Container IP</dt><dd>{ip}</dd>
 <dt>Listening on</dt><dd>0.0.0.0:{PORT}</dd>
 <dt>Served at</dt><dd>{now}</dd>
 <dt>Request path</dt><dd>{self.path}</dd>
 <dt>PID</dt><dd>{os.getpid()}</dd>
</dl>
<p><a href="/health">/health</a> &mdash; <a href="/">reload</a></p>
</body></html>"""
        data = html.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, fmt, *args):
        ts = datetime.datetime.now().strftime("%H:%M:%S")
        sys.stderr.write(f"[{ts}] {self.address_string()} {fmt % args}\n")
        sys.stderr.flush()

class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True

if __name__ == "__main__":
    print(f"n8 publish-test server listening on 0.0.0.0:{PORT}", flush=True)
    print(f"hostname={socket.gethostname()}  pid={os.getpid()}", flush=True)
    with Server(("0.0.0.0", PORT), Handler) as httpd:
        httpd.serve_forever()
