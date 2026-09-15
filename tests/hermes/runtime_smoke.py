"""Run inside the test image with a fresh container home; uses real entry + Hermes."""
import json
import os
from pathlib import Path
import subprocess

os.environ.update(
    NEMESIS8_PROVIDER="hermes",
    NEMESIS8_CONFIG_JSON=json.dumps({"provider": "hermes", "mcp_tools": [],
                                   "disabled_builtins": ["ask", "shivvr", "nemesis8"]}),
    NEMESIS8_WORKSPACE="/workspace",
    HERMES_HOME="/opt/nemesis8/.hermes",
    CODEX_DEFAULT_MODEL=os.environ.get("HERMES_SMOKE_MODEL", "ollama/gemma4:e2b"),
)
result = subprocess.run(
    ["nemesis8-entry", "--prompt",
     "Call nemesis8_gateway_status once to read the gateway status. "
     "Then reply with exactly HERMES_N8_OK. Do not spawn or stop agents."],
    text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=180,
)
print(result.stdout, flush=True)
print("ENTRY_EXIT=" + str(result.returncode), flush=True)

import yaml
config = yaml.safe_load(Path("/opt/nemesis8/.hermes/config.yaml").read_text())
assert "nemesis8" in config["plugins"]["enabled"]
assert "nuts-files" in config["mcp_servers"], config["mcp_servers"].keys()
assert Path("/opt/nemesis8/.hermes/plugins/nemesis8/plugin.yaml").is_file()
assert "Nemesis 8" in Path("/opt/nemesis8/.hermes/SOUL.md").read_text()
from hermes_cli.plugins import get_plugin_manager
from tools.registry import registry
manager = get_plugin_manager()
manager.discover_and_load()
loaded = [p for p in manager.list_plugins() if p["name"] == "nemesis8"]
assert len(loaded) == 1 and loaded[0]["enabled"] and not loaded[0]["error"], loaded
print("NEMESIS8_PLUGIN=" + json.dumps(loaded[0], default=str), flush=True)
names = {"nemesis8_gateway_status", "nemesis8_agent_list", "nemesis8_agent_spawn",
         "nemesis8_agent_stop", "nemesis8_agent_kill"}
definitions = registry.get_definitions(names, quiet=True)
assert len(definitions) == 5, definitions
print("NATIVE_TOOL_DEFINITIONS=5", flush=True)
import model_tools  # Discover the built-in skill_view handler.
skill = registry.get_entry("skill_view").handler({"name": "nemesis8:nemesis8-fleet"})
assert "Nemesis8 Fleet Orchestration Skill" in skill, skill
print("NATIVE_FLEET_SKILL_VERIFIED", flush=True)
entry = registry.get_entry("nemesis8_gateway_status")
print("GATEWAY_STATUS=" + entry.handler({}), flush=True)
assert result.returncode == 0, "Hermes entry failed"
import sqlite3
with sqlite3.connect("/opt/nemesis8/.hermes/state.db") as db:
    rows = db.execute("SELECT tool_calls, timestamp FROM messages WHERE tool_calls IS NOT NULL").fetchall()
    calls = [call for raw, _ in rows for call in json.loads(raw or "[]")]
    print("SESSION_TOOL_NAMES=" + json.dumps([call.get("function", call).get("name") for call in calls]), flush=True)
    print("SESSION_TIMESTAMP_SAMPLE=" + str([stamp for _, stamp in rows[:1]]), flush=True)
    invoked = []
    for call in calls:
        fn = call.get("function", call)
        invoked.append(fn.get("name"))
        if fn.get("name") == "tool_call":
            args = json.loads(fn.get("arguments") or "{}")
            invoked.extend(item.get("name") for item in args.get("calls", []))
    results = db.execute("SELECT content FROM messages WHERE role='tool'").fetchall()
    print("MODEL_TOOL_RESULTS=" + json.dumps(results), flush=True)
    assert "nemesis8_gateway_status" in invoked, invoked
    assert any("uptime_secs" in (text or "") for (text,) in results), results
    print("MODEL_NATIVE_TOOL_CALL_VERIFIED", flush=True)
print("HERMES_RUNTIME_SMOKE_PASS", flush=True)
