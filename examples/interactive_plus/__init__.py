"""
codex-keyboard: Python wrapper for programmatic control of OpenAI Codex CLI

This module allows you to send keyboard input and prompts to Codex CLI
programmatically, working on both Windows and macOS/Linux.

Two approaches are provided:
1. PTY-based interactive control (for TUI mode)
2. SDK-style JSONL communication (for exec mode)
"""

from __future__ import annotations

import json
import os
import platform
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import AsyncGenerator, Callable, Iterator, Optional, Union
import asyncio
from abc import ABC, abstractmethod


# Platform detection
IS_WINDOWS = platform.system() == "Windows"


@dataclass
class CodexEvent:
    """Represents an event from Codex CLI."""
    type: str
    data: dict = field(default_factory=dict)
    raw: str = ""


@dataclass
class CodexConfig:
    """Configuration for Codex CLI."""
    codex_path: Optional[str] = None
    working_directory: Optional[str] = None
    model: Optional[str] = None
    approval_mode: str = "suggest"  # suggest, auto-edit, full-auto
    sandbox_mode: str = "workspace-write"  # read-only, workspace-write, danger-full-access
    api_key: Optional[str] = None
    extra_args: list[str] = field(default_factory=list)
    
    def find_codex(self) -> str:
        """Find the codex executable."""
        if self.codex_path and os.path.exists(self.codex_path):
            return self.codex_path
        
        # Try common locations
        codex = shutil.which("codex")
        if codex:
            return codex
        
        # Check npm global
        if IS_WINDOWS:
            npm_global = Path.home() / "AppData" / "Roaming" / "npm" / "codex.cmd"
            if npm_global.exists():
                return str(npm_global)
        else:
            npm_global = Path.home() / ".npm-global" / "bin" / "codex"
            if npm_global.exists():
                return str(npm_global)
        
        raise FileNotFoundError(
            "Codex CLI not found. Install with: npm install -g @openai/codex"
        )


class CodexController(ABC):
    """Abstract base class for Codex control."""
    
    @abstractmethod
    def start(self) -> None:
        """Start the Codex process."""
        pass
    
    @abstractmethod
    def send_input(self, text: str) -> None:
        """Send text input to Codex."""
        pass
    
    @abstractmethod
    def send_key(self, key: str) -> None:
        """Send a special key (Enter, Tab, Escape, etc.)."""
        pass
    
    @abstractmethod
    def read_output(self, timeout: float = 1.0) -> str:
        """Read available output from Codex."""
        pass
    
    @abstractmethod
    def close(self) -> None:
        """Close the Codex process."""
        pass
    
    def __enter__(self):
        self.start()
        return self
    
    def __exit__(self, exc_type, exc_val, exc_tb):
        self.close()


