#!/usr/bin/env python3
"""
ComfyUI MCP server for the local DeepBlue Dynamics image workflow.

Exposes a narrow tool around image.json. It submits the workflow to ComfyUI,
optionally waits for completion by polling /history/{prompt_id}, and returns
the generated image metadata and /view URLs.
"""

from __future__ import annotations

from copy import deepcopy
from pathlib import Path
from typing import Any, Dict, List, Optional
from urllib import error as _urlerror
from urllib import parse as _urlparse
from urllib import request as _urlrequest
import json
import os
import random
import time
import uuid

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("comfy-image")

HERE = Path(__file__).resolve().parent
DEFAULT_WORKFLOW_PATH = HERE / "image.json"
DEFAULT_COMFYUI_BASE_URL = "http://127.0.0.1:8188"


def _base_url(server_url: Optional[str] = None) -> str:
    """Resolve the ComfyUI base URL."""
    return (server_url or os.environ.get("COMFYUI_BASE_URL") or DEFAULT_COMFYUI_BASE_URL).rstrip("/")


def _workflow_path(workflow_path: Optional[str] = None) -> Path:
    """Resolve the workflow template path."""
    if workflow_path:
        return Path(workflow_path).expanduser().resolve()
    env_path = os.environ.get("COMFY_IMAGE_WORKFLOW")
    if env_path:
        return Path(env_path).expanduser().resolve()
    return DEFAULT_WORKFLOW_PATH


def _load_workflow(workflow_path: Optional[str] = None) -> Dict[str, Any]:
    """Load a ComfyUI API workflow JSON template."""
    path = _workflow_path(workflow_path)
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)


def _request_json(
    method: str,
    url: str,
    payload: Optional[Dict[str, Any]] = None,
    timeout: int = 30,
) -> Dict[str, Any]:
    """Make a JSON HTTP request using stdlib urllib."""
    data = None
    headers = {"Accept": "application/json"}
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
        headers["Content-Type"] = "application/json"

    req = _urlrequest.Request(url, data=data, headers=headers, method=method)
    try:
        with _urlrequest.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode("utf-8")
            if resp.status < 200 or resp.status >= 300:
                return {"success": False, "error": f"HTTP {resp.status}: {raw}"}
            return json.loads(raw) if raw else {}
    except _urlerror.HTTPError as e:
        details = e.read().decode("utf-8", errors="replace")
        return {"success": False, "error": f"HTTP {e.code}: {details}"}
    except Exception as e:
        return {"success": False, "error": str(e)}


def _validate_dimension(name: str, value: int) -> Optional[str]:
    """Return an error message if a requested image dimension is invalid."""
    try:
        number = int(value)
    except Exception:
        return f"{name} must be an integer"
    if number < 256 or number > 2048:
        return f"{name} must be between 256 and 2048"
    if number % 8 != 0:
        return f"{name} must be divisible by 8"
    return None


def _build_prompt(headline_text: str, subtext: str, subject: Optional[str] = None) -> str:
    """Build the branded prompt while allowing only copy/subject changes."""
    subject_line = subject or "an abstract autonomous crawler moving through a web of connected nodes, links, and data streams"
    return f"""Visual style: dark technical, cinematic, high-contrast, cybernetic web-crawler aesthetic.
Use a near-black deep ocean/terminal background with luminous phosphor green accents.
Show {subject_line}. The scene should feel like an AI agent navigating the open web
without permission gates or platform lock-in.

Include readable text:
"{headline_text}"
"{subtext}"

Brand cues:
- DeepBlue Dynamics
- phosphor green: #00ff64
- dark background: #0a0a10
- subtle cyan/blue secondary glows
- no cute mascot, no cartoon insect, no literal food/grub worm
- no logos from other companies
- no browser UI chrome

Composition:
- Strong central visual on the right or center-right
- Text on the left with generous spacing
- Must remain legible when cropped in Twitter/X large summary cards
- Professional product launch card, not a poster cluttered with tiny text
- Add subtle scanlines, node graphs, crawler trails, and network depth"""


