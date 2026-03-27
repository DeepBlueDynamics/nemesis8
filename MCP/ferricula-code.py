"""ferricula-code: Index a codebase into ferricula as keystones.

Walks a directory, summarizes each significant file, and stores
each as a keystone memory in ferricula. Enables semantic code
search across all indexed projects via ferricula_recall.

Environment:
    FERRICULA_URL: ferricula instance URL (default http://localhost:8765)
    FERRICULA_TARGET: target port/name (optional)
"""

import os
import sys
import json
import urllib.request
import urllib.error
from pathlib import Path
from mcp.server.fastmcp import FastMCP

mcp = FastMCP("ferricula-code")

FERRICULA_URL = os.environ.get("FERRICULA_URL", "http://localhost:8765")
FERRICULA_TARGET = os.environ.get("FERRICULA_TARGET", None)

# File extensions worth indexing
CODE_EXTENSIONS = {
    ".py", ".rs", ".js", ".ts", ".tsx", ".jsx", ".go", ".java", ".c", ".cpp",
    ".h", ".hpp", ".rb", ".php", ".swift", ".kt", ".scala", ".sh", ".bash",
    ".toml", ".yaml", ".yml", ".json", ".sql", ".html", ".css", ".scss",
    ".vue", ".svelte", ".lua", ".zig", ".nim", ".ex", ".exs", ".erl",
    ".md", ".txt", ".dockerfile",
}

# Directories to skip
SKIP_DIRS = {
    ".git", ".hg", "node_modules", "__pycache__", ".venv", "venv",
    "target", "build", "dist", ".next", ".nuxt", "vendor", ".cache",
    ".tox", "eggs", "*.egg-info", ".mypy_cache", ".pytest_cache",
}

# Max lines to read per file for summary
MAX_LINES = 80
# Max files to index in one run
MAX_FILES = 500


def ferricula_remember(text: str, keystone: bool = True, channel: str = "seeing"):
    """Store a memory in ferricula."""
    payload = {
        "text": text,
        "channel": channel,
        "keystone": keystone,
        "importance": 0.8 if keystone else 0.3,
    }
    if FERRICULA_TARGET:
        payload["target"] = FERRICULA_TARGET

    data = json.dumps(payload).encode()
    req = urllib.request.Request(
        f"{FERRICULA_URL}/remember",
        data=data,
        method="POST",
    )
    req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return json.loads(resp.read().decode())
    except Exception as e:
        return {"error": str(e)}


def ferricula_recall(query: str, limit: int = 10):
    """Search ferricula memories."""
    payload = {"query": query}
    if FERRICULA_TARGET:
        payload["target"] = FERRICULA_TARGET

    data = json.dumps(payload).encode()
    req = urllib.request.Request(
        f"{FERRICULA_URL}/recall",
        data=data,
        method="POST",
    )
    req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return json.loads(resp.read().decode())
    except Exception as e:
        return {"error": str(e)}


