#!/usr/bin/env python3
"""MCP: mcp-config

Manage which MCP tools are installed in the current workspace.

This tool allows discovering available MCP tools, viewing currently installed
tools, and modifying the workspace mcp_tools list in .codex-container.toml
(legacy .codex-mcp.config is supported for backward compatibility). Changes
take effect on the next container restart.
"""

from __future__ import annotations

import json
import logging
import os
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

import tomlkit

from mcp.server.fastmcp import FastMCP

# Setup logging
_default_log_root = Path(os.environ.get("CODEX_WORKSPACE_ROOT", "/workspace"))
log_dir = _default_log_root / ".mcp-logs"
log_dir.mkdir(parents=True, exist_ok=True)

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s - %(name)s - %(levelname)s - %(message)s",
    handlers=[
        logging.FileHandler(log_dir / "mcp-config.log"),
    ],
)
logger = logging.getLogger("mcp-config")

mcp = FastMCP("mcp-config")

# Paths
MCP_SOURCE = Path("/opt/mcp-installed")
MCP_DEST = Path("/opt/codex-home/mcp")
DEFAULT_CONFIG = MCP_SOURCE / ".codex-mcp.config"
DEFAULT_CONFIG_PATH = str(DEFAULT_CONFIG)

CONFIG_TOML_FILENAMES = [
    ".codex-container.toml",
    ".codex_container.toml",
]
CONFIG_JSON_FILENAMES = [
    ".codex-container.json",
    ".codex_container.json",
]


def _has_config(root: Path) -> bool:
    return any((root / name).exists() for name in CONFIG_TOML_FILENAMES + CONFIG_JSON_FILENAMES)


def _resolve_workspace_root() -> Path:
    env_root = os.environ.get("CODEX_WORKSPACE_ROOT")
    if env_root:
        candidate = Path(env_root)
        if candidate.exists():
            return candidate

    cwd = Path.cwd()
    for parent in [cwd] + list(cwd.parents):
        if _has_config(parent):
            return parent

    workspace = Path("/workspace")
    if workspace.exists():
        if _has_config(workspace):
            return workspace
        candidates = [p for p in workspace.iterdir() if p.is_dir() and _has_config(p)]
        if len(candidates) == 1:
            return candidates[0]

    return workspace


def _config_candidates(root: Path) -> List[Path]:
    candidates = [root / name for name in CONFIG_TOML_FILENAMES]
    candidates.extend(root / name for name in CONFIG_JSON_FILENAMES)
    return candidates


def _list_available_tools() -> List[str]:
    """List all MCP tools available in the image."""
    if not MCP_SOURCE.exists():
        return []

    tools = []
    for f in sorted(MCP_SOURCE.glob("*.py")):
        # Skip helper modules (prefixed with _)
        if not f.name.startswith("_"):
            tools.append(f.name)

    return tools


def _list_installed_tools() -> List[str]:
    """List currently installed MCP tools."""
    if not MCP_DEST.exists():
        return []

    tools = []
    for f in sorted(MCP_DEST.glob("*.py")):
        if not f.name.startswith("_"):
            tools.append(f.name)

    return tools


def _find_workspace_config_path() -> Path:
    workspace_root = _resolve_workspace_root()
    candidates = _config_candidates(workspace_root)
    for candidate in candidates:
        if candidate.exists():
            return candidate
    return candidates[0]


def _read_legacy_config(config_path: Path) -> List[str]:
    if not config_path.exists():
        return []

    tools = []
    try:
        for line in config_path.read_text(encoding="utf-8").splitlines():
            line = line.strip().rstrip("\r")
            if not line or line.startswith("#"):
                continue
            tools.append(line)
    except Exception as e:
        logger.error(f"Failed to read legacy config {config_path}: {e}")

    return tools


