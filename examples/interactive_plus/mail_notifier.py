"""
Mail notifier sidecar for codex-keyboard.

Polls the message relay for new messages and injects "you have mail"
notifications into a running Codex TUI session via PTY keystroke injection.

Architecture:
    Message Relay (port 8099)
        | (HTTP poll every N seconds)
    MailNotifier (this module)
        | (PTY injection via CodexPTYController)
    Running Codex TUI
        -> Agent sees prompt, uses relay_fetch_messages() MCP tool
"""

from __future__ import annotations

import json
import os
import platform
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from typing import Callable, Optional

IS_WINDOWS = platform.system() == "Windows"


@dataclass
class RelayConfig:
    """Configuration for the message relay connection."""

    base_url: str = ""
    project: str = "default"
    poll_interval: float = 5.0
    idle_threshold: float = 2.0  # seconds of no user input before injecting
    batch_window: float = 1.0  # seconds to batch multiple messages
    max_notification_length: int = 300  # truncate long notification text

    def __post_init__(self):
        if not self.base_url:
            self.base_url = os.environ.get(
                "RELAY_BASE_URL", "http://localhost:8099"
            )


class RelayPoller:
    """
    Polls the message relay HTTP endpoint for new messages.

    Uses only stdlib (urllib) so the core package has zero dependencies.
    """

    def __init__(self, config: Optional[RelayConfig] = None):
        self.config = config or RelayConfig()
        self._last_seen_ts: int = _now_ms()
        self._lock = threading.Lock()

    def check_health(self) -> bool:
        """Check if the relay is reachable."""
        for url in self._urls("/health"):
            try:
                req = urllib.request.Request(url, method="GET")
                with urllib.request.urlopen(req, timeout=3) as resp:
                    data = json.loads(resp.read())
                    if data.get("ok"):
                        # Lock in the working base URL.
                        self.config.base_url = url.rsplit("/health", 1)[0]
                        return True
            except Exception:
                continue
        return False

    def fetch_new(self) -> list[dict]:
        """Fetch messages newer than the last check."""
        with self._lock:
            since = self._last_seen_ts

        url = (
            f"{self.config.base_url}/messages"
            f"?project={self.config.project}&since={since}&limit=50"
        )
        try:
            req = urllib.request.Request(url, method="GET")
            with urllib.request.urlopen(req, timeout=5) as resp:
                data = json.loads(resp.read())
        except Exception:
            return []

        messages = data.get("messages", [])
        if messages:
            max_t = max(m.get("t", 0) for m in messages)
            with self._lock:
                if max_t > self._last_seen_ts:
                    self._last_seen_ts = max_t

        return messages

    def _urls(self, path: str) -> list[str]:
        """Return candidate URLs, trying configured base first then fallback."""
        base = self.config.base_url.rstrip("/")
        candidates = [f"{base}{path}"]
        # Fallback for Docker containers.
        if "localhost" in base:
            candidates.append(
                f"{base.replace('localhost', 'host.docker.internal')}{path}"
            )
        elif "host.docker.internal" in base:
            candidates.append(
                f"{base.replace('host.docker.internal', 'localhost')}{path}"
            )
        return candidates


def _now_ms() -> int:
    return int(time.time() * 1000)


def _format_notification(messages: list[dict], max_len: int = 300) -> str:
    """Format a batch of messages into a notification prompt."""
    count = len(messages)
    if count == 0:
        return ""

    parts = [f"You have {count} new message{'s' if count != 1 else ''}. "]

    # Summarize senders.
    senders = list({m.get("from", "unknown") for m in messages})
    if len(senders) <= 3:
        parts.append(f"From: {', '.join(senders)}. ")
    else:
        parts.append(f"From: {', '.join(senders[:3])}, and others. ")

    # Show subjects if present.
    subjects = [m.get("subject") for m in messages if m.get("subject")]
    if subjects:
        parts.append(f"Subjects: {'; '.join(subjects[:3])}. ")

    parts.append(
        "Use relay_fetch_messages to read and respond to them."
    )

    text = "".join(parts)
    if len(text) > max_len:
        text = text[: max_len - 3] + "..."
    return text


class MailNotifier:
    """
    Sidecar that polls the relay and injects notifications into a Codex PTY.

    Usage:
        from codex_keyboard import CodexPTYController, CodexConfig
        from codex_keyboard.mail_notifier import MailNotifier, RelayConfig

        pty = CodexPTYController(CodexConfig())
        pty.start()

        notifier = MailNotifier(pty, RelayConfig(project="my-project"))
        notifier.start()   # background thread

        # ... user interacts with TUI normally ...

        notifier.stop()
        pty.close()
    """

    def __init__(
        self,
        pty_controller,
        relay_config: Optional[RelayConfig] = None,
        on_notify: Optional[Callable[[list[dict]], None]] = None,
    ):
        """
        Args:
            pty_controller: A started CodexPTYController instance.
            relay_config: Relay connection settings.
            on_notify: Optional callback fired when notifications are injected.
                       Receives the list of new messages.
        """
        self.pty = pty_controller
        self.relay = RelayPoller(relay_config)
        self.config = self.relay.config
        self.on_notify = on_notify

        self._thread: Optional[threading.Thread] = None
        self._stop_event = threading.Event()
        self._last_inject_time: float = 0
        self._pending: list[dict] = []

    def start(self) -> None:
        """Start the background polling thread."""
        if self._thread and self._thread.is_alive():
            return

        self._stop_event.clear()
        self._thread = threading.Thread(
            target=self._poll_loop, daemon=True, name="mail-notifier"
        )
        self._thread.start()

    def stop(self) -> None:
        """Stop the background polling thread."""
        self._stop_event.set()
        if self._thread:
            self._thread.join(timeout=10)
            self._thread = None

    @property
    def is_running(self) -> bool:
        return self._thread is not None and self._thread.is_alive()

    def _poll_loop(self) -> None:
        """Main polling loop (runs in background thread)."""
        # Wait for relay to be reachable before entering loop.
        while not self._stop_event.is_set():
            if self.relay.check_health():
                break
            self._stop_event.wait(self.config.poll_interval)

        while not self._stop_event.is_set():
            try:
                new_msgs = self.relay.fetch_new()
                if new_msgs:
                    self._pending.extend(new_msgs)

                # Batch: wait a short window for more messages to arrive.
                if self._pending:
                    time.sleep(self.config.batch_window)
                    # Check again in case more arrived during the window.
                    more = self.relay.fetch_new()
                    if more:
                        self._pending.extend(more)

                    self._inject_notification()

            except Exception:
                pass  # Swallow errors — keep polling.

            self._stop_event.wait(self.config.poll_interval)

    def _inject_notification(self) -> None:
        """Inject a notification into the Codex TUI."""
        if not self._pending:
            return

        # Check the PTY is still alive.
        if not self.pty.is_alive:
            return

        messages = self._pending
        self._pending = []

        notification = _format_notification(
            messages, self.config.max_notification_length
        )
        if not notification:
            return

        # Inject: send the notification text + Enter to submit it as a prompt.
        self.pty.send_input(notification)
        time.sleep(0.1)
        self.pty.send_key("enter")

        self._last_inject_time = time.time()

        if self.on_notify:
            try:
                self.on_notify(messages)
            except Exception:
                pass