def _prepare_workflow(
    headline_text: str,
    subtext: str,
    subject: Optional[str],
    seed: Optional[int],
    width: Optional[int],
    height: Optional[int],
    filename_prefix: Optional[str],
    workflow_path: Optional[str],
) -> Dict[str, Any]:
    """Copy the template and inject the tool arguments into known nodes."""
    workflow = deepcopy(_load_workflow(workflow_path))

    workflow["45"]["inputs"]["text"] = _build_prompt(headline_text, subtext, subject)
    workflow["44"]["inputs"]["seed"] = int(seed if seed is not None else random.randint(1, 2**63 - 1))

    if width is not None:
        workflow["41"]["inputs"]["width"] = int(width)
    if height is not None:
        workflow["41"]["inputs"]["height"] = int(height)
    if filename_prefix:
        workflow["9"]["inputs"]["filename_prefix"] = filename_prefix

    return workflow


def _view_url(base_url: str, image: Dict[str, Any]) -> str:
    """Build a ComfyUI /view URL for an image output entry."""
    params = {
        "filename": image.get("filename", ""),
        "subfolder": image.get("subfolder", ""),
        "type": image.get("type", "output"),
    }
    return f"{base_url}/view?{_urlparse.urlencode(params)}"


def _local_output_path(image: Dict[str, Any]) -> Optional[str]:
    """Map ComfyUI image metadata to a local path if COMFYUI_OUTPUT_DIR is set."""
    output_dir = os.environ.get("COMFYUI_OUTPUT_DIR")
    filename = image.get("filename")
    if not output_dir or not filename:
        return None
    subfolder = image.get("subfolder") or ""
    return str((Path(output_dir).expanduser() / subfolder / filename).resolve())


def _extract_outputs(base_url: str, history_payload: Dict[str, Any], prompt_id: str) -> Dict[str, Any]:
    """Extract image metadata from a ComfyUI history response."""
    entry = history_payload.get(prompt_id, history_payload)
    outputs = entry.get("outputs", {}) if isinstance(entry, dict) else {}

    images: List[Dict[str, Any]] = []
    for node_id, node_output in outputs.items():
        for image in node_output.get("images", []) or []:
            item = dict(image)
            item["node_id"] = node_id
            item["view_url"] = _view_url(base_url, item)
            local_path = _local_output_path(item)
            if local_path:
                item["local_path"] = local_path
            images.append(item)

    return {
        "history": entry,
        "outputs": outputs,
        "images": images,
    }


def _history(prompt_id: str, server_url: Optional[str], timeout: int = 30) -> Dict[str, Any]:
    """Fetch one ComfyUI history entry."""
    base = _base_url(server_url)
    encoded = _urlparse.quote(prompt_id, safe="")
    return _request_json("GET", f"{base}/history/{encoded}", timeout=timeout)


def _wait_for_history(
    prompt_id: str,
    server_url: Optional[str],
    timeout_seconds: int,
    poll_interval_seconds: float,
) -> Dict[str, Any]:
    """Poll ComfyUI until the prompt appears in history or timeout expires."""
    deadline = time.monotonic() + max(1, int(timeout_seconds))
    interval = max(0.25, float(poll_interval_seconds))

    while time.monotonic() < deadline:
        result = _history(prompt_id, server_url, timeout=30)
        if result.get("success") is False:
            return result
        if prompt_id in result:
            return {"success": True, "history_payload": result}
        time.sleep(interval)

    return {
        "success": False,
        "error": f"Timed out waiting for ComfyUI prompt_id {prompt_id}",
        "prompt_id": prompt_id,
    }