def summarize_file(path: Path, project: str) -> str | None:
    """Create a summary of a file for indexing."""
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except Exception:
        return None

    lines = text.splitlines()
    if not lines:
        return None

    rel_path = str(path)
    ext = path.suffix.lower()

    # Extract key information based on file type
    imports = []
    functions = []
    classes = []
    key_lines = []

    for i, line in enumerate(lines[:MAX_LINES]):
        stripped = line.strip()

        # Python
        if ext == ".py":
            if stripped.startswith("import ") or stripped.startswith("from "):
                imports.append(stripped)
            elif stripped.startswith("def "):
                functions.append(stripped.split("(")[0].replace("def ", ""))
            elif stripped.startswith("class "):
                classes.append(stripped.split("(")[0].split(":")[0].replace("class ", ""))
            elif stripped.startswith('"""') or stripped.startswith("'''"):
                key_lines.append(stripped)

        # Rust
        elif ext == ".rs":
            if stripped.startswith("use "):
                imports.append(stripped)
            elif stripped.startswith("pub fn ") or stripped.startswith("fn "):
                sig = stripped.split("{")[0].strip()
                functions.append(sig)
            elif stripped.startswith("pub struct ") or stripped.startswith("struct "):
                classes.append(stripped.split("{")[0].strip())
            elif stripped.startswith("pub enum ") or stripped.startswith("enum "):
                classes.append(stripped.split("{")[0].strip())
            elif stripped.startswith("impl "):
                classes.append(stripped.split("{")[0].strip())

        # JavaScript/TypeScript
        elif ext in (".js", ".ts", ".tsx", ".jsx"):
            if stripped.startswith("import "):
                imports.append(stripped)
            elif "function " in stripped or stripped.startswith("export "):
                functions.append(stripped.split("{")[0].strip()[:100])
            elif stripped.startswith("class "):
                classes.append(stripped.split("{")[0].strip())

        # Go
        elif ext == ".go":
            if stripped.startswith("import"):
                imports.append(stripped)
            elif stripped.startswith("func "):
                functions.append(stripped.split("{")[0].strip())
            elif stripped.startswith("type ") and "struct" in stripped:
                classes.append(stripped.split("{")[0].strip())

        # Collect first meaningful comment/docstring
        if i < 5 and (stripped.startswith("//") or stripped.startswith("#") or stripped.startswith("/*")):
            key_lines.append(stripped)

    # Build summary
    parts = [f"[project:{project}] {rel_path}"]
    parts.append(f"  {len(lines)} lines, {ext}")

    if key_lines:
        parts.append(f"  {key_lines[0][:120]}")
    if imports:
        parts.append(f"  imports: {', '.join(imports[:8])}")
    if classes:
        parts.append(f"  types: {', '.join(classes[:10])}")
    if functions:
        parts.append(f"  functions: {', '.join(functions[:15])}")

    # Include first few non-empty, non-import lines as context
    content_lines = [
        l.strip() for l in lines[:30]
        if l.strip() and not l.strip().startswith(("import ", "from ", "use ", "#!", "//!", "/*"))
    ][:5]
    if content_lines:
        parts.append(f"  context: {' | '.join(content_lines)}")

    return "\n".join(parts)


def walk_project(directory: str) -> list[Path]:
    """Walk a project directory and return indexable files."""
    root = Path(directory)
    files = []

    for path in root.rglob("*"):
        if any(skip in path.parts for skip in SKIP_DIRS):
            continue
        if path.is_file() and path.suffix.lower() in CODE_EXTENSIONS:
            files.append(path)
        if len(files) >= MAX_FILES:
            break

    return sorted(files)


@mcp.tool()
def index_project(directory: str, project: str = "") -> str:
    """Index a codebase into ferricula as keystone memories.

    Walks the directory, summarizes each significant file, and stores
    each as a permanent keystone in ferricula. Enables semantic code
    search via ferricula_recall.

    Args:
        directory: Path to the project directory to index.
        project: Project name for tagging (defaults to directory name).
    """
    root = Path(directory)
    if not root.is_dir():
        return json.dumps({"error": f"directory not found: {directory}"})

    if not project:
        project = root.name

    files = walk_project(directory)
    indexed = 0
    errors = 0

    for path in files:
        summary = summarize_file(path, project)
        if not summary:
            continue

        result = ferricula_remember(summary, keystone=True, channel="seeing")
        if "error" in result:
            errors += 1
        else:
            indexed += 1

    return json.dumps({
        "project": project,
        "files_found": len(files),
        "indexed": indexed,
        "errors": errors,
        "search_hint": f'Use ferricula_recall("project:{project} <query>") to search',
    })


@mcp.tool()
def search_code(query: str, project: str = "") -> str:
    """Search indexed code via ferricula memory.

    Args:
        query: What to search for (semantic — describe what you need).
        project: Optional project filter.
    """
    search_query = f"project:{project} {query}" if project else query
    result = ferricula_recall(search_query)

    if isinstance(result, dict) and "error" in result:
        return json.dumps(result)

    return json.dumps(result, indent=2)


@mcp.tool()
def index_file(file_path: str, project: str = "") -> str:
    """Index a single file into ferricula.

    Args:
        file_path: Path to the file to index.
        project: Project name for tagging.
    """
    path = Path(file_path)
    if not path.is_file():
        return json.dumps({"error": f"file not found: {file_path}"})

    if not project:
        project = path.parent.name

    summary = summarize_file(path, project)
    if not summary:
        return json.dumps({"error": "could not summarize file"})

    result = ferricula_remember(summary, keystone=True, channel="seeing")
    return json.dumps({"file": str(path), "project": project, "result": result})


if __name__ == "__main__":
    mcp.run()
