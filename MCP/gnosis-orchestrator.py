#!/usr/bin/env python3
"""MCP: gnosis-orchestrator

Unified orchestration tool for Gnosis system management.

This server consolidates the following tool families behind a single action
interface:
- config management
- container/system restart
- host service engine controls
- monitor scheduler controls
- session discovery/search
- time utilities
- task planning/instructor

Usage:
- Call `gnosis_orchestrator(action="help")` for full docs.
- Call `gnosis_orchestrator(action="<domain.action>", args={...})` to execute.

This wrapper intentionally returns recovery-focused errors with:
- likely causes
- "try instead" alternatives
- concrete next steps
"""

from __future__ import annotations

import asyncio
import importlib.util
import inspect
import json
import sys
import types
import tomllib
from dataclasses import dataclass
from difflib import get_close_matches
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

try:
    from mcp.server.fastmcp import FastMCP, Context  # type: ignore
except Exception:
    class Context:  # pragma: no cover - local compatibility shim
        pass

    class FastMCP:  # pragma: no cover - local compatibility shim
        def __init__(self, _name: str):
            self.name = _name

        def tool(self, *args: Any, **kwargs: Any):
            def _decorator(fn: Any) -> Any:
                return fn

            return _decorator

        def run(self, *args: Any, **kwargs: Any) -> None:
            raise RuntimeError(
                "FastMCP runtime is not available. Install the MCP runtime package to run as an MCP server."
            )

    # Ensure dynamically imported MCP modules can resolve this import path too:
    # from mcp.server.fastmcp import FastMCP
    mcp_pkg = sys.modules.get("mcp") or types.ModuleType("mcp")
    server_pkg = sys.modules.get("mcp.server") or types.ModuleType("mcp.server")
    fastmcp_mod = sys.modules.get("mcp.server.fastmcp") or types.ModuleType("mcp.server.fastmcp")
    fastmcp_mod.FastMCP = FastMCP  # type: ignore[attr-defined]
    fastmcp_mod.Context = Context  # type: ignore[attr-defined]
    server_pkg.fastmcp = fastmcp_mod  # type: ignore[attr-defined]
    mcp_pkg.server = server_pkg  # type: ignore[attr-defined]
    sys.modules["mcp"] = mcp_pkg
    sys.modules["mcp.server"] = server_pkg
    sys.modules["mcp.server.fastmcp"] = fastmcp_mod


def _toml_escape(value: str) -> str:
    escaped = value.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def _toml_value(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, str):
        return _toml_escape(value)
    if isinstance(value, list):
        return "[" + ", ".join(_toml_value(v) for v in value) + "]"
    if value is None:
        return '""'
    return _toml_escape(str(value))


def _toml_dumps_simple(data: Dict[str, Any]) -> str:
    lines: List[str] = []
    table_items: List[Tuple[str, Dict[str, Any]]] = []
    for key, value in data.items():
        if isinstance(value, dict):
            table_items.append((key, value))
        else:
            lines.append(f"{key} = {_toml_value(value)}")
    for table_name, table in table_items:
        lines.append("")
        lines.append(f"[{table_name}]")
        for k, v in table.items():
            lines.append(f"{k} = {_toml_value(v)}")
    return "\n".join(lines).rstrip() + "\n"


try:
    import tomlkit  # type: ignore
except Exception:
    tomlkit = types.ModuleType("tomlkit")  # type: ignore[assignment]

    def _document() -> Dict[str, Any]:
        return {}

    def _parse(text: str) -> Dict[str, Any]:
        return tomllib.loads(text)

    def _dumps(data: Dict[str, Any]) -> str:
        return _toml_dumps_simple(data)

    tomlkit.document = _document  # type: ignore[attr-defined]
    tomlkit.parse = _parse  # type: ignore[attr-defined]
    tomlkit.dumps = _dumps  # type: ignore[attr-defined]
    sys.modules["tomlkit"] = tomlkit  # type: ignore[arg-type]


mcp = FastMCP("gnosis-orchestrator")


@dataclass(frozen=True)
class ActionSpec:
    action: str
    module_file: str
    function_name: str
    summary: str
    aliases: Tuple[str, ...] = ()


