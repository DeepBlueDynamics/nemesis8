# Transcription service

A standalone Whisper speech-to-text sidecar for nemesis8 agents. It keeps the
Whisper model loaded in memory and exposes a small HTTP API, so agents transcribe
audio without paying model-load cost per request.

The matching agent-side client already ships in the image as the MCP tool
**`transcribe-wav`** (`MCP/transcribe-wav.py`), which exposes `transcribe_wav()` and
`check_transcription_status()`. This service is the backend that tool talks to.

> Moved here from `hyperia/services/transcription`. It has zero coupling to either
> app — it's a generic Whisper HTTP service.

## Run it

CPU (small `base` model, no GPU needed):

```sh
cd services/transcription
docker compose up -d --build
```

NVIDIA GPU (faster, defaults to `medium`):

```sh
cd services/transcription
docker compose -f docker-compose.gpu.yml up -d --build
```

Override the model with `WHISPER_MODEL` (e.g. `tiny`, `base`, `small`, `medium`,
`large-v3`):

```sh
WHISPER_MODEL=large-v3 docker compose -f docker-compose.gpu.yml up -d --build
```

The service listens on **`0.0.0.0:8767`** and persists transcripts to the
`transcription-data` volume at `/data/transcripts`.

## How agents reach it

The `transcribe-wav` MCP tool defaults to `http://host.docker.internal:8767` — the
host-published port above, reachable from inside agent containers on Docker Desktop
(Windows/macOS) and via `--add-host` on Linux. Override per environment by setting
`TRANSCRIPTION_SERVICE_URL` in the agent (nemesis8 passes it through automatically),
or per call via the tool's `service_url` argument.

## API

| Method & path | Purpose |
|---|---|
| `POST /transcribe` | multipart upload (`file`, optional `job_id`/`model`/`callback_url`) → `{ job_id, status: "queued" }` |
| `GET /status/{job_id}` | poll job state (`queued`/`processing`/`completed`/`failed`) |
| `GET /download/{job_id}` | fetch the completed transcript (text) |
| `POST /classify` | multipart upload → coarse ambient-sound labels (traffic, wind, birds, rain, …) |
| `GET /health` | model + queue status, `gpu_available` flag |

The `/classify` endpoint pairs with the optional `dog-bark-detector` client in
`MCP/disabled/` if you want ambient-sound labelling exposed to agents.

## Notes

- No auth; binds `0.0.0.0`. Intended for a trusted localhost/LAN sidecar — don't
  expose `8767` to the public internet.
- Job queue is in-memory (lost on restart); completed transcripts persist on the
  volume.
- First run downloads the Whisper model (baked at build time via the `WHISPER_MODEL`
  build arg, so the running container starts ready).