def _load_config(config_path: Path) -> Tuple[Any, str, Optional[str]]:
    if config_path.suffix == ".json":
        data = {}
        if config_path.exists():
            try:
                data = json.loads(config_path.read_text(encoding="utf-8"))
            except Exception as e:
                logger.error(f"Failed to read JSON config {config_path}: {e}")
                return {}, "json", str(e)
        return data, "json", None

    data = tomlkit.document()
    if config_path.exists():
        try:
            data = tomlkit.parse(config_path.read_text(encoding="utf-8"))
        except Exception as e:
            logger.error(f"Failed to read TOML config {config_path}: {e}")
            return tomlkit.document(), "toml", str(e)
    return data, "toml", None


def _save_config(config_path: Path, data: Any, fmt: str) -> None:
    config_path.parent.mkdir(parents=True, exist_ok=True)
    if fmt == "json":
        config_path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
        return

    config_path.write_text(tomlkit.dumps(data), encoding="utf-8")


def _resolve_workspace_tools() -> Tuple[List[str], Path, str, Optional[str]]:
    config_path = _find_workspace_config_path()
    data, fmt, parse_error = _load_config(config_path)
    legacy_config = _resolve_workspace_root() / ".codex-mcp.config"

    if "mcp_tools" in data and not parse_error:
        tools = data.get("mcp_tools") or []
        if not isinstance(tools, list):
            tools = []
        return tools, config_path, fmt, None

    legacy_tools = _read_legacy_config(legacy_config)
    if legacy_tools:
        if not parse_error:
            data["mcp_tools"] = legacy_tools
            _save_config(config_path, data, fmt)
            return legacy_tools, config_path, "migrated", None
        return legacy_tools, config_path, "legacy", parse_error

    default_tools = _read_legacy_config(DEFAULT_CONFIG)
    if default_tools:
        if not parse_error:
            data["mcp_tools"] = default_tools
            _save_config(config_path, data, fmt)
            return default_tools, config_path, "seeded", None
        return default_tools, config_path, "default", parse_error

    if parse_error:
        return [], config_path, "parse_error", parse_error
    return [], config_path, "empty", None


def _write_workspace_tools(tools: List[str]) -> Path:
    config_path = _find_workspace_config_path()
    data, fmt, parse_error = _load_config(config_path)
    if parse_error:
        raise ValueError(f"Failed to parse {config_path}; fix the file before editing.")
    data["mcp_tools"] = list(tools)
    _save_config(config_path, data, fmt)
    return config_path


@mcp.tool()
async def mcp_list_available() -> Dict[str, Any]:
    """List all MCP tools available in the Docker image.

    This shows all tools that can be installed, regardless of whether they're
    currently active in this workspace.

    Returns:
        Dictionary with list of available tool filenames.

    Example:
        {
            "success": true,
            "count": 34,
            "tools": ["time-tool.py", "calculate.py", "gnosis-crawl.py", ...]
        }
    """
    logger.info("Listing available MCP tools")

    try:
        tools = _list_available_tools()
        return {
            "success": True,
            "count": len(tools),
            "tools": tools,
            "source_path": str(MCP_SOURCE)
        }
    except Exception as e:
        logger.error(f"Failed to list available tools: {e}")
        return {"success": False, "error": str(e)}


@mcp.tool()
async def mcp_list_installed() -> Dict[str, Any]:
    """List currently installed (active) MCP tools in this workspace.

    These are the tools that were loaded at container startup based on the
    active workspace config (mcp_tools).

    Returns:
        Dictionary with list of currently installed tool filenames.

    Example:
        {
            "success": true,
            "count": 12,
            "tools": ["time-tool.py", "gnosis-crawl.py", ...],
            "config_source": "workspace" or "default"
        }
    """
    logger.info("Listing installed MCP tools")

    try:
        tools = _list_installed_tools()

        _, _, source, parse_error = _resolve_workspace_tools()
        if parse_error:
            config_source = "parse_error"
        else:
            config_source = "workspace" if source in {"toml", "json", "migrated", "seeded"} else "default"

        return {
            "success": True,
            "count": len(tools),
            "tools": tools,
            "config_source": config_source,
            "install_path": str(MCP_DEST)
        }
    except Exception as e:
        logger.error(f"Failed to list installed tools: {e}")
        return {"success": False, "error": str(e)}


