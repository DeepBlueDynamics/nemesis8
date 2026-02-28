#!/usr/bin/env python3
"""Download Qwen-Image-Edit-2511 GGUF workflow models into a local folder.

Defaults are tuned for a 12GB card (Q3_K_M quant). Adjust with --gguf.
"""
from __future__ import annotations

import argparse
import sys
import urllib.request
from pathlib import Path

UNET_BASE_URL = "https://huggingface.co/unsloth/Qwen-Image-Edit-2511-GGUF/resolve/main/"
TEXT_ENCODER_URL = (
    "https://huggingface.co/Comfy-Org/Qwen-Image_ComfyUI/resolve/main/"
    "split_files/text_encoders/qwen_2.5_vl_7b_fp8_scaled.safetensors"
)
VAE_URL = (
    "https://huggingface.co/Comfy-Org/Qwen-Image_ComfyUI/resolve/main/"
    "split_files/vae/qwen_image_vae.safetensors"
)
LORA_URL = (
    "https://huggingface.co/lightx2v/Qwen-Image-Lightning/resolve/main/"
    "Qwen-Image-Lightning-4steps-V1.0.safetensors"
)

CHUNK_SIZE = 1024 * 1024 * 4


def _download(url: str, dest: Path, overwrite: bool) -> None:
    if dest.exists() and not overwrite:
        print(f"skip (exists): {dest}")
        return

    dest.parent.mkdir(parents=True, exist_ok=True)
    print(f"download: {url}\n  -> {dest}")
    req = urllib.request.Request(url, headers={"User-Agent": "qwen-image-edit-downloader"})
    with urllib.request.urlopen(req) as resp, dest.open("wb") as f:
        total = resp.headers.get("content-length")
        total = int(total) if total is not None else None
        downloaded = 0
        while True:
            chunk = resp.read(CHUNK_SIZE)
            if not chunk:
                break
            f.write(chunk)
            downloaded += len(chunk)
            if total:
                pct = (downloaded / total) * 100
                print(f"  {downloaded/1e9:.2f}GB / {total/1e9:.2f}GB ({pct:.1f}%)", end="\r")
        if total:
            print("".ljust(60), end="\r")
    print(f"done: {dest}")


def main() -> int:
    parser = argparse.ArgumentParser(description="Download Qwen-Image-Edit-2511 GGUF workflow models.")
    parser.add_argument(
        "--output-dir",
        default=str(Path(__file__).resolve().parent / "models"),
        help="Local output directory for models (default: ./models)",
    )
    parser.add_argument(
        "--gguf",
        default="qwen-image-edit-2511-Q3_K_M.gguf",
        help="GGUF filename to download (default: qwen-image-edit-2511-Q3_K_M.gguf)",
    )
    parser.add_argument(
        "--skip-lora",
        action="store_true",
        help="Skip downloading the Lightning LoRA",
    )
    parser.add_argument(
        "--overwrite",
        action="store_true",
        help="Overwrite existing files",
    )
    args = parser.parse_args()

    output_dir = Path(args.output_dir).resolve()
    gguf_name = args.gguf
    if not gguf_name.endswith(".gguf"):
        gguf_name += ".gguf"

    unet_url = UNET_BASE_URL + gguf_name
    unet_path = output_dir / "unet" / gguf_name
    text_encoder_path = output_dir / "text_encoders" / Path(TEXT_ENCODER_URL).name
    vae_path = output_dir / "vae" / Path(VAE_URL).name
    lora_path = output_dir / "loras" / Path(LORA_URL).name

    print(f"Output directory: {output_dir}")
    _download(unet_url, unet_path, args.overwrite)
    _download(TEXT_ENCODER_URL, text_encoder_path, args.overwrite)
    _download(VAE_URL, vae_path, args.overwrite)

    if not args.skip_lora:
        _download(LORA_URL, lora_path, args.overwrite)
    else:
        print("skip: Lightning LoRA (--skip-lora)")

    print("\nNext:")
    print(f"- Import workflow: {Path(__file__).resolve().parent / 'workflow_qwen_image_edit_2511_gguf.json'}")
    print("- Point ComfyUI to this models folder or copy/symlink it into your ComfyUI models directory.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