class CodexPTYController(CodexController):
    """
    PTY-based controller for interactive TUI mode.
    Uses pexpect (Unix) or wexpect (Windows) to control the terminal.
    """
    
    # Key mappings for special keys
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
        "ctrl+j": "\n",  # newline (for multiline input)
        "shift+enter": "\x1b[13;2u",  # Some terminals
        "alt+enter": "\x1b\r",
    }
    
    def __init__(self, config: Optional[CodexConfig] = None):
        self.config = config or CodexConfig()
        self.process = None
        self._output_buffer = ""
    
    def start(self) -> None:
        """Start Codex in interactive TUI mode."""
        codex_path = self.config.find_codex()
        
        args = [codex_path]
        
        if self.config.model:
            args.extend(["--model", self.config.model])
        if self.config.approval_mode:
            args.extend(["--approval-mode", self.config.approval_mode])
        
        args.extend(self.config.extra_args)
        
        env = os.environ.copy()
        if self.config.api_key:
            env["OPENAI_API_KEY"] = self.config.api_key
        
        cwd = self.config.working_directory or os.getcwd()
        
        if IS_WINDOWS:
            import wexpect
            self.process = wexpect.spawn(
                " ".join(args),
                cwd=cwd,
                env=env,
                encoding="utf-8"
            )
        else:
            import pexpect
            self.process = pexpect.spawn(
                args[0],
                args[1:],
                cwd=cwd,
                env=env,
                encoding="utf-8",
                dimensions=(40, 120)  # rows, cols
            )
        
        # Wait for TUI to initialize
        time.sleep(2)
    
    def send_input(self, text: str) -> None:
        """Send text input to Codex."""
        if not self.process:
            raise RuntimeError("Codex not started. Call start() first.")
        self.process.send(text)
    
    def send_key(self, key: str) -> None:
        """
        Send a special key to Codex.
        
        Examples:
            controller.send_key("enter")
            controller.send_key("ctrl+c")
            controller.send_key("escape")
        """
        if not self.process:
            raise RuntimeError("Codex not started. Call start() first.")
        
        key_lower = key.lower()
        if key_lower in self.KEYS:
            self.process.send(self.KEYS[key_lower])
        else:
            # Send as literal if not a known key
            self.process.send(key)
    
    def send_prompt(self, prompt: str, submit: bool = True) -> None:
        """
        Send a complete prompt and optionally submit it.
        
        Args:
            prompt: The prompt text to send
            submit: If True, press Enter after sending the prompt
        """
        self.send_input(prompt)
        if submit:
            time.sleep(0.1)  # Small delay for input to be processed
            self.send_key("enter")
    
    def read_output(self, timeout: float = 1.0) -> str:
        """Read available output from Codex."""
        if not self.process:
            raise RuntimeError("Codex not started. Call start() first.")
        
        try:
            if IS_WINDOWS:
                # wexpect approach
                self.process.expect(r".+", timeout=timeout)
                return self.process.after or ""
            else:
                # pexpect approach
                self.process.expect(r".+", timeout=timeout)
                return self.process.after or ""
        except Exception:
            return ""
    
    def wait_for_pattern(self, pattern: str, timeout: float = 30.0) -> bool:
        """Wait for a specific pattern in the output."""
        if not self.process:
            raise RuntimeError("Codex not started. Call start() first.")
        
        try:
            self.process.expect(pattern, timeout=timeout)
            return True
        except Exception:
            return False
    
    def approve_action(self) -> None:
        """Approve a pending action (press 'y' or Enter)."""
        self.send_key("enter")
    
    def reject_action(self) -> None:
        """Reject a pending action (press 'n')."""
        self.send_input("n")
        self.send_key("enter")
    
    def close(self) -> None:
        """Close the Codex process."""
        if self.process:
            try:
                self.send_key("ctrl+c")
                time.sleep(0.5)
                self.process.close()
            except Exception:
                pass
            self.process = None
    
    @property
    def is_alive(self) -> bool:
        """Check if the Codex process is still running."""
        if not self.process:
            return False
        return self.process.isalive()