ACTION_SPECS: List[ActionSpec] = [
    # Config
    ActionSpec("config.list_available", "mcp-config.py", "mcp_list_available", "List all available MCP tools.", ("config.available",)),
    ActionSpec("config.list_installed", "mcp-config.py", "mcp_list_installed", "List currently installed MCP tools.", ("config.installed",)),
    ActionSpec("config.show", "mcp-config.py", "mcp_show_config", "Show active workspace MCP configuration.", ("config.current_config", "config.current")),
    ActionSpec("config.add_tool", "mcp-config.py", "mcp_add_tool", "Add a tool file to workspace mcp_tools."),
    ActionSpec("config.remove_tool", "mcp-config.py", "mcp_remove_tool", "Remove a tool file from workspace mcp_tools."),
    ActionSpec("config.set_tools", "mcp-config.py", "mcp_set_tools", "Replace workspace mcp_tools list."),
    # Service engine
    ActionSpec("services.health", "service-engine.py", "service_engine_health", "Check service-engine health."),
    ActionSpec("services.list", "service-engine.py", "service_engine_services", "List services from service-engine."),
    ActionSpec("services.start", "service-engine.py", "service_engine_start", "Start a named service."),
    ActionSpec("services.stop", "service-engine.py", "service_engine_stop", "Stop a named service."),
    ActionSpec("services.restart", "service-engine.py", "service_engine_restart", "Restart a named service."),
    ActionSpec("services.logs", "service-engine.py", "service_engine_logs", "Fetch logs for a named service."),
    # Scheduler
    ActionSpec("scheduler.list", "monitor-scheduler.py", "list_triggers", "List monitor triggers for a session."),
    ActionSpec("scheduler.get", "monitor-scheduler.py", "get_trigger", "Get one trigger by id."),
    ActionSpec("scheduler.create", "monitor-scheduler.py", "create_trigger", "Create a monitor trigger."),
    ActionSpec("scheduler.update", "monitor-scheduler.py", "update_trigger", "Update trigger fields."),
    ActionSpec("scheduler.toggle", "monitor-scheduler.py", "toggle_trigger", "Enable/disable a trigger."),
    ActionSpec("scheduler.delete", "monitor-scheduler.py", "delete_trigger", "Delete a trigger."),
    ActionSpec("scheduler.record_fire_result", "monitor-scheduler.py", "record_fire_result", "Set last_fired for a trigger."),
    ActionSpec("scheduler.clock_now", "monitor-scheduler.py", "clock_now", "Current time from scheduler utility."),
    ActionSpec("scheduler.clock_add", "monitor-scheduler.py", "clock_add", "Add duration to timestamp."),
    # Sessions
    ActionSpec("sessions.list", "session-tools.py", "session_list", "List sessions with optional query filter."),
    ActionSpec("sessions.detail", "session-tools.py", "session_detail", "Get details for one session id."),
    ActionSpec("sessions.search", "session-tools.py", "session_search", "Search sessions by query."),
    # Time
    ActionSpec("time.now", "time-tool.py", "time_now", "Current time by timezone/location."),
    ActionSpec("time.convert", "time-tool.py", "time_convert", "Convert datetime between timezones."),
    ActionSpec("time.list_timezones", "time-tool.py", "time_list_timezones", "List/filter IANA timezones."),
    # Planning
    ActionSpec("planning.next_step", "task-instructor.py", "get_next_step", "Get next executable task step.", ("planning.instructor",)),
]


ACTION_INDEX: Dict[str, ActionSpec] = {spec.action: spec for spec in ACTION_SPECS}
for spec in ACTION_SPECS:
    for alias in spec.aliases:
        ACTION_INDEX[alias] = spec

CANONICAL_ACTIONS = sorted({spec.action for spec in ACTION_SPECS})

DOMAIN_TIPS: Dict[str, List[str]] = {
    "config": [
        "Use config.show before edits to confirm current tool state.",
        "Changes to mcp_tools apply after container restart.",
    ],
    "services": [
        "Verify service-engine availability with services.health first.",
        "If host bridge is unavailable, check SERVICE_ENGINE_URL.",
    ],
    "scheduler": [
        "Use scheduler.list to verify trigger ids before updates/deletes.",
        "For updates, pass updates_json (string) or updates (object).",
    ],
    "sessions": [
        "Use sessions.list for broad scan, sessions.search for focused lookup.",
        "Keep query terms short for best fuzzy match behavior.",
    ],
    "time": [
        "If timezone is invalid, call time.list_timezones with a query fragment.",
        "Use RFC3339/ISO timestamps for best parsing reliability.",
    ],
    "planning": [
        "Provide completed_steps to keep planning stateful across calls.",
        "If planning lacks tools, pass available_tools explicitly.",
    ],
}

