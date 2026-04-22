#!/usr/bin/env python3
"""
ComfyUI MCP server for the local DeepBlue Dynamics image workflow.

Exposes a narrow tool around an embedded ComfyUI workflow. It submits the
workflow to ComfyUI,
optionally waits for completion by polling /history/{prompt_id}, and returns
the generated image metadata, /view URLs, and optional downloaded files.
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
DEFAULT_COMFYUI_BASE_URL = "http://host.docker.internal:8000"
DEFAULT_DOWNLOAD_DIR = HERE / "outputs"

EMBEDDED_WORKFLOW_JSON = r"""
{
  "9": {
    "inputs": {
      "filename_prefix": "z-image",
      "images": [
        "43",
        0
      ]
    },
    "class_type": "SaveImage",
    "_meta": {
      "title": "Save Image"
    }
  },
  "39": {
    "inputs": {
      "clip_name": "qwen_3_4b.safetensors",
      "type": "lumina2",
      "device": "default"
    },
    "class_type": "CLIPLoader",
    "_meta": {
      "title": "Load CLIP"
    }
  },
  "40": {
    "inputs": {
      "vae_name": "ae.safetensors"
    },
    "class_type": "VAELoader",
    "_meta": {
      "title": "Load VAE"
    }
  },
  "41": {
    "inputs": {
      "width": 1024,
      "height": 1024,
      "batch_size": 1
    },
    "class_type": "EmptySD3LatentImage",
    "_meta": {
      "title": "EmptySD3LatentImage"
    }
  },
  "42": {
    "inputs": {
      "conditioning": [
        "45",
        0
      ]
    },
    "class_type": "ConditioningZeroOut",
    "_meta": {
      "title": "ConditioningZeroOut"
    }
  },
  "43": {
    "inputs": {
      "samples": [
        "44",
        0
      ],
      "vae": [
        "40",
        0
      ]
    },
    "class_type": "VAEDecode",
    "_meta": {
      "title": "VAE Decode"
    }
  },
  "44": {
    "inputs": {
      "seed": 717346662507128,
      "steps": 9,
      "cfg": 1,
      "sampler_name": "res_multistep",
      "scheduler": "simple",
      "denoise": 1,
      "model": [
        "47",
        0
      ],
      "positive": [
        "45",
        0
      ],
      "negative": [
        "42",
        0
      ],
      "latent_image": [
        "41",
        0
      ]
    },
    "class_type": "KSampler",
    "_meta": {
      "title": "KSampler"
    }
  },
  "45": {
    "inputs": {
      "text": "",
      "clip": [
        "39",
        0
      ]
    },
    "class_type": "CLIPTextEncode",
    "_meta": {
      "title": "CLIP Text Encode (Prompt)"
    }
  },
  "46": {
    "inputs": {
      "unet_name": "z_image_turbo-Q6_K.gguf"
    },
    "class_type": "UnetLoaderGGUF",
    "_meta": {
      "title": "Unet Loader (GGUF)"
    }
  },
  "47": {
    "inputs": {
      "shift": 3,
      "model": [
        "46",
        0
      ]
    },
    "class_type": "ModelSamplingAuraFlow",
    "_meta": {
      "title": "ModelSamplingAuraFlow"
    }
  }
}
"""

EMBEDDED_WORKFLOW = json.loads(EMBEDDED_WORKFLOW_JSON)


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
    if not workflow_path and not os.environ.get("COMFY_IMAGE_WORKFLOW"):
        return deepcopy(EMBEDDED_WORKFLOW)

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


def _build_prompt_text(
    prompt_text: Optional[str],
    headline_text: Optional[str],
    subtext: Optional[str],
    subject: Optional[str],
) -> str:
    """Build the exact prompt sent to ComfyUI from MCP tool arguments."""
    if prompt_text and prompt_text.strip():
        return prompt_text.strip()

    parts = [
        value.strip()
        for value in (headline_text, subtext, subject)
        if value and value.strip()
    ]
    return "\n".join(parts)


def _prepare_workflow(
    prompt_text: Optional[str],
    headline_text: Optional[str],
    subtext: Optional[str],
    subject: Optional[str],
    seed: Optional[int],
    width: Optional[int],
    height: Optional[int],
    filename_prefix: Optional[str],
    workflow_path: Optional[str],
) -> Dict[str, Any]:
    """Copy the template and inject the tool arguments into known nodes."""
    workflow = _load_workflow(workflow_path)

    workflow["45"]["inputs"]["text"] = _build_prompt_text(prompt_text, headline_text, subtext, subject)
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


def _download_dir(download_dir: Optional[str] = None) -> Path:
    """Resolve where fetched ComfyUI outputs should be written."""
    configured = download_dir or os.environ.get("COMFY_IMAGE_DOWNLOAD_DIR")
    if configured:
        return Path(configured).expanduser().resolve()
    return DEFAULT_DOWNLOAD_DIR.resolve()


def _download_image(view_url: str, image: Dict[str, Any], download_dir: Optional[str] = None) -> Dict[str, Any]:
    """Download one ComfyUI /view image into a local directory."""
    target_dir = _download_dir(download_dir)
    target_dir.mkdir(parents=True, exist_ok=True)

    filename = image.get("filename") or "comfy-output.png"
    subfolder = image.get("subfolder") or ""
    safe_parts = [part for part in Path(subfolder).parts if part not in ("", ".", "..")]
    target_path = target_dir.joinpath(*safe_parts, filename).resolve()
    target_path.parent.mkdir(parents=True, exist_ok=True)

    try:
        with _urlrequest.urlopen(view_url, timeout=60) as resp:
            if resp.status < 200 or resp.status >= 300:
                return {"success": False, "error": f"HTTP {resp.status}", "view_url": view_url}
            target_path.write_bytes(resp.read())
    except Exception as e:
        return {"success": False, "error": str(e), "view_url": view_url}

    return {
        "success": True,
        "path": str(target_path),
        "bytes": target_path.stat().st_size,
        "view_url": view_url,
    }


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
    prompt_text: Optional[str] = None,
    headline_text: Optional[str] = None,
    subtext: Optional[str] = None,
    subject: Optional[str] = None,
    seed: Optional[int] = None,
    width: Optional[int] = None,
    height: Optional[int] = None,
    filename_prefix: str = "z-image",
    wait_for_completion: bool = True,
    timeout_seconds: int = 300,
    poll_interval_seconds: float = 1.0,
    download_images: bool = True,
    download_dir: Optional[str] = None,
    server_url: Optional[str] = None,
    workflow_path: Optional[str] = None,
) -> Dict[str, Any]:
    """Generate an image with ComfyUI.

    Args:
        prompt_text: Full prompt to send to ComfyUI. When set, this is used exactly.
        headline_text: Optional first line used when prompt_text is omitted.
        subtext: Optional second line used when prompt_text is omitted.
        subject: Optional additional prompt line used when prompt_text is omitted.
        seed: Optional fixed seed. Randomized when omitted.
        width: Optional image width override. Must be 256-2048 and divisible by 8.
        height: Optional image height override. Must be 256-2048 and divisible by 8.
        filename_prefix: ComfyUI SaveImage prefix.
        wait_for_completion: If True, poll history and return image outputs.
        timeout_seconds: Maximum seconds to wait when wait_for_completion is True.
        poll_interval_seconds: Delay between history polls.
        download_images: If True, fetch completed images into download_dir.
        download_dir: Local directory for fetched images, default comfy_image/outputs.
        server_url: Optional ComfyUI base URL, default COMFYUI_BASE_URL or host.docker.internal:8000.
        workflow_path: Optional workflow template path. Defaults to the embedded workflow.

    Returns:
        Dictionary with prompt_id, seed, status, and generated image metadata.
    """
    prompt = _build_prompt_text(prompt_text, headline_text, subtext, subject)
    if not prompt:
        return {"success": False, "error": "prompt_text or at least one of headline_text, subtext, subject is required"}

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
            prompt_text=prompt,
            headline_text=None,
            subtext=None,
            subject=None,
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
        "prompt_text": workflow["45"]["inputs"]["text"],
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

    if download_images:
        downloads = []
        for image in response["images"]:
            downloads.append(_download_image(image["view_url"], image, download_dir))
        response["downloads"] = downloads

    return response


@mcp.tool()
async def comfy_generation_status(
    prompt_id: str,
    server_url: Optional[str] = None,
    download_images: bool = False,
    download_dir: Optional[str] = None,
) -> Dict[str, Any]:
    """Check whether a ComfyUI generation has completed and return image outputs.

    Args:
        prompt_id: The prompt_id returned by generate_launch_asset.
        server_url: Optional ComfyUI base URL, default COMFYUI_BASE_URL or host.docker.internal:8000.
        download_images: If True, fetch completed images into download_dir.
        download_dir: Local directory for fetched images, default comfy_image/outputs.

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
    response = {
        "success": True,
        "status": "completed",
        "prompt_id": prompt_id,
        "images": extracted["images"],
        "outputs": extracted["outputs"],
    }
    if download_images:
        response["downloads"] = [
            _download_image(image["view_url"], image, download_dir)
            for image in response["images"]
        ]
    return response


