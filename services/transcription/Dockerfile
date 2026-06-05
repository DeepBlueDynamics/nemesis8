FROM python:3.11-slim

RUN apt-get update && apt-get install -y ffmpeg && rm -rf /var/lib/apt/lists/*

RUN pip install --no-cache-dir openai-whisper aiohttp soundfile numpy

ENV WHISPER_MODEL=base
ENV STORAGE_DIR=/data

RUN mkdir -p /data/pending /data/transcripts

# Pre-download model at build time
ARG WHISPER_MODEL=base
RUN python3 -c "import whisper; whisper.load_model('${WHISPER_MODEL}')"

COPY transcription_service.py /app/transcription_service.py
WORKDIR /app

EXPOSE 8767

HEALTHCHECK --interval=30s --timeout=10s --start-period=60s --retries=3 \
    CMD python3 -c "import urllib.request; urllib.request.urlopen('http://localhost:8767/health')" || exit 1

CMD ["python3", "transcription_service.py"]