MODULE_CACHE: Dict[str, Any] = {}


def _module_candidates(filename: str) -> List[Path]:
    here = Path(__file__).resolve().parent
    cwd = Path.cwd()
    return [
        here / filename,
        cwd / "MCP" / filename,
        Path("/workspace/codex-container/MCP") / filename,
    ]


def _load_module(filename: str) -> Any:
    if filename in MODULE_CACHE:
        return MODULE_CACHE[filename]

    selected: Optional[Path] = None
    for candidate in _module_candidates(filename):
        if candidate.exists():
            selected = candidate
            break
    if not selected:
        raise FileNotFoundError(f"Could not locate module file: {filename}")

    module_name = f"gnosis_orch_{filename.replace('-', '_').replace('.', '_')}"
    spec = importlib.util.spec_from_file_location(module_name, str(selected))
    if spec is None or spec.loader is None:
        raise RuntimeError(f"Could not create module spec for {selected}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    try:
        spec.loader.exec_module(module)
    except Exception:
        sys.modules.pop(module_name, None)
        raise
    MODULE_CACHE[filename] = module
    return module


def _norm_action(action: str) -> str:
    return (action or "").strip().lower()


def _action_domain(action: str) -> str:
    if "." in action:
        return action.split(".", 1)[0]
    return action


def _list_domain_actions(domain: str) -> List[str]:
    prefix = f"{domain}."
    return [a for a in CANONICAL_ACTIONS if a.startswith(prefix)]


def _format_signature(fn: Any) -> str:
    sig = inspect.signature(fn)
    return f"{fn.__name__}{sig}"


def _required_optional_params(fn: Any) -> Tuple[List[str], List[str]]:
    required: List[str] = []
    optional: List[str] = []
    sig = inspect.signature(fn)
    for name, param in sig.parameters.items():
        if name == "ctx":
            continue
        if param.kind in (inspect.Parameter.VAR_POSITIONAL, inspect.Parameter.VAR_KEYWORD):
            continue
        if param.default is inspect._empty:
            required.append(name)
        else:
            optional.append(name)
    return required, optional


def _positive_error(
    *,
    action: str,
    error_code: str,
    message: str,
    likely_cause: str,
    try_instead: List[str],
    next_steps: List[str],
    did_you_mean: Optional[List[str]] = None,
    example_fix: Optional[str] = None,
    details: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    payload: Dict[str, Any] = {
        "success": False,
        "action": action,
        "error_code": error_code,
        "what_happened": message,
        "likely_cause": likely_cause,
        "try_instead": try_instead,
        "next_steps": next_steps,
    }
    if did_you_mean:
        payload["did_you_mean"] = did_you_mean
    if example_fix:
        payload["example_fix"] = example_fix
    if details:
        payload["details"] = details
    return payload


def _response_tips(action: str) -> List[str]:
    domain = _action_domain(action)
    return DOMAIN_TIPS.get(domain, [])


def _common_alternatives(action: str) -> List[str]:
    domain = _action_domain(action)
    if domain == "config":
        return ["config.show", "config.list_available", "config.list_installed"]
    if domain == "services":
        return ["services.health", "services.list"]
    if domain == "scheduler":
        return ["scheduler.list", "scheduler.get"]
    if domain == "sessions":
        return ["sessions.list", "sessions.search"]
    if domain == "time":
        return ["time.now", "time.list_timezones"]
    if domain == "planning":
        return ["planning.next_step"]
    return ["help"]


def _decode_args(args: Optional[Dict[str, Any]], args_json: Optional[str]) -> Tuple[Dict[str, Any], Optional[str]]:
    merged: Dict[str, Any] = {}
    if args:
        merged.update(args)
    if args_json:
        try:
            decoded = json.loads(args_json)
        except json.JSONDecodeError as exc:
            return {}, f"args_json is not valid JSON: {exc}"
        if not isinstance(decoded, dict):
            return {}, "args_json must decode to a JSON object"
        merged.update(decoded)
    return merged, None


def _normalize_scheduler_update_args(action: str, payload: Dict[str, Any]) -> Dict[str, Any]:
    if action != "scheduler.update":
        return payload
    updated = dict(payload)
    if "updates_json" not in updated and "updates" in updated:
        updates_obj = updated.pop("updates")
        updated["updates_json"] = json.dumps(updates_obj)
    return updated


async def _call_action(spec: ActionSpec, payload: Dict[str, Any]) -> Dict[str, Any]:
    module = _load_module(spec.module_file)
    fn = getattr(module, spec.function_name, None)
    if fn is None:
        raise AttributeError(f"{spec.function_name} not found in {spec.module_file}")

    required, optional = _required_optional_params(fn)
    allowed = set(required + optional)

    missing = [k for k in required if k not in payload]
    if missing:
        raise ValueError(f"Missing required args: {', '.join(missing)}")

    unknown = [k for k in payload.keys() if k not in allowed]
    if unknown:
        raise TypeError(f"Unknown args for {spec.action}: {', '.join(sorted(unknown))}")

    result = fn(**payload)
    if inspect.isawaitable(result):
        result = await result
    if isinstance(result, dict):
        return result
    return {"success": True, "result": result}


def _help_for_action(action: str) -> Dict[str, Any]:
    canonical = ACTION_INDEX.get(action)
    if not canonical:
        matches = get_close_matches(action, sorted(ACTION_INDEX.keys()), n=5, cutoff=0.45)
        return _positive_error(
            action="help",
            error_code="unknown_help_topic",
            message=f"Unknown help topic: {action}",
            likely_cause="Topic does not match a known action or domain.",
            try_instead=["help", "help.config", "help.scheduler"],
            next_steps=["Use help with no topic to list all actions.", "Pick one action from the list and rerun help.<action>."],
            did_you_mean=matches or None,
        )

    module = _load_module(canonical.module_file)
    fn = getattr(module, canonical.function_name, None)
    if fn is None:
        return _positive_error(
            action=canonical.action,
            error_code="help_target_missing",
            message=f"Function {canonical.function_name} was not found.",
            likely_cause="Module version mismatch or missing source file.",
            try_instead=_common_alternatives(canonical.action),
            next_steps=["Verify the MCP source files exist in /workspace/codex-container/MCP.", "Rerun help after restoring the file."],
        )

    required, optional = _required_optional_params(fn)
    doc = inspect.getdoc(fn) or "(no docstring)"
    return {
        "success": True,
        "action": "help",
        "topic": canonical.action,
        "summary": canonical.summary,
        "target": {"module": canonical.module_file, "function": canonical.function_name},
        "signature": _format_signature(fn),
        "required_args": required,
        "optional_args": optional,
        "tips": _response_tips(canonical.action),
        "doc": doc,
    }


def _help_for_domain(domain: str) -> Dict[str, Any]:
    actions = _list_domain_actions(domain)
    if not actions:
        return _positive_error(
            action="help",
            error_code="unknown_domain",
            message=f"Unknown domain: {domain}",
            likely_cause="Domain prefix is not part of this orchestrator.",
            try_instead=["help", "help.config", "help.services", "help.scheduler", "help.sessions", "help.time", "help.planning"],
            next_steps=["Call help with no topic to list valid domains and actions."],
        )
    return {
        "success": True,
        "action": "help",
        "topic": domain,
        "actions": actions,
        "tips": DOMAIN_TIPS.get(domain, []),
    }


def _help_overview() -> Dict[str, Any]:
    domains = sorted({a.split(".", 1)[0] for a in CANONICAL_ACTIONS})
    examples = [
        {"action": "config.show", "args": {}},
        {"action": "services.health", "args": {}},
        {"action": "sessions.search", "args": {"query": "memex"}},
        {"action": "time.now", "args": {"timezone_name": "America/New_York"}},
        {"action": "planning.next_step", "args": {"task": "Ship release notes draft"}},
    ]
    return {
        "success": True,
        "action": "help",
        "overview": "Unified Gnosis operations tool. Execute actions through one endpoint.",
        "domains": domains,
        "actions": CANONICAL_ACTIONS,
        "examples": examples,
        "next_step": "Call help.<domain> or help.<action> for exact argument docs.",
    }


def _resolve_help_topic(action: str, payload: Dict[str, Any]) -> Optional[str]:
    if action == "help":
        topic = payload.get("topic")
        if isinstance(topic, str) and topic.strip():
            return topic.strip().lower()
        return None
    if action.startswith("help."):
        return action.split("help.", 1)[1].strip().lower()
    return None


@mcp.tool()
async def gnosis_orchestrator(
    action: str,
    args: Optional[Dict[str, Any]] = None,
    args_json: Optional[str] = None,
) -> Dict[str, Any]:
    """Execute a unified Gnosis management action.

    Args:
        action: Action name like "config.show", "services.health", "scheduler.list",
            "sessions.search", "time.now", or "planning.next_step". Use "help" for docs.
        args: Optional object of arguments for the selected action.
        args_json: Optional JSON string with action arguments.

    Returns:
        Structured result from the target capability. Errors include recovery guidance.
    """

    normalized = _norm_action(action)
    payload, decode_error = _decode_args(args, args_json)
    if decode_error:
        return _positive_error(
            action=normalized or "(empty)",
            error_code="invalid_args_json",
            message=decode_error,
            likely_cause="Malformed JSON or non-object payload.",
            try_instead=["Pass args as object", "Use help for expected argument names"],
            next_steps=["Validate your JSON syntax.", "Retry with args_json='{\"key\":\"value\"}' or args={...}."],
            example_fix='{"action":"time.now","args":{"timezone_name":"UTC"}}',
        )

    help_topic = _resolve_help_topic(normalized, payload)
    if normalized == "help" or normalized.startswith("help."):
        if not help_topic:
            return _help_overview()
        if help_topic in {"config", "services", "scheduler", "sessions", "time", "planning"}:
            return _help_for_domain(help_topic)
        return _help_for_action(help_topic)

    spec = ACTION_INDEX.get(normalized)
    if spec is None:
        matches = get_close_matches(normalized, sorted(ACTION_INDEX.keys()) + ["help"], n=5, cutoff=0.45)
        return _positive_error(
            action=normalized,
            error_code="unknown_action",
            message=f"Unknown action: {normalized}",
            likely_cause="Action name typo or unsupported capability.",
            try_instead=["help", "help.config", "help.scheduler"],
            next_steps=["Pick a valid action from help output.", "Rerun with action='<domain.action>'."],
            did_you_mean=matches or None,
        )

    payload = _normalize_scheduler_update_args(spec.action, payload)

    try:
        raw = await _call_action(spec, payload)
    except ValueError as exc:
        module = _load_module(spec.module_file)
        fn = getattr(module, spec.function_name)
        required, optional = _required_optional_params(fn)
        return _positive_error(
            action=spec.action,
            error_code="missing_required_args",
            message=str(exc),
            likely_cause="One or more required parameters were not supplied.",
            try_instead=[f"help.{spec.action}", f"help.{_action_domain(spec.action)}"],
            next_steps=["Add all required args and retry.", "If unsure, copy the signature from help output."],
            example_fix=f"{spec.action} requires: {required}; optional: {optional}",
        )
    except TypeError as exc:
        module = _load_module(spec.module_file)
        fn = getattr(module, spec.function_name)
        required, optional = _required_optional_params(fn)
        return _positive_error(
            action=spec.action,
            error_code="invalid_arguments",
            message=str(exc),
            likely_cause="Unexpected argument names or type mismatch.",
            try_instead=[f"help.{spec.action}"],
            next_steps=["Rename/remove unknown args.", "Retry with only required and optional args."],
            details={"required_args": required, "optional_args": optional},
        )
    except FileNotFoundError as exc:
        return _positive_error(
            action=spec.action,
            error_code="module_not_found",
            message=str(exc),
            likely_cause="Source module not present in expected MCP directories.",
            try_instead=_common_alternatives(spec.action),
            next_steps=["Confirm file exists under /workspace/codex-container/MCP.", "Restart container after restoring files."],
        )
    except Exception as exc:
        return _positive_error(
            action=spec.action,
            error_code="execution_exception",
            message=str(exc),
            likely_cause="Unexpected runtime exception in target action.",
            try_instead=_common_alternatives(spec.action),
            next_steps=["Check service dependencies and environment vars.", f"Use help.{spec.action} and retry with minimal args."],
        )

    if isinstance(raw, dict) and raw.get("success") is False:
        err = str(raw.get("error") or raw.get("what_happened") or "Action failed")
        return _positive_error(
            action=spec.action,
            error_code="action_failed",
            message=err,
            likely_cause="Target module returned an operational failure.",
            try_instead=_common_alternatives(spec.action),
            next_steps=["Review error text.", "Run corresponding help action for expected inputs.", "Retry with adjusted args."],
            details={"raw_result": raw, "tips": _response_tips(spec.action)},
        )

    if not isinstance(raw, dict):
        raw = {"success": True, "result": raw}
    raw.setdefault("success", True)
    raw.setdefault("action", spec.action)
    raw.setdefault("tips", _response_tips(spec.action))
    return raw


if __name__ == "__main__":
    mcp.run()
