#!/usr/bin/env python3
"""
Persistent HTTP transcription service.
Keeps Whisper model loaded, accepts file uploads, processes queue.

Start locally:
    pip install openai-whisper aiohttp soundfile
    python transcription_service.py

Or with Docker:
    docker compose up -d
"""

import asyncio
import os
import sys
import aiohttp
from aiohttp import web
from pathlib import Path
import whisper
import uuid
import json
from datetime import datetime, timezone

# Service configuration
SERVICE_PORT = int(os.environ.get("WHISPER_PORT", "8767"))
WHISPER_MODEL = os.environ.get("WHISPER_MODEL", "medium")

# Storage paths — use local directory when running outside Docker
STORAGE_DIR = Path(os.environ.get("STORAGE_DIR", str(Path.home() / ".hyperia" / "transcription-storage")))
PENDING_DIR = STORAGE_DIR / "pending"
TRANSCRIPTS_DIR = STORAGE_DIR / "transcripts"

# Initialize storage
PENDING_DIR.mkdir(parents=True, exist_ok=True)
TRANSCRIPTS_DIR.mkdir(parents=True, exist_ok=True)

# In-memory job queue
JOBS = {}

# Model loading
MODEL = None
GPU_AVAILABLE = False


def utc_now():
    """Get current UTC timestamp as ISO string."""
    return datetime.now(timezone.utc).isoformat()


async def load_model():
    """Load Whisper model on startup."""
    global MODEL, GPU_AVAILABLE
    import torch
    GPU_AVAILABLE = torch.cuda.is_available()
    device = "cuda" if GPU_AVAILABLE else "cpu"
    print(f"Loading Whisper model: {WHISPER_MODEL} on {device.upper()}", file=sys.stderr, flush=True)
    MODEL = whisper.load_model(WHISPER_MODEL, device=device)
    print(f"Model {WHISPER_MODEL} loaded on {device.upper()} and ready", file=sys.stderr, flush=True)


async def handle_transcribe(request):
    """POST /transcribe - Accept WAV upload and queue transcription."""
    try:
        reader = await request.multipart()

        job_id = None
        wav_data = None
        original_filename = None
        model_name = WHISPER_MODEL
        callback_url = None

        async for field in reader:
            if field.name == "file":
                wav_data = await field.read()
                if hasattr(field, 'filename') and field.filename:
                    original_filename = field.filename
            elif field.name == "job_id":
                job_id = (await field.read()).decode()
            elif field.name == "model":
                model_name = (await field.read()).decode()
            elif field.name == "callback_url":
                callback_url = (await field.read()).decode()

        if not wav_data:
            return web.json_response({"error": "No file uploaded"}, status=400)

        if not job_id:
            job_id = str(uuid.uuid4())

        # Save WAV to pending
        wav_file = PENDING_DIR / f"{job_id}.wav"
        wav_file.write_bytes(wav_data)

        print(f"Job {job_id} received: {len(wav_data)} bytes (original: {original_filename})", file=sys.stderr, flush=True)

        # Queue job
        JOBS[job_id] = {
            "status": "queued",
            "model": model_name,
            "wav_file": str(wav_file),
            "original_filename": original_filename or f"{job_id}.wav",
            "callback_url": callback_url,
            "created": utc_now(),
            "file_size": len(wav_data)
        }

        return web.json_response({
            "success": True,
            "job_id": job_id,
            "status": "queued",
            "gpu_available": GPU_AVAILABLE,
            "message": f"Job {job_id} queued for transcription"
        })

    except Exception as e:
        print(f"Upload error: {e}", file=sys.stderr, flush=True)
        return web.json_response({"error": str(e)}, status=500)


async def handle_status(request):
    """GET /status/{job_id} - Check transcription status."""
    try:
        job_id = request.match_info["job_id"]

        if job_id not in JOBS:
            return web.json_response({"error": "Job not found"}, status=404)

        job = JOBS[job_id]
        response = {
            "success": True,
            "job_id": job_id,
            "status": job["status"],
            "created": job["created"]
        }

        if "progress" in job:
            response["progress"] = job["progress"]
        if "completed" in job:
            response["completed"] = job["completed"]
        if "error" in job:
            response["error"] = job["error"]
        if job["status"] == "completed" and "transcript" in job:
            response["transcript_preview"] = job["transcript"][:200] + "..." if len(job["transcript"]) > 200 else job["transcript"]

        return web.json_response(response)
    except Exception as e:
        return web.json_response({"error": f"Internal server error: {str(e)}"}, status=500)


