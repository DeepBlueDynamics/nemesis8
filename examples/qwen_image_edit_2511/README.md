# Qwen Image Edit 2511 (GGUF) test project

This folder contains a minimal ComfyUI workflow and a Python downloader for the
Qwen-Image-Edit-2511 GGUF setup. Defaults are tuned for a 12GB GPU.

## Files
- `download_models.py`: downloads the GGUF UNet, text encoder, VAE, and Lightning LoRA.
- `workflow_qwen_image_edit_2511_gguf.json`: ComfyUI workflow using the GGUF UNet loader.

## Quick start
```bash
python /workspace/codex-container/examples/qwen_image_edit_2511/download_models.py
```

This creates a local model tree under:
```
/workspace/codex-container/examples/qwen_image_edit_2511/models/
  unet/
  text_encoders/
  vae/
  loras/
```

Point ComfyUI at that folder (symlink/copy or configure extra model paths), then
import the workflow JSON.

## 12GB GPU notes
- Default quant is `Q3_K_M` (~9.7GB). This is the safest fit for 12GB VRAM.
- You can try `Q4_0` (~11.9GB) or `Q4_K_S` (~12.3GB), but they are tight on 12GB.
- If you hit OOM, lower `ImageScaleToTotalPixels` to `0.8` in the workflow.

To switch quant:
```bash
python download_models.py --gguf qwen-image-edit-2511-Q4_0.gguf
```

## ComfyUI requirements
- Install the GGUF loader nodes from `https://github.com/city96/ComfyUI-GGUF`.
- The workflow uses the Lightning LoRA. If you skip it, disable the LoRA node in ComfyUI.

## Where the files come from
- GGUF UNet: `https://huggingface.co/unsloth/Qwen-Image-Edit-2511-GGUF`
- Text encoder + VAE: `https://huggingface.co/Comfy-Org/Qwen-Image_ComfyUI`
- Lightning LoRA: `https://huggingface.co/lightx2v/Qwen-Image-Lightning`
