#!/usr/bin/env python3
"""MCP: nemesis8

Self-configuration tools for the nemesis8 container. Allows agents running
INSIDE the container to add, list, and remove MCP tools — changes persist
to the host via the /opt/codex-home bind mount.

New tools are available on the NEXT session start (MCP servers are spawned
at startup, not hot-reloaded).

Deps are installed to /opt/codex-home/mcp-packages/ which is on PYTHONPATH
for all MCP tool subprocesses.
"""

import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Optional

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("nemesis8")

MCP_DIR = Path("/opt/codex-home/mcp")
PACKAGES_DIR = Path("/opt/codex-home/mcp-packages")
PIP = "/opt/mcp-venv/bin/pip"
WORKSPACE = Path(os.environ.get("NEMESIS8_WORKSPACE", "/workspace"))


def _find_config() -> Optional[Path]:
    """Walk up from workspace to find .nemesis8.toml."""
    d = WORKSPACE
    for _ in range(5):
        p = d / ".nemesis8.toml"
        if p.is_file():
            return p
        d = d.parent
    return None


def _parse_requires(content: str) -> list[str]:
    """Extract packages from '# requires: pkg1, pkg2' header lines."""
    pkgs = []
    for line in content.splitlines()[:30]:
        line = line.strip()
        if line.startswith("# requires:"):
            rest = line[len("# requires:"):].strip()
            pkgs.extend(p.strip() for p in rest.split(",") if p.strip())
    return pkgs


def _update_mcp_tools(filename: str, add: bool) -> str:
    config_path = _find_config()
    if not config_path:
        return "No .nemesis8.toml found — update mcp_tools manually."

    content = config_path.read_text()
    # Simple approach: find mcp_tools line and edit it
    lines = content.splitlines()
    for i, line in enumerate(lines):
        if line.strip().startswith("mcp_tools"):
            # parse the list
            import re
            m = re.search(r'\[([^\]]*)\]', line)
            if m:
                items_str = m.group(1)
                items = [x.strip().strip('"').strip("'") for x in items_str.split(',') if x.strip()]
                if add and filename not in items:
                    items.append(filename)
                elif not add and filename in items:
                    items.remove(filename)
                new_items = ", ".join(f'"{x}"' for x in items)
                lines[i] = re.sub(r'\[([^\]]*)\]', f'[{new_items}]', line)
                config_path.write_text('\n'.join(lines))
                return f"Updated mcp_tools in {config_path}"
    return f"mcp_tools key not found in {config_path}"


@mcp.tool()
def mcp_add(
    filename: str,
    content: str,
    requires: list = [],
) -> str:
    """Add a new MCP tool to this container. Writes the file, installs any pip
    deps, and registers it in .nemesis8.toml. Takes effect on next session start.

    Args:
        filename: Tool filename, e.g. "mytool.py". Must end in .py.
        content: Full Python source of the MCP tool.
        requires: Pip packages to install, e.g. ["requests>=2.0", "boto3"]."""
    if not filename.endswith(".py"):
        return json.dumps({"error": "filename must end in .py"})

    MCP_DIR.mkdir(parents=True, exist_ok=True)
    dest = MCP_DIR / filename

    # Parse any # requires: header from the content too
    all_deps = list(requires)
    for pkg in _parse_requires(content):
        if pkg not in all_deps:
            all_deps.append(pkg)

    dest.write_text(content)
    dest.chmod(0o644)

    results = {"file": str(dest), "deps": [], "config": ""}

    # Install deps
    if all_deps:
        PACKAGES_DIR.mkdir(parents=True, exist_ok=True)
        result = subprocess.run(
            [PIP, "install", f"--target={PACKAGES_DIR}", "--quiet"] + all_deps,
            capture_output=True, text=True
        )
        if result.returncode != 0:
            return json.dumps({"error": f"pip install failed: {result.stderr}", "file": str(dest)})
        results["deps"] = all_deps

    results["config"] = _update_mcp_tools(filename, add=True)
    results["status"] = "ok — restart session to activate"
    return json.dumps(results)


@mcp.tool()
def mcp_list() -> str:
    """List MCP tools currently installed in this container.
    Shows filename and whether it's registered in .nemesis8.toml."""
    config_path = _find_config()
    registered = set()
    if config_path:
        import re
        content = config_path.read_text()
        for line in content.splitlines():
            if "mcp_tools" in line:
                registered = set(re.findall(r'"([^"]+\.py)"', line))
                break

    tools = []
    if MCP_DIR.exists():
        for f in sorted(MCP_DIR.glob("*.py")):
            tools.append({
                "name": f.name,
                "registered": f.name in registered,
                "size": f.stat().st_size,
            })

    return json.dumps(tools, indent=2)


@mcp.tool()
def mcp_remove(filename: str) -> str:
    """Remove an MCP tool — deletes the file and deregisters it from .nemesis8.toml.

    Args:
        filename: Tool filename, e.g. "mytool.py"."""
    dest = MCP_DIR / filename
    results = {}
    if dest.is_file():
        dest.unlink()
        results["file"] = f"deleted {dest}"
    else:
        results["file"] = f"not found: {dest}"
    results["config"] = _update_mcp_tools(filename, add=False)
    return json.dumps(results)


@mcp.tool()
def mcp_install_deps(packages: list) -> str:
    """Install pip packages into the persistent MCP packages directory.
    Useful for adding deps for an existing tool.

    Args:
        packages: List of pip package specs, e.g. ["requests>=2.0", "boto3"]."""
    if not packages:
        return json.dumps({"error": "no packages specified"})
    PACKAGES_DIR.mkdir(parents=True, exist_ok=True)
    result = subprocess.run(
        [PIP, "install", f"--target={PACKAGES_DIR}"] + packages,
        capture_output=True, text=True
    )
    if result.returncode != 0:
        return json.dumps({"error": result.stderr})
    return json.dumps({"installed": packages, "target": str(PACKAGES_DIR), "output": result.stdout[-500:]})


if __name__ == "__main__":
    mcp.run()
