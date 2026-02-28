#!/usr/bin/env python3
"""
MCP: pdf-reader

Utilities for downloading PDFs and rendering selected pages to images.
Designed to feed Claude vision with page images and OCR workflows.
"""

from __future__ import annotations

import base64
import hashlib
import os
import asyncio
from pathlib import Path
from typing import Dict, List, Optional
from urllib.parse import urlparse

import aiohttp
import fitz  # PyMuPDF
try:
    import anthropic
    ANTHROPIC_AVAILABLE = True
except ImportError:
    anthropic = None
    ANTHROPIC_AVAILABLE = False

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("pdf-reader")
DEFAULT_OCR_MODEL = os.getenv("PDF_OCR_CLAUDE_MODEL", "claude-sonnet-4-5-20250929")
DEFAULT_NUTS_OCR_MODEL = os.getenv("NUTS_OCR_MODEL", "benhaotang/Nanonets-OCR-s")


def _safe_filename(url: str) -> str:
    parsed = urlparse(url)
    name = os.path.basename(parsed.path) or "download.pdf"
    if not name.lower().endswith(".pdf"):
        name = f"{name}.pdf"
    return name


def _ensure_dir(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)


def _dpi_to_scale(dpi: int) -> float:
    return max(72, dpi) / 72.0


def _compute_user_hash(email: str) -> str:
    digest = hashlib.sha256(email.encode("utf-8")).hexdigest()
    return digest[:12]


def _parse_page_spec(page_spec: str, page_count: int) -> List[int]:
    """
    Parse a page spec string like:
    - "all"
    - "1,3,5"
    - "1-4,8,10-12"
    Returns 1-based page numbers.
    """
    if not page_spec or page_spec.strip().lower() == "all":
        return list(range(1, page_count + 1))

    pages: List[int] = []
    seen = set()
    for raw_token in page_spec.split(","):
        token = raw_token.strip()
        if not token:
            continue
        if "-" in token:
            parts = token.split("-", 1)
            if len(parts) != 2:
                raise ValueError(f"invalid_page_range: {token}")
            start = int(parts[0].strip())
            end = int(parts[1].strip())
            if start > end:
                raise ValueError(f"invalid_page_range: {token}")
            for page_num in range(start, end + 1):
                if page_num < 1 or page_num > page_count:
                    raise ValueError(f"page_out_of_range: {page_num}")
                if page_num not in seen:
                    seen.add(page_num)
                    pages.append(page_num)
        else:
            page_num = int(token)
            if page_num < 1 or page_num > page_count:
                raise ValueError(f"page_out_of_range: {page_num}")
            if page_num not in seen:
                seen.add(page_num)
                pages.append(page_num)

    if not pages:
        raise ValueError("no_pages_selected")
    return pages


def _encode_png_for_claude(data: bytes) -> Dict[str, object]:
    return {
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": "image/png",
            "data": base64.b64encode(data).decode("utf-8"),
        },
    }


def _is_supported_for_nuts_ocr(path: Path) -> bool:
    return path.suffix.lower() in {".pdf", ".png", ".jpg", ".jpeg", ".webp", ".tiff"}


@mcp.tool()
async def download_pdf(url: str, dest_dir: str = "./pdf", filename: Optional[str] = None) -> Dict[str, object]:
    """
    Download a PDF from a URL to a local directory.

    Args:
        url: PDF URL
        dest_dir: Directory to save the PDF
        filename: Optional override for the saved filename
    """
    if not url:
        return {"success": False, "error": "url_required"}

    dest = Path(dest_dir)
    _ensure_dir(dest)
    name = filename or _safe_filename(url)
    if not name.lower().endswith(".pdf"):
        name = f"{name}.pdf"
    out_path = dest / name

    try:
        async with aiohttp.ClientSession() as session:
            async with session.get(url, timeout=60) as resp:
                if resp.status != 200:
                    return {"success": False, "status": resp.status, "error": "download_failed"}
                data = await resp.read()
        out_path.write_bytes(data)
        return {
            "success": True,
            "path": str(out_path),
            "bytes": len(data),
        }
    except Exception as e:
        return {"success": False, "error": str(e)}