async def handle_download(request):
    """GET /download/{job_id} - Download completed transcript."""
    try:
        job_id = request.match_info["job_id"]

        if job_id not in JOBS:
            return web.json_response({"error": "Job not found"}, status=404)

        job = JOBS[job_id]

        if job["status"] != "completed":
            return web.json_response({
                "error": "Transcription not ready",
                "status": job["status"]
            }, status=404)

        if "transcript_file" not in job:
            transcript_file = TRANSCRIPTS_DIR / f"{job_id}.txt"
        else:
            transcript_file = Path(job["transcript_file"])

        if not transcript_file.exists():
            return web.json_response({"error": f"Transcript file missing: {transcript_file.name}"}, status=500)

        return web.Response(
            text=transcript_file.read_text(),
            content_type="text/plain"
        )
    except Exception as e:
        return web.json_response({"error": f"Internal server error: {str(e)}"}, status=500)


async def handle_health(request):
    """GET /health - Service health check."""
    queued_count = len([j for j in JOBS.values() if j["status"] == "queued"])
    processing_count = len([j for j in JOBS.values() if j["status"] == "processing"])
    completed_count = len([j for j in JOBS.values() if j["status"] == "completed"])
    failed_count = len([j for j in JOBS.values() if j["status"] == "failed"])

    return web.json_response({
        "status": "ok",
        "model_loaded": MODEL is not None,
        "model_name": WHISPER_MODEL,
        "gpu_available": GPU_AVAILABLE,
        "queue": {
            "queued": queued_count,
            "processing": processing_count,
            "completed": completed_count,
            "failed": failed_count,
            "total": len(JOBS)
        }
    })


async def process_queue():
    """Background task to process queued transcription jobs."""
    print("Queue processor started", file=sys.stderr, flush=True)

    while True:
        try:
            queued = [jid for jid, job in JOBS.items() if job["status"] == "queued"]

            if queued:
                job_id = queued[0]
                job = JOBS[job_id]

                print(f"Processing job {job_id}", file=sys.stderr, flush=True)

                job["status"] = "processing"
                job["progress"] = "Transcribing audio..."

                start_time = datetime.now(timezone.utc)
                loop = asyncio.get_event_loop()
                result = await loop.run_in_executor(
                    None,
                    lambda: MODEL.transcribe(job["wav_file"], beam_size=5, verbose=False)
                )
                end_time = datetime.now(timezone.utc)

                transcript_text = result["text"].strip()
                language = result.get("language", "unknown")
                segments = result.get("segments", [])
                audio_duration = segments[-1]["end"] if segments else 0.0

                wav_filename = job.get("original_filename", Path(job["wav_file"]).name)
                processing_duration = (end_time - start_time).total_seconds()

                formatted_transcript = f"""========== TRANSMISSION STATUS REPORT ==========
STATUS: TRANSCRIPTION COMPLETE
FILE: {wav_filename}
MODEL: {WHISPER_MODEL}
STARTED: {start_time.strftime('%Y-%m-%d %H:%M:%S %Z')}
FINISHED: {end_time.strftime('%Y-%m-%d %H:%M:%S %Z')}
DURATION (AUDIO): {audio_duration:.2f}s
LANGUAGE: {language}
------------------------------------------------------------
TELEGRAPH COPY FOLLOWS
0001 [0000.00s - {audio_duration:.2f}s] {transcript_text}
END OF TRANSMISSION STOP
"""

                transcript_filename = Path(wav_filename).stem + ".txt"
                transcript_file = TRANSCRIPTS_DIR / transcript_filename
                transcript_file.write_text(formatted_transcript)

                job["status"] = "completed"
                job["transcript_file"] = str(transcript_file)
                job["transcript"] = transcript_text
                job["completed"] = utc_now()

                print(f"Job {job_id} completed: {len(transcript_text)} chars in {processing_duration:.1f}s", file=sys.stderr, flush=True)

                # Send callback if URL was provided
                callback_url = job.get("callback_url")
                if callback_url:
                    try:
                        async with aiohttp.ClientSession() as session:
                            payload = {
                                "job_id": job_id,
                                "status": "completed",
                                "text": transcript_text,
                                "file": wav_filename,
                            }
                            async with session.post(callback_url, json=payload, timeout=aiohttp.ClientTimeout(total=10)) as resp:
                                print(f"Callback sent to {callback_url} -> {resp.status}", file=sys.stderr, flush=True)
                    except Exception as cb_err:
                        print(f"Callback failed for {job_id}: {cb_err}", file=sys.stderr, flush=True)

                # Clean up WAV file
                wav_path = Path(job["wav_file"])
                if wav_path.exists():
                    wav_path.unlink()

            else:
                await asyncio.sleep(2)

        except Exception as e:
            print(f"Queue processor error for job {job_id}: {e}", file=sys.stderr, flush=True)
            if job_id in JOBS:
                JOBS[job_id]["status"] = "failed"
                JOBS[job_id]["error"] = str(e)
                JOBS[job_id]["failed"] = utc_now()
                callback_url = JOBS[job_id].get("callback_url")
                if callback_url:
                    try:
                        async with aiohttp.ClientSession() as session:
                            payload = {"job_id": job_id, "status": "failed", "error": str(e)}
                            await session.post(callback_url, json=payload, timeout=aiohttp.ClientTimeout(total=10))
                    except Exception:
                        pass

        await asyncio.sleep(0.5)