class CodexExecController:
    """
    Controller for non-interactive exec mode.
    Uses the official --json flag to get JSONL event stream.
    
    This is the recommended approach for programmatic control as used by:
    - The official TypeScript SDK (@openai/codex-sdk)
    - codex-container's gateway (scripts/codex_gateway.js)
    
    Event types include:
    - thread.started: New thread initialized
    - turn.started, turn.completed, turn.failed: Turn lifecycle
    - item.*: Agent messages, reasoning, command executions, file changes, 
              MCP tool calls, web searches, plan updates
    - error: Non-fatal errors
    """
    
    def __init__(self, config: Optional[CodexConfig] = None):
        self.config = config or CodexConfig()
        self._last_session_id: Optional[str] = None
        self._last_thread_id: Optional[str] = None
    
    def _build_args(
        self,
        prompt: Optional[str] = None,
        session_id: Optional[str] = None,
        resume_last: bool = False,
        json_mode: bool = True
    ) -> list[str]:
        """Build command line arguments for codex exec."""
        codex_path = self.config.find_codex()
        
        args = [codex_path, "exec"]
        
        # JSON output mode - this is critical for programmatic control
        if json_mode:
            args.append("--json")
        
        # Model selection
        if self.config.model:
            args.extend(["--model", self.config.model])
        
        # Sandbox mode
        if self.config.sandbox_mode:
            args.extend(["--sandbox", self.config.sandbox_mode])
        
        # Approval/auto mode
        if self.config.approval_mode == "full-auto":
            args.append("--full-auto")
        
        # Working directory
        if self.config.working_directory:
            args.extend(["--cd", self.config.working_directory])
        
        # Extra args from config
        args.extend(self.config.extra_args)
        
        # Handle resume mode
        if resume_last:
            args.append("resume")
            args.append("--last")
            if prompt:
                args.append(prompt)
        elif session_id:
            args.append("resume")
            args.append(session_id)
            if prompt:
                args.append(prompt)
        else:
            # Normal execution - prompt goes at the end
            if prompt:
                args.append(prompt)
        
        return args
    
    def _get_env(self) -> dict:
        """Get environment variables for the subprocess."""
        env = os.environ.copy()
        if self.config.api_key:
            env["CODEX_API_KEY"] = self.config.api_key
        return env
    
    def run(
        self,
        prompt: str,
        on_event: Optional[Callable[[CodexEvent], None]] = None,
        timeout: Optional[float] = None,
        session_id: Optional[str] = None,
        resume_last: bool = False
    ) -> dict:
        """
        Run a prompt in exec mode and return the result.
        
        Args:
            prompt: The prompt to send to Codex
            on_event: Optional callback for each JSONL event received
            timeout: Optional timeout in seconds
            session_id: Optional session ID to resume
            resume_last: If True, resume the most recent session
        
        Returns:
            dict with keys:
            - success: bool
            - return_code: int
            - output: str (final agent message)
            - events: list[CodexEvent]
            - session_id: str (if available)
            - thread_id: str (if available)
        """
        args = self._build_args(prompt, session_id, resume_last)
        env = self._get_env()
        cwd = self.config.working_directory or os.getcwd()
        
        events = []
        final_output = ""
        session_id_found = None
        thread_id_found = None
        
        try:
            process = subprocess.Popen(
                args,
                cwd=cwd,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,  # Capture stderr separately
                text=True
            )
            
            # Process stdout (JSONL events)
            while True:
                line = process.stdout.readline()
                if not line and process.poll() is not None:
                    break
                
                line = line.strip()
                if not line:
                    continue
                
                try:
                    data = json.loads(line)
                    event_type = data.get("type", "unknown")
                    event = CodexEvent(type=event_type, data=data, raw=line)
                    events.append(event)
                    
                    # Extract session/thread IDs
                    if event_type == "thread.started":
                        thread_id_found = data.get("thread_id")
                    if "session_id" in data:
                        session_id_found = data.get("session_id")
                    
                    # Extract final output from agent messages
                    if event_type == "item.agent_message":
                        final_output = data.get("text", "")
                    elif data.get("item", {}).get("type") == "agent_message":
                        final_output = data.get("item", {}).get("text", "")
                    
                    if on_event:
                        on_event(event)
                        
                except json.JSONDecodeError:
                    # Non-JSON output (progress messages go to stderr)
                    event = CodexEvent(type="output", data={"text": line}, raw=line)
                    events.append(event)
                    if on_event:
                        on_event(event)
            
            # Store for potential resume
            self._last_session_id = session_id_found
            self._last_thread_id = thread_id_found
            
            return {
                "success": process.returncode == 0,
                "return_code": process.returncode,
                "output": final_output,
                "events": events,
                "session_id": session_id_found,
                "thread_id": thread_id_found,
            }
            
        except Exception as e:
            return {
                "success": False,
                "return_code": -1,
                "output": str(e),
                "events": events,
                "session_id": None,
                "thread_id": None,
            }
    
    def run_streaming(
        self,
        prompt: str,
        session_id: Optional[str] = None,
        resume_last: bool = False
    ) -> Iterator[CodexEvent]:
        """
        Run a prompt and yield JSONL events as they arrive.
        
        This is ideal for real-time UI updates or processing.
        
        Args:
            prompt: The prompt to send to Codex
            session_id: Optional session ID to resume
            resume_last: If True, resume the most recent session
        
        Yields:
            CodexEvent objects as they are received from stdout
        """
        args = self._build_args(prompt, session_id, resume_last)
        env = self._get_env()
        cwd = self.config.working_directory or os.getcwd()
        
        process = subprocess.Popen(
            args,
            cwd=cwd,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True
        )
        
        try:
            for line in iter(process.stdout.readline, ""):
                line = line.strip()
                if not line:
                    continue
                
                try:
                    data = json.loads(line)
                    event = CodexEvent(
                        type=data.get("type", "unknown"),
                        data=data,
                        raw=line
                    )
                    
                    # Track session info
                    if event.type == "thread.started":
                        self._last_thread_id = data.get("thread_id")
                    if "session_id" in data:
                        self._last_session_id = data.get("session_id")
                    
                    yield event
                    
                except json.JSONDecodeError:
                    yield CodexEvent(type="output", data={"text": line}, raw=line)
        finally:
            process.wait()
    
    def resume(
        self,
        prompt: str,
        session_id: Optional[str] = None,
        on_event: Optional[Callable[[CodexEvent], None]] = None
    ) -> dict:
        """
        Resume a previous session with a new prompt.
        
        Args:
            prompt: Follow-up prompt
            session_id: Session ID to resume (uses last session if None)
            on_event: Optional callback for events
        
        Returns:
            Same as run()
        """
        if session_id:
            return self.run(prompt, on_event=on_event, session_id=session_id)
        else:
            return self.run(prompt, on_event=on_event, resume_last=True)
    
    @property
    def last_session_id(self) -> Optional[str]:
        """Get the session ID from the last run."""
        return self._last_session_id
    
    @property 
    def last_thread_id(self) -> Optional[str]:
        """Get the thread ID from the last run."""
        return self._last_thread_id