@mcp.tool()
async def list_pdf_pages(pdf_path: str) -> Dict[str, object]:
    """
    Return basic metadata about a PDF and its page count.
    """
    path = Path(pdf_path)
    if not path.exists():
        return {"success": False, "error": "file_not_found", "path": str(path)}

    try:
        doc = fitz.open(path)
        page_count = doc.page_count
        doc.close()
        return {
            "success": True,
            "path": str(path),
            "pages": page_count,
        }
    except Exception as e:
        return {"success": False, "error": str(e), "path": str(path)}


@mcp.tool()
async def split_pdf_pages(
    pdf_path: str,
    pages: List[int],
    output_dir: str = "./pdf/pages",
    dpi: int = 200,
    image_format: str = "png",
) -> Dict[str, object]:
    """
    Render selected PDF pages to images.

    Args:
        pdf_path: Path to PDF
        pages: 1-based page numbers to render
        output_dir: Directory to write page images
        dpi: Render DPI (default 200)
        image_format: "png" or "jpg"
    """
    path = Path(pdf_path)
    if not path.exists():
        return {"success": False, "error": "file_not_found", "path": str(path)}
    if not pages:
        return {"success": False, "error": "pages_required"}

    out_dir = Path(output_dir)
    _ensure_dir(out_dir)
    fmt = image_format.lower().strip(".")
    if fmt not in {"png", "jpg", "jpeg"}:
        return {"success": False, "error": "invalid_image_format"}

    try:
        doc = fitz.open(path)
        scale = _dpi_to_scale(dpi)
        matrix = fitz.Matrix(scale, scale)
        results = []
        for page_num in pages:
            if page_num < 1 or page_num > doc.page_count:
                results.append({
                    "page": page_num,
                    "success": False,
                    "error": "page_out_of_range",
                })
                continue
            page = doc.load_page(page_num - 1)
            pix = page.get_pixmap(matrix=matrix, alpha=False)
            out_name = f"{path.stem}_p{page_num}.{fmt}"
            out_path = out_dir / out_name
            pix.save(str(out_path))
            results.append({
                "page": page_num,
                "success": True,
                "path": str(out_path),
                "bytes": out_path.stat().st_size,
            })
        doc.close()
        return {
            "success": True,
            "pdf_path": str(path),
            "output_dir": str(out_dir),
            "pages_requested": pages,
            "results": results,
        }
    except Exception as e:
        return {"success": False, "error": str(e), "path": str(path)}


@mcp.tool()
async def pdf_rotate(
    pdf_path: str,
    output_path: str,
    angle: int = 90,
    pages: str = "all",
) -> Dict[str, object]:
    """
    Rotate pages in a PDF and write a new file.

    Args:
        pdf_path: Input PDF path
        output_path: Output PDF path (must be different from input)
        angle: Rotation angle in degrees (allowed: +/-90, +/-180, +/-270)
        pages: Page selection string ("all", "1,3,5", "1-4,8")
    """
    in_path = Path(pdf_path)
    out_path = Path(output_path)

    if not in_path.exists():
        return {"success": False, "error": "file_not_found", "path": str(in_path)}

    if in_path.resolve() == out_path.resolve():
        return {
            "success": False,
            "error": "output_must_differ_from_input",
            "pdf_path": str(in_path),
            "output_path": str(out_path),
        }

    if angle not in {-270, -180, -90, 90, 180, 270}:
        return {
            "success": False,
            "error": "invalid_angle",
            "allowed_angles": [-270, -180, -90, 90, 180, 270],
        }

    try:
        doc = fitz.open(in_path)
        selected_pages = _parse_page_spec(pages, doc.page_count)
        normalized_angle = angle % 360

        for page_num in selected_pages:
            page = doc.load_page(page_num - 1)  # PyMuPDF uses 0-based pages
            current_rotation = page.rotation or 0
            page.set_rotation((current_rotation + normalized_angle) % 360)

        _ensure_dir(out_path.parent)
        doc.save(out_path)
        doc.close()

        return {
            "success": True,
            "pdf_path": str(in_path),
            "output_path": str(out_path),
            "angle_applied": normalized_angle,
            "pages": selected_pages,
            "pages_rotated": len(selected_pages),
            "bytes": out_path.stat().st_size,
        }
    except Exception as e:
        return {
            "success": False,
            "error": str(e),
            "pdf_path": str(in_path),
            "output_path": str(out_path),
        }