@mcp.tool()
async def mcp_show_config() -> Dict[str, Any]:
    """Show the current MCP configuration for this workspace.

    Displays which config file is being used (workspace or default) and what
    tools are configured.

    Returns:
        Dictionary with configuration details and tool list.

    Example:
        {
            "success": true,
            "using_workspace_config": true,
            "config_path": "/workspace/.codex-container.toml",
            "tools": ["time-tool.py", "calculate.py", ...],
            "note": "Using workspace-specific configuration"
        }
    """
    logger.info("Showing current MCP config")

    try:
        tools, config_path, source, parse_error = _resolve_workspace_tools()
        using_workspace = source in {"toml", "json", "migrated", "seeded"} and not parse_error
        if parse_error:
            note = f"Failed to parse {config_path}. Fix the file before editing."
        elif source == "seeded":
            note = f"Seeded from image defaults. Edit {config_path} to customize."
        elif source == "migrated":
            note = f"Migrated legacy config to {config_path}."
        elif source == "empty":
            note = f"No tools configured. Add mcp_tools to {config_path}."
        else:
            note = "Using workspace-specific configuration" if using_workspace else "Using default configuration."

        return {
            "success": True,
            "using_workspace_config": using_workspace,
            "config_path": str(config_path),
            "tools": tools,
            "count": len(tools),
            "note": note,
            "parse_error": bool(parse_error),
        }
    except Exception as e:
        logger.error(f"Failed to show config: {e}")
        return {"success": False, "error": str(e)}


@mcp.tool()
async def mcp_add_tool(tool_name: str) -> Dict[str, Any]:
    """Add an MCP tool to the workspace configuration.

    Adds the specified tool to the workspace mcp_tools list in .codex-container.toml.
    If the workspace config doesn't exist, it will be created with the current default tools
    plus the new tool. Changes take effect on next container restart.

    Args:
        tool_name: Name of the tool file to add (e.g., "gnosis-crawl.py")

    Returns:
        Dictionary with success status and next steps.

    Example:
        {
            "success": true,
            "tool": "gnosis-crawl.py",
            "config_path": "/workspace/.codex-container.toml",
            "message": "Added gnosis-crawl.py to configuration. Restart container to apply changes."
        }
    """
    logger.info(f"Adding tool: {tool_name}")

    try:
        # Validate tool exists
        available = _list_available_tools()
        if tool_name not in available:
            return {
                "success": False,
                "error": f"Tool '{tool_name}' not found in available tools",
                "available_tools": available
            }

        # Resolve current tools (workspace config or migrated defaults)
        current_tools, config_path, source, parse_error = _resolve_workspace_tools()
        if parse_error:
            return {
                "success": False,
                "error": f"Failed to parse {config_path}. Fix the file before editing.",
                "config_path": str(config_path),
            }
        created_new = source in {"seeded", "migrated", "empty"}

        # Check if already present
        if tool_name in current_tools:
            return {
                "success": False,
                "error": f"Tool '{tool_name}' is already in the configuration",
                "config_path": str(config_path)
            }

        # Add tool and write config
        current_tools.append(tool_name)
        config_path = _write_workspace_tools(current_tools)

        logger.info(f"Successfully added {tool_name} to workspace config")

        return {
            "success": True,
            "tool": tool_name,
            "config_path": str(config_path),
            "created_new_config": created_new,
            "total_tools": len(current_tools),
            "message": f"Added {tool_name} to configuration. Restart container to apply changes.",
            "next_step": "Restart the container for changes to take effect"
        }
    except Exception as e:
        logger.error(f"Failed to add tool {tool_name}: {e}")
        return {"success": False, "error": str(e)}