@mcp.tool()
async def fetch_comfy_image(
    view_url: str,
    filename: Optional[str] = None,
    download_dir: Optional[str] = None,
) -> Dict[str, Any]:
    """Fetch a ComfyUI /view image URL into a local directory.

    Args:
        view_url: ComfyUI /view URL returned by generate_launch_asset.
        filename: Optional local filename override.
        download_dir: Local directory for fetched images, default comfy_image/outputs.

    Returns:
        Dictionary with the saved local path and byte count.
    """
    if not view_url or not view_url.strip():
        return {"success": False, "error": "view_url is required"}

    parsed = _urlparse.urlparse(view_url.strip())
    params = _urlparse.parse_qs(parsed.query)
    image = {
        "filename": filename or (params.get("filename", ["comfy-output.png"])[0]),
        "subfolder": params.get("subfolder", [""])[0],
        "type": params.get("type", ["output"])[0],
    }
    return _download_image(view_url.strip(), image, download_dir)


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
        "workflow_source": "file" if workflow_path or os.environ.get("COMFY_IMAGE_WORKFLOW") else "embedded",
        "workflow_path": str(path) if workflow_path or os.environ.get("COMFY_IMAGE_WORKFLOW") else None,
        "workflow_exists": path.exists() if workflow_path or os.environ.get("COMFY_IMAGE_WORKFLOW") else True,
        "output_dir": os.environ.get("COMFYUI_OUTPUT_DIR"),
        "download_dir": str(_download_dir()),
    }


if __name__ == "__main__":
    mcp.run()