@mcp.tool()
async def generate_launch_asset(
    headline_text: str,
    subtext: str,
    subject: Optional[str] = None,
    seed: Optional[int] = None,
    width: Optional[int] = None,
    height: Optional[int] = None,
    filename_prefix: str = "z-image",
    wait_for_completion: bool = True,
    timeout_seconds: int = 300,
    poll_interval_seconds: float = 1.0,
    server_url: Optional[str] = None,
    workflow_path: Optional[str] = None,
) -> Dict[str, Any]:
    """Generate a branded DeepBlue Dynamics promotional image with ComfyUI.

    Args:
        headline_text: Main text to request in the image, e.g. "GrubCrawler".
        subtext: Secondary text to request in the image.
        subject: Optional visual subject while preserving the locked brand style.
        seed: Optional fixed seed. Randomized when omitted.
        width: Optional image width override. Must be 256-2048 and divisible by 8.
        height: Optional image height override. Must be 256-2048 and divisible by 8.
        filename_prefix: ComfyUI SaveImage prefix.
        wait_for_completion: If True, poll history and return image outputs.
        timeout_seconds: Maximum seconds to wait when wait_for_completion is True.
        poll_interval_seconds: Delay between history polls.
        server_url: Optional ComfyUI base URL, default COMFYUI_BASE_URL or localhost:8188.
        workflow_path: Optional workflow template path, default sibling image.json.

    Returns:
        Dictionary with prompt_id, seed, status, and generated image metadata.
    """
    if not headline_text or not headline_text.strip():
        return {"success": False, "error": "headline_text is required"}
    if not subtext or not subtext.strip():
        return {"success": False, "error": "subtext is required"}

    if width is not None:
        error = _validate_dimension("width", width)
        if error:
            return {"success": False, "error": error}
    if height is not None:
        error = _validate_dimension("height", height)
        if error:
            return {"success": False, "error": error}

    try:
        workflow = _prepare_workflow(
            headline_text=headline_text.strip(),
            subtext=subtext.strip(),
            subject=subject.strip() if subject else None,
            seed=seed,
            width=width,
            height=height,
            filename_prefix=filename_prefix,
            workflow_path=workflow_path,
        )
    except Exception as e:
        return {"success": False, "error": f"Failed to prepare workflow: {e}"}

    base = _base_url(server_url)
    client_id = str(uuid.uuid4())
    submit_payload = {"prompt": workflow, "client_id": client_id}
    submit = _request_json("POST", f"{base}/prompt", payload=submit_payload, timeout=30)
    if submit.get("success") is False:
        return submit

    prompt_id = submit.get("prompt_id")
    if not prompt_id:
        return {"success": False, "error": "ComfyUI did not return a prompt_id", "response": submit}

    response: Dict[str, Any] = {
        "success": True,
        "status": "queued",
        "prompt_id": prompt_id,
        "client_id": client_id,
        "seed": workflow["44"]["inputs"]["seed"],
        "width": workflow["41"]["inputs"]["width"],
        "height": workflow["41"]["inputs"]["height"],
        "filename_prefix": workflow["9"]["inputs"]["filename_prefix"],
    }

    if not wait_for_completion:
        return response

    waited = _wait_for_history(prompt_id, server_url, timeout_seconds, poll_interval_seconds)
    if waited.get("success") is False:
        response.update({"success": False, "status": "timeout", "error": waited.get("error")})
        return response

    extracted = _extract_outputs(base, waited["history_payload"], prompt_id)
    response.update({
        "status": "completed",
        "images": extracted["images"],
        "outputs": extracted["outputs"],
    })
    return response


@mcp.tool()
async def comfy_generation_status(
    prompt_id: str,
    server_url: Optional[str] = None,
) -> Dict[str, Any]:
    """Check whether a ComfyUI generation has completed and return image outputs.

    Args:
        prompt_id: The prompt_id returned by generate_launch_asset.
        server_url: Optional ComfyUI base URL, default COMFYUI_BASE_URL or localhost:8188.

    Returns:
        Dictionary with status and generated image metadata when available.
    """
    if not prompt_id or not prompt_id.strip():
        return {"success": False, "error": "prompt_id is required"}

    prompt_id = prompt_id.strip()
    base = _base_url(server_url)
    result = _history(prompt_id, server_url, timeout=30)
    if result.get("success") is False:
        return result
    if prompt_id not in result:
        return {"success": True, "status": "running_or_queued", "prompt_id": prompt_id}

    extracted = _extract_outputs(base, result, prompt_id)
    return {
        "success": True,
        "status": "completed",
        "prompt_id": prompt_id,
        "images": extracted["images"],
        "outputs": extracted["outputs"],
    }


@mcp.tool()
async def comfy_image_status(
    server_url: Optional[str] = None,
    workflow_path: Optional[str] = None,
) -> Dict[str, Any]:
    """Report local configuration for this ComfyUI image MCP server.

    Args:
        server_url: Optional ComfyUI base URL override.
        workflow_path: Optional workflow template path override.

    Returns:
        Dictionary describing configured paths and server URL.
    """
    path = _workflow_path(workflow_path)
    return {
        "success": True,
        "server_url": _base_url(server_url),
        "workflow_path": str(path),
        "workflow_exists": path.exists(),
        "output_dir": os.environ.get("COMFYUI_OUTPUT_DIR"),
    }


if __name__ == "__main__":
    mcp.run()