@mcp.tool()
async def mcp_remove_tool(tool_name: str) -> Dict[str, Any]:
    """Remove an MCP tool from the workspace configuration.

    Removes the specified tool from the workspace mcp_tools list in .codex-container.toml.
    If the workspace config doesn't exist, it will be created from the default config
    with the specified tool removed. Changes take effect on next container restart.

    Args:
        tool_name: Name of the tool file to remove (e.g., "gnosis-crawl.py")

    Returns:
        Dictionary with success status and next steps.

    Example:
        {
            "success": true,
            "tool": "gnosis-crawl.py",
            "config_path": "/workspace/.codex-container.toml",
            "message": "Removed gnosis-crawl.py from configuration. Restart container to apply changes."
        }
    """
    logger.info(f"Removing tool: {tool_name}")

    try:
        # Resolve current tools (workspace config or migrated defaults)
        current_tools, config_path, source, parse_error = _resolve_workspace_tools()
        if parse_error:
            return {
                "success": False,
                "error": f"Failed to parse {config_path}. Fix the file before editing.",
                "config_path": str(config_path),
            }
        created_new = source in {"seeded", "migrated", "empty"}

        if source == "empty":
            return {
                "success": False,
                "error": "No configuration file found",
                "note": f"Add mcp_tools to {config_path} first"
            }

        # Check if present
        if tool_name not in current_tools:
            return {
                "success": False,
                "error": f"Tool '{tool_name}' is not in the configuration",
                "current_tools": current_tools
            }

        # Remove tool and write config
        current_tools.remove(tool_name)
        config_path = _write_workspace_tools(current_tools)

        logger.info(f"Successfully removed {tool_name} from workspace config")

        return {
            "success": True,
            "tool": tool_name,
            "config_path": str(config_path),
            "created_new_config": created_new,
            "remaining_tools": len(current_tools),
            "message": f"Removed {tool_name} from configuration. Restart container to apply changes.",
            "next_step": "Restart the container for changes to take effect"
        }
    except Exception as e:
        logger.error(f"Failed to remove tool {tool_name}: {e}")
        return {"success": False, "error": str(e)}


@mcp.tool()
async def mcp_set_tools(tool_names: List[str]) -> Dict[str, Any]:
    """Set the complete list of MCP tools for this workspace.

    Replaces the entire workspace mcp_tools list with the specified list
    of tools. This is useful for bulk configuration changes. Changes take
    effect on next container restart.

    Args:
        tool_names: List of tool filenames to configure (e.g., ["time-tool.py", "calculate.py"])

    Returns:
        Dictionary with success status and next steps.

    Example:
        {
            "success": true,
            "tool_count": 5,
            "tools": ["time-tool.py", "calculate.py", ...],
            "message": "Configuration updated with 5 tools. Restart container to apply changes."
        }
    """
    logger.info(f"Setting tools: {tool_names}")

    try:
        # Validate all tools exist
        available = _list_available_tools()
        invalid_tools = [t for t in tool_names if t not in available]

        if invalid_tools:
            return {
                "success": False,
                "error": f"Invalid tools: {', '.join(invalid_tools)}",
                "available_tools": available
            }

        # Write new config
        config_path = _write_workspace_tools(tool_names)

        logger.info(f"Successfully set {len(tool_names)} tools in workspace config")

        return {
            "success": True,
            "tool_count": len(tool_names),
            "tools": sorted(tool_names),
            "config_path": str(config_path),
            "message": f"Configuration updated with {len(tool_names)} tools. Restart container to apply changes.",
            "next_step": "Restart the container for changes to take effect"
        }
    except Exception as e:
        logger.error(f"Failed to set tools: {e}")
        return {"success": False, "error": str(e)}


if __name__ == "__main__":
    mcp.run()