async def handle_classify(request):
    """POST /classify - Classify ambient sounds in a WAV upload."""
    try:
        reader = await request.multipart()
        wav_data = None

        async for field in reader:
            if field.name == "file":
                wav_data = await field.read()

        if not wav_data:
            return web.json_response({"error": "No file uploaded"}, status=400)

        import numpy as np
        import io
        import soundfile as sf

        audio, sr = sf.read(io.BytesIO(wav_data), dtype='float32')

        if audio.ndim > 1:
            audio = audio.mean(axis=1)

        from numpy.fft import rfft, rfftfreq

        window_size = min(len(audio), sr)
        labels = []

        windowed = audio[:window_size] * np.hanning(window_size)
        spectrum = np.abs(rfft(windowed)) ** 2
        freqs = rfftfreq(window_size, 1.0 / sr)

        total_power = np.sum(spectrum) + 1e-10
        rms = np.sqrt(np.mean(audio ** 2))

        def band_power(lo, hi):
            mask = (freqs >= lo) & (freqs < hi)
            return np.sum(spectrum[mask]) / total_power

        sub_bass = band_power(20, 60)
        bass = band_power(60, 250)
        mid = band_power(500, 2000)
        upper_mid = band_power(2000, 4000)
        presence = band_power(4000, 8000)
        brilliance = band_power(8000, 16000)

        peak_idx = np.argmax(spectrum[1:]) + 1
        peak_freq = freqs[peak_idx]
        centroid = np.sum(freqs * spectrum) / total_power
        zcr = np.sum(np.abs(np.diff(np.sign(audio[:window_size])))) / (2 * window_size)

        if rms < 0.005:
            labels.append({"label": "Silence", "score": 0.95})
        else:
            if bass + sub_bass > 0.5 and centroid < 500:
                labels.append({"label": "Traffic/machinery", "score": round(min((bass + sub_bass) * 1.2, 0.99), 2)})
            if zcr > 0.15 and brilliance + presence > 0.15:
                labels.append({"label": "Wind", "score": round(min(zcr * 2.0, 0.99), 2)})
            if upper_mid + presence > 0.3 and centroid > 2000:
                labels.append({"label": "Bird/animal", "score": round(min((upper_mid + presence) * 1.5, 0.99), 2)})
            if mid + upper_mid > 0.4 and 500 < peak_freq < 4000 and zcr > 0.05:
                labels.append({"label": "Dog bark", "score": round(min((mid + upper_mid) * 1.0, 0.99), 2)})
            if presence + brilliance > 0.25 and zcr > 0.1 and centroid > 3000:
                labels.append({"label": "Rain/water", "score": round(min((presence + brilliance) * 1.5, 0.99), 2)})
            if bass > 0.3 and centroid < 800:
                labels.append({"label": "Footsteps/impact", "score": round(min(bass * 1.3, 0.99), 2)})
            if not labels:
                labels.append({"label": "Ambient noise", "score": round(rms * 5, 2)})

        labels.sort(key=lambda x: x["score"], reverse=True)

        return web.json_response({
            "success": True,
            "labels": labels[:5],
            "rms": round(float(rms), 4),
            "centroid": round(float(centroid), 1),
            "peak_freq": round(float(peak_freq), 1),
        })

    except Exception as e:
        return web.json_response({"error": str(e)}, status=500)


async def start_background_tasks(app):
    """Start background tasks on app startup."""
    await load_model()
    app["queue_processor"] = asyncio.create_task(process_queue())


async def cleanup_background_tasks(app):
    """Cleanup background tasks on shutdown."""
    app["queue_processor"].cancel()
    await app["queue_processor"]


def main():
    """Start the transcription service."""
    print("=" * 60, file=sys.stderr, flush=True)
    print("TRANSCRIPTION SERVICE", file=sys.stderr, flush=True)
    print(f"  Port: {SERVICE_PORT}", file=sys.stderr, flush=True)
    print(f"  Model: {WHISPER_MODEL}", file=sys.stderr, flush=True)
    print(f"  Storage: {STORAGE_DIR}", file=sys.stderr, flush=True)
    print("=" * 60, file=sys.stderr, flush=True)

    app = web.Application()
    app.router.add_post("/transcribe", handle_transcribe)
    app.router.add_post("/classify", handle_classify)
    app.router.add_get("/status/{job_id}", handle_status)
    app.router.add_get("/download/{job_id}", handle_download)
    app.router.add_get("/health", handle_health)

    app.on_startup.append(start_background_tasks)
    app.on_cleanup.append(cleanup_background_tasks)

    web.run_app(app, host="0.0.0.0", port=SERVICE_PORT)


if __name__ == "__main__":
    main()
