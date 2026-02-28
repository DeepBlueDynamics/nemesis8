#!/usr/bin/env python3
"""
Examples demonstrating codex-keyboard usage.

This shows how to programmatically control OpenAI Codex CLI using
the official --json exec mode interface.
"""

from codex_keyboard import (
    CodexConfig,
    CodexExecController,
    CodexThread,
    CodexEvent,
    run_prompt,
    run_prompt_streaming,
)


def example_simple_prompt():
    """Run a simple prompt and get the result."""
    print("=" * 60)
    print("Example 1: Simple Prompt Execution")
    print("=" * 60)
    
    result = run_prompt(
        "What is 2 + 2?",
        full_auto=True,  # Don't require approval
    )
    
    print(f"Success: {result['success']}")
    print(f"Output: {result['output']}")
    print(f"Session ID: {result.get('session_id')}")
    print()


def example_streaming():
    """Stream events in real-time."""
    print("=" * 60)
    print("Example 2: Streaming Events")
    print("=" * 60)
    
    for event in run_prompt_streaming(
        "List the files in the current directory",
        full_auto=True
    ):
        if event.type == "thread.started":
            print(f"Thread started: {event.data.get('thread_id')}")
        elif event.type == "item.agent_message":
            print(f"Agent: {event.data.get('text', '')[:100]}...")
        elif event.type == "item.command_execution":
            print(f"Running command...")
        elif event.type == "turn.completed":
            print("Turn completed!")
        else:
            print(f"Event: {event.type}")
    print()


def example_multi_turn():
    """Multi-turn conversation using CodexThread."""
    print("=" * 60)
    print("Example 3: Multi-Turn Conversation")
    print("=" * 60)
    
    thread = CodexThread(CodexConfig(approval_mode="full-auto"))
    
    # First turn
    print("Turn 1: Asking initial question...")
    result = thread.run("What programming language is this project using?")
    print(f"Response: {result['output'][:200]}...")
    
    # Second turn - continues same session
    print("\nTurn 2: Follow-up question...")
    result = thread.run("What are the main dependencies?")
    print(f"Response: {result['output'][:200]}...")
    
    print(f"\nSession maintained: {thread.session_id}")
    print(f"Total turns: {len(thread.turns)}")
    print()


def example_with_callback():
    """Use event callback for real-time processing."""
    print("=" * 60)
    print("Example 4: Event Callback")
    print("=" * 60)
    
    events_received = []
    
    def on_event(event: CodexEvent):
        events_received.append(event)
        if event.type.startswith("item."):
            print(f"  -> {event.type}")
    
    config = CodexConfig(
        approval_mode="full-auto",
        sandbox_mode="read-only",
    )
    
    controller = CodexExecController(config)
    result = controller.run(
        "What is the purpose of this project?",
        on_event=on_event
    )
    
    print(f"\nTotal events: {len(events_received)}")
    print(f"Output: {result['output'][:200]}...")
    print()


def example_session_resume():
    """Resume a previous session."""
    print("=" * 60)
    print("Example 5: Session Resume")
    print("=" * 60)
    
    config = CodexConfig(approval_mode="full-auto")
    controller = CodexExecController(config)
    
    # First run
    print("Running first prompt...")
    result1 = controller.run("Remember the number 42 for later")
    session_id = result1.get("session_id")
    print(f"Session ID: {session_id}")
    
    if session_id:
        # Resume with that session
        print("\nResuming session...")
        result2 = controller.resume(
            "What number did I tell you to remember?",
            session_id=session_id
        )
        print(f"Response: {result2['output']}")
    print()


def example_custom_config():
    """Fully customized configuration."""
    print("=" * 60)
    print("Example 6: Custom Configuration")
    print("=" * 60)
    
    config = CodexConfig(
        # Model selection
        model="gpt-4",
        
        # Full auto mode - no approval prompts
        approval_mode="full-auto",
        
        # Sandbox mode
        sandbox_mode="workspace-write",
        
        # Working directory
        working_directory=".",
        
        # Extra CLI arguments
        extra_args=["--skip-git-repo-check"],
    )
    
    controller = CodexExecController(config)
    
    result = controller.run("List files in the current directory")
    print(f"Success: {result['success']}")
    print(f"Output preview: {result['output'][:200]}...")
    print()


if __name__ == "__main__":
    print("\n" + "=" * 60)
    print("codex-keyboard Examples")
    print("=" * 60 + "\n")
    print("Note: Make sure you have Codex CLI installed and authenticated:")
    print("  npm install -g @openai/codex")
    print("  codex login\n")
    
    # Uncomment examples to run them
    # These require Codex CLI to be installed and authenticated
    
    print("To run examples, uncomment them in the main block.")
    print("Each example demonstrates a different usage pattern.\n")
    
    # example_simple_prompt()
    # example_streaming()
    # example_multi_turn()
    # example_with_callback()
    # example_session_resume()
    # example_custom_config()