@mcp.tool()
async def ocr_pdf_pages_with_claude(
    pdf_path: str,
    pages: Optional[List[int]] = None,
    dpi: int = 200,
    model: Optional[str] = None,
    max_tokens: int = 1800,
) -> Dict[str, object]:
    """
    OCR selected PDF pages using Claude vision.

    Args:
        pdf_path: Path to PDF
        pages: 1-based page numbers to OCR (defaults to all pages)
        dpi: Render DPI before OCR (default 200)
        model: Claude model ID
        max_tokens: Claude output token limit per page
    """
    if not ANTHROPIC_AVAILABLE:
        return {"success": False, "error": "anthropic_package_missing"}

    api_key = os.environ.get("ANTHROPIC_API_KEY")
    if not api_key:
        return {"success": False, "error": "ANTHROPIC_API_KEY_not_set"}

    path = Path(pdf_path)
    if not path.exists():
        return {"success": False, "error": "file_not_found", "path": str(path)}

    try:
        doc = fitz.open(path)
    except Exception as e:
        return {"success": False, "error": str(e), "path": str(path)}

    try:
        if pages and len(pages) > 0:
            selected_pages = pages
        else:
            selected_pages = list(range(1, doc.page_count + 1))

        client = anthropic.Anthropic(api_key=api_key)
        scale = _dpi_to_scale(dpi)
        matrix = fitz.Matrix(scale, scale)
        results = []
        total_input_tokens = 0
        total_output_tokens = 0

        for page_num in selected_pages:
            if page_num < 1 or page_num > doc.page_count:
                results.append({"page": page_num, "success": False, "error": "page_out_of_range"})
                continue

            page = doc.load_page(page_num - 1)
            pix = page.get_pixmap(matrix=matrix, alpha=False)
            content = [
                {
                    "type": "text",
                    "text": (
                        "Perform OCR on this PDF page image. "
                        "Transcribe all visible text faithfully. "
                        "Keep line breaks where possible. "
                        "Do not summarize or add commentary."
                    ),
                },
                _encode_png_for_claude(pix.tobytes("png")),
            ]

            try:
                message = client.messages.create(
                    model=model or DEFAULT_OCR_MODEL,
                    max_tokens=max_tokens,
                    messages=[{"role": "user", "content": content}],
                )
                text_parts = []
                for block in message.content:
                    if getattr(block, "type", None) == "text":
                        text_parts.append(getattr(block, "text", ""))
                text = "\n".join(part for part in text_parts if part).strip()
                in_tokens = message.usage.input_tokens
                out_tokens = message.usage.output_tokens
                total_input_tokens += in_tokens
                total_output_tokens += out_tokens
                results.append(
                    {
                        "page": page_num,
                        "success": True,
                        "text": text,
                        "usage": {
                            "input_tokens": in_tokens,
                            "output_tokens": out_tokens,
                        },
                    }
                )
            except Exception as e:
                results.append({"page": page_num, "success": False, "error": str(e)})

        return {
            "success": True,
            "pdf_path": str(path),
            "pages_requested": selected_pages,
            "model": model or DEFAULT_OCR_MODEL,
            "results": results,
            "usage_totals": {
                "input_tokens": total_input_tokens,
                "output_tokens": total_output_tokens,
            },
        }
    finally:
        doc.close()


@mcp.tool()
async def nuts_ocr_fetch_markdown(
    session_id: str,
    user_email: str = "anonymous@gnosis-ocr.local",
    base_url: str = "https://ocr.nuts.services",
) -> Dict[str, object]:
    """
    Fetch OCR markdown output from OCR Nuts by session id.

    Args:
        session_id: OCR session id
        user_email: Email identity used when uploading
        base_url: OCR service base URL
    """
    if not session_id:
        return {"success": False, "error": "session_id_required"}

    root = base_url.rstrip("/")
    user_hash = _compute_user_hash(user_email)
    markdown_url = f"{root}/api/results/{user_hash}/{session_id}/markdown.md"

    try:
        async with aiohttp.ClientSession() as session:
            async with session.get(markdown_url, timeout=60) as resp:
                if resp.status != 200:
                    return {
                        "success": False,
                        "error": "markdown_fetch_failed",
                        "status": resp.status,
                        "session_id": session_id,
                        "user_hash": user_hash,
                        "markdown_url": markdown_url,
                    }
                markdown = await resp.text()
        return {
            "success": True,
            "session_id": session_id,
            "user_hash": user_hash,
            "markdown_url": markdown_url,
            "markdown": markdown,
        }
    except Exception as e:
        return {
            "success": False,
            "error": str(e),
            "session_id": session_id,
            "user_hash": user_hash,
            "markdown_url": markdown_url,
        }