class CodexThread:
    """
    High-level interface for managing a Codex conversation thread.
    
    Similar to the Thread class in the official TypeScript SDK.
    Maintains state across multiple turns.
    
    Example:
        thread = CodexThread()
        
        # First turn
        result = thread.run("Create a hello world script")
        print(result.output)
        
        # Continue the conversation  
        result = thread.run("Now add error handling")
        print(result.output)
    """
    
    def __init__(self, config: Optional[CodexConfig] = None):
        self.config = config or CodexConfig()
        self._controller = CodexExecController(config)
        self._session_id: Optional[str] = None
        self._thread_id: Optional[str] = None
        self._turns: list[dict] = []
    
    def run(
        self,
        prompt: str,
        on_event: Optional[Callable[[CodexEvent], None]] = None
    ) -> dict:
        """
        Run a turn in the conversation.
        
        First call starts a new session, subsequent calls resume.
        """
        if self._session_id:
            # Resume existing session
            result = self._controller.run(
                prompt,
                on_event=on_event,
                session_id=self._session_id
            )
        else:
            # Start new session
            result = self._controller.run(prompt, on_event=on_event)
            self._session_id = result.get("session_id")
            self._thread_id = result.get("thread_id")
        
        self._turns.append(result)
        return result
    
    def run_streaming(
        self,
        prompt: str
    ) -> Iterator[CodexEvent]:
        """Run a turn and stream events."""
        if self._session_id:
            yield from self._controller.run_streaming(
                prompt,
                session_id=self._session_id
            )
        else:
            for event in self._controller.run_streaming(prompt):
                # Capture IDs from first turn
                if event.type == "thread.started":
                    self._thread_id = event.data.get("thread_id")
                if "session_id" in event.data:
                    self._session_id = event.data.get("session_id")
                yield event
    
    @property
    def session_id(self) -> Optional[str]:
        return self._session_id
    
    @property
    def thread_id(self) -> Optional[str]:
        return self._thread_id
    
    @property
    def turns(self) -> list[dict]:
        return self._turns.copy()