@mcp.tool()
async def nuts_ocr_upload_and_wait(
    file_path: str,
    user_email: str = "anonymous@gnosis-ocr.local",
    base_url: str = "https://ocr.nuts.services",
    ocr_model: Optional[str] = None,
    poll_seconds: int = 3,
    timeout_seconds: int = 300,
    fetch_markdown: bool = True,
) -> Dict[str, object]:
    """
    Upload a document to OCR Nuts, poll status until complete, optionally fetch markdown.

    Args:
        file_path: Path to local PDF/image file
        user_email: Identity sent to OCR service via X-User-Email
        base_url: OCR service base URL
        ocr_model: OCR model id (defaults to benhaotang/Nanonets-OCR-s)
        poll_seconds: Poll interval (seconds)
        timeout_seconds: Max wait time (seconds)
        fetch_markdown: If True, fetch markdown output when complete
    """
    path = Path(file_path)
    if not path.exists():
        return {"success": False, "error": "file_not_found", "path": str(path)}
    if not _is_supported_for_nuts_ocr(path):
        return {
            "success": False,
            "error": "unsupported_file_type",
            "path": str(path),
            "supported": [".pdf", ".png", ".jpg", ".jpeg", ".webp", ".tiff"],
        }

    root = base_url.rstrip("/")
    user_hash = _compute_user_hash(user_email)
    upload_url = f"{root}/storage/upload"
    status_url_template = f"{root}/storage/{user_hash}" + "/{session_id}/session_status.json"
    model_id = ocr_model or DEFAULT_NUTS_OCR_MODEL

    session_id: Optional[str] = None
    try:
        async with aiohttp.ClientSession() as session:
            with path.open("rb") as fh:
                form = aiohttp.FormData()
                form.add_field("file", fh, filename=path.name)
                form.add_field("ocr_model", model_id)
                async with session.post(
                    upload_url,
                    data=form,
                    headers={"X-User-Email": user_email},
                    timeout=120,
                ) as resp:
                    if resp.status != 200:
                        body = await resp.text()
                        return {
                            "success": False,
                            "error": "upload_failed",
                            "status": resp.status,
                            "body": body[:2000],
                            "upload_url": upload_url,
                        }
                    payload = await resp.json()
                    session_id = payload.get("session_id")
                    if not session_id:
                        return {
                            "success": False,
                            "error": "missing_session_id",
                            "upload_response": payload,
                        }

            status_url = status_url_template.format(session_id=session_id)
            elapsed = 0
            last_stage = {}
            while elapsed <= timeout_seconds:
                async with session.get(status_url, timeout=60) as status_resp:
                    if status_resp.status == 200:
                        status_data = await status_resp.json()
                        stage = status_data.get("stages", {}).get("document_processing", {}) or {}
                        last_stage = stage
                        if stage.get("status") == "complete":
                            result: Dict[str, object] = {
                                "success": True,
                                "session_id": session_id,
                                "user_hash": user_hash,
                                "upload_url": upload_url,
                                "status_url": status_url,
                                "markdown_url": f"{root}/api/results/{user_hash}/{session_id}/markdown.md",
                                "stage": stage,
                            }
                            if fetch_markdown:
                                async with session.get(result["markdown_url"], timeout=60) as md_resp:
                                    if md_resp.status == 200:
                                        result["markdown"] = await md_resp.text()
                                    else:
                                        result["markdown_error"] = {
                                            "status": md_resp.status,
                                            "error": "markdown_fetch_failed",
                                        }
                            return result

                await asyncio.sleep(max(1, poll_seconds))
                elapsed += max(1, poll_seconds)

            return {
                "success": False,
                "error": "timeout_waiting_for_completion",
                "session_id": session_id,
                "user_hash": user_hash,
                "status_url": status_url_template.format(session_id=session_id),
                "last_stage": last_stage,
                "elapsed_seconds": elapsed,
            }
    except Exception as e:
        return {
            "success": False,
            "error": str(e),
            "session_id": session_id,
            "path": str(path),
            "upload_url": upload_url,
        }


if __name__ == "__main__":
    mcp.run()