# Convenience functions

def run_prompt(
    prompt: str,
    model: Optional[str] = None,
    full_auto: bool = False,
    working_directory: Optional[str] = None,
    on_event: Optional[Callable[[CodexEvent], None]] = None
) -> dict:
    """
    Simple function to run a prompt through Codex exec mode.
    
    Args:
        prompt: The prompt to send
        model: Optional model override
        full_auto: If True, run in full-auto mode (no approval prompts)
        working_directory: Optional working directory
        on_event: Optional callback for each JSONL event
    
    Returns:
        dict with execution results including output and events
    """
    config = CodexConfig(
        model=model,
        approval_mode="full-auto" if full_auto else "suggest",
        working_directory=working_directory
    )
    controller = CodexExecController(config)
    return controller.run(prompt, on_event=on_event)


def run_prompt_streaming(
    prompt: str,
    model: Optional[str] = None,
    full_auto: bool = False,
    working_directory: Optional[str] = None
) -> Iterator[CodexEvent]:
    """
    Run a prompt and stream events as they arrive.
    
    Args:
        prompt: The prompt to send
        model: Optional model override
        full_auto: If True, run in full-auto mode
        working_directory: Optional working directory
    
    Yields:
        CodexEvent objects in real-time
    """
    config = CodexConfig(
        model=model,
        approval_mode="full-auto" if full_auto else "suggest",
        working_directory=working_directory
    )
    controller = CodexExecController(config)
    yield from controller.run_streaming(prompt)


def interactive_session(
    model: Optional[str] = None,
    working_directory: Optional[str] = None
) -> CodexPTYController:
    """
    Create an interactive Codex session using PTY.
    
    This is for advanced use cases where you need to control
    the TUI directly. For most programmatic use, prefer
    CodexExecController or CodexThread instead.
    
    Example:
        with interactive_session() as codex:
            codex.send_prompt("Create a hello world script")
            time.sleep(5)
            codex.approve_action()
    
    Returns:
        CodexPTYController context manager
    """
    config = CodexConfig(
        model=model,
        working_directory=working_directory
    )
    return CodexPTYController(config)


def find_codex_binary() -> str:
    """Find the codex executable path."""
    config = CodexConfig()
    return config.find_codex()


# Async versions

async def run_prompt_async(
    prompt: str,
    model: Optional[str] = None,
    full_auto: bool = False,
    working_directory: Optional[str] = None,
    on_event: Optional[Callable[[CodexEvent], None]] = None
) -> dict:
    """Async version of run_prompt."""
    loop = asyncio.get_event_loop()
    return await loop.run_in_executor(
        None,
        lambda: run_prompt(prompt, model, full_auto, working_directory, on_event)
    )


async def stream_prompt_async(
    prompt: str,
    model: Optional[str] = None,
    full_auto: bool = False,
    working_directory: Optional[str] = None
) -> AsyncGenerator[CodexEvent, None]:
    """Async streaming version."""
    config = CodexConfig(
        model=model,
        approval_mode="full-auto" if full_auto else "suggest",
        working_directory=working_directory
    )
    controller = CodexExecController(config)
    
    for event in controller.run_streaming(prompt):
        yield event
        await asyncio.sleep(0)  # Yield control


__all__ = [
    # Configuration
    "CodexConfig",
    "CodexEvent",
    
    # Controllers
    "CodexController",
    "CodexExecController",
    "CodexPTYController",
    "CodexThread",
    
    # Convenience functions
    "run_prompt",
    "run_prompt_streaming",
    "interactive_session",
    "find_codex_binary",
    
    # Async functions
    "run_prompt_async",
    "stream_prompt_async",
    
    # Constants
    "IS_WINDOWS",
]
