ARG NEMESIS8_BASE_TAG=latest

# ── Build stage: compile nemesis8-entry ──────────────────────────────
FROM docker.io/deepbluedynamics/nemesis8-base:${NEMESIS8_BASE_TAG} AS builder

# The base image is slimmed (no build-essential / libssl-dev / pkg-config
# after the pip venv is built). Install them here in the builder stage so
# rustc has cc/ld and -sys crates can find OpenSSL. This layer is discarded
# — only the compiled binary makes it to the runtime image below.
RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    build-essential \
    libssl-dev \
    pkg-config \
  && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/opt/rust/rustup \
    CARGO_HOME=/opt/rust/cargo
RUN mkdir -p "$RUSTUP_HOME" "$CARGO_HOME" \
  && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
  && . "$CARGO_HOME/env" \
  && rustup default stable
ENV PATH="$CARGO_HOME/bin:${PATH}"
COPY Cargo.toml Cargo.lock build.rs /opt/nemesis8-build/
COPY src/ /opt/nemesis8-build/src/
COPY providers/ /opt/nemesis8-build/providers/
# The embedded agent BASE system prompt — src/config.rs does
# include_str!("../prompts/BASE.md") at compile time, so it must be in the
# build context or `cargo build` fails "couldn't read .../prompts/BASE.md".
COPY prompts/ /opt/nemesis8-build/prompts/
# Same deal for the embedded Troubleshooting script (config.rs include_str!s it).
COPY scripts/antigravity_wipe.sh /opt/nemesis8-build/scripts/antigravity_wipe.sh
# Vendored path dependency (FST tagger + BM25 used by session search). Must be
# present before `cargo build` or manifest resolution fails with
# "failed to read /opt/nemesis8-build/lume/Cargo.toml".
COPY lume/ /opt/nemesis8-build/lume/
# nuts-files: a self-contained Rust MCP server (its own workspace) that path-deps
# aegis-edit. Layout must match `../../aegis-edit` from tools/nuts-files.
COPY aegis-edit/ /opt/nemesis8-build/aegis-edit/
COPY tools/ /opt/nemesis8-build/tools/
RUN cd /opt/nemesis8-build \
  && cargo build --release --bin nemesis8-entry \
  && cargo build --release --bin nemesis8-monitor \
  && cd /opt/nemesis8-build/tools/nuts-files \
  && cargo build --release --locked \
  && cd /opt/nemesis8-build/tools/shivvr \
  && cargo build --release --locked \
  && cd /opt/nemesis8-build/tools/ask-rs \
  && cargo build --release --locked \
  && cd /opt/nemesis8-build/tools/n8gw \
  && cargo build --release --locked

# ── Runtime image ────────────────────────────────────────────────────
FROM docker.io/deepbluedynamics/nemesis8-base:${NEMESIS8_BASE_TAG}

ARG TZ
ENV TZ="$TZ"

# ── Provider CLIs ────────────────────────────────────────────────
# Providers to install — comma-separated names from .nemesis8.toml
# Override at build time: docker build --build-arg INSTALL_PROVIDERS=codex,gemini
ARG INSTALL_PROVIDERS=codex,claude,antigravity,grok,pi,opencode,hermes
# Include latest ffmpeg static build — false by default to keep image lean
# Enable with: nemesis8 build --ffmpeg  or  ffmpeg = true in .nemesis8.toml
ARG INCLUDE_FFMPEG=false
# Bake NVIDIA GPU support — false by default. Enable with: nemesis8 build --gpu
# (adds the CUDA runtime + cuDNN, ~1.2 GB). Run with `n8 --gpu` (docker --gpus all).
ARG INCLUDE_GPU=false
# C/C++ build toolchain so AGENTS can compile native code (cargo build, C,
# node-gyp, Python C extensions) — false by default (the runtime image is slim,
# #17). Enable with: nemesis8 build --native. Without it, `cargo check` works but
# linking fails with "cc not found".
ARG INCLUDE_NATIVE=false
ARG CHISEL_VERSION=1.11.5

# Provider TOMLs live at /opt/defaults/providers in both the final image
# and here in the install layer — the installer reads them as its data
# source so each provider's install method (npm / curl / host) is defined
# in providers/<name>.toml under [provider.install], not hardcoded in this
# Dockerfile or the install script.
COPY providers/ /opt/defaults/providers/
COPY scripts/install-providers.py /tmp/install-providers.py
RUN python3 /tmp/install-providers.py "${INSTALL_PROVIDERS}" \
  && rm -f /tmp/install-providers.py

# ── Optional: latest ffmpeg static build ─────────────────────────
# Skipped by default; enable with nemesis8 build --ffmpeg
RUN if [ "$INCLUDE_FFMPEG" = "true" ]; then \
    RELEASE_JSON=$(curl -fsSL https://api.github.com/repos/BtbN/FFmpeg-Builds/releases/latest) \
    && FFMPEG_URL=$(echo "$RELEASE_JSON" \
        | grep '"browser_download_url"' \
        | grep 'ffmpeg-master-latest-linux64-gpl\.tar\.xz"' \
        | head -1 \
        | sed 's/.*"browser_download_url": "\(.*\)"/\1/') \
    && echo "[ffmpeg] downloading $FFMPEG_URL" \
    && curl -fsSL "$FFMPEG_URL" -o /tmp/ffmpeg.tar.xz \
    && tar xf /tmp/ffmpeg.tar.xz -C /tmp \
    && mv /tmp/ffmpeg-master-latest-linux64-gpl/bin/ffmpeg /usr/local/bin/ffmpeg \
    && mv /tmp/ffmpeg-master-latest-linux64-gpl/bin/ffprobe /usr/local/bin/ffprobe \
    && chmod 755 /usr/local/bin/ffmpeg /usr/local/bin/ffprobe \
    && rm -rf /tmp/ffmpeg* \
    && ffmpeg -version | head -1; \
  fi

# ── Optional: native build toolchain (so agents can COMPILE code) ──
# Skipped by default to keep the runtime image slim; enable with
# `nemesis8 build --native`. gcc/g++/make + libc headers (cc + crt for linking),
# pkg-config and libssl-dev for the common Rust *-sys crates.
RUN if [ "$INCLUDE_NATIVE" = "true" ]; then \
      echo "[native] installing C/C++ build toolchain" \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
         build-essential pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/* \
    && cc --version | head -1; \
  fi

# ── Optional: NVIDIA GPU support (CUDA runtime + cuDNN) ───────────
# Skipped by default; enable with: nemesis8 build --gpu
# These NVIDIA_* envs are honored only when the container is run with
# `docker --gpus all` (n8 --gpu) on a host with nvidia-container-toolkit; they
# are inert otherwise, so they're safe to set unconditionally. The CUDA runtime
# libs are pip-installed into the MCP venv and symlinked onto the linker path so
# GPU frameworks (torch, faster-whisper, …) the agent installs can find them.
ENV NVIDIA_VISIBLE_DEVICES=all
ENV NVIDIA_DRIVER_CAPABILITIES=compute,utility
RUN if [ "$INCLUDE_GPU" = "true" ]; then \
      echo "[gpu] installing CUDA runtime + cuDNN (cu12) into the MCP venv" \
    && /opt/mcp-venv/bin/pip install --no-cache-dir nvidia-cuda-runtime-cu12 nvidia-cudnn-cu12 \
    && SP=$(/opt/mcp-venv/bin/python3 -c "import site; print(site.getsitepackages()[0])") \
    && find "$SP/nvidia" -name '*.so*' -exec ln -sf {} /usr/local/lib/ \; \
    && ldconfig \
    && echo "[gpu] CUDA runtime linked onto the loader path"; \
  fi
# Image GPU-capability marker — n8 --gpu reads this to decide whether to pass
# --gpus all or warn that the image needs rebuilding with --gpu.
LABEL nemesis8.gpu="${INCLUDE_GPU}"

# Reverse port exposure data plane. Chisel is a single static binary; the host
# gateway runs the reverse server, and containers run the client via docker exec.
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
      amd64) chisel_arch="amd64" ;; \
      arm64) chisel_arch="arm64" ;; \
      armhf) chisel_arch="armv7" ;; \
      *) echo "unsupported chisel arch: $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://github.com/jpillora/chisel/releases/download/v${CHISEL_VERSION}/chisel_${CHISEL_VERSION}_linux_${chisel_arch}.gz" -o /tmp/chisel.gz; \
    gunzip /tmp/chisel.gz; \
    install -m 0555 /tmp/chisel /usr/local/bin/chisel; \
    rm -f /tmp/chisel; \
    chisel --version

# Login helper script for OAuth callback bridging
COPY scripts/codex_login.sh /usr/local/bin/codex_login.sh
RUN chmod 555 /usr/local/bin/codex_login.sh

# The container IS the sandbox — let agents run their danger/yolo modes as root
# without complaint. We run as root because non-root (issue #8's USER node) broke
# the /opt/nemesis8 bind mount on Windows/macOS Docker Desktop. Each agent family
# has its own escape hatch:
#   CODEX_UNSAFE_ALLOW_NO_SANDBOX — Codex
#   IS_SANDBOX — Claude Code, so `--permission-mode bypassPermissions` /
#                `--dangerously-skip-permissions` work as root (also antigravity)
ENV CODEX_UNSAFE_ALLOW_NO_SANDBOX=1
ENV IS_SANDBOX=1

# Cache-bust: injected by nemesis8 to force refresh of MCP tools and Rust build
# Placed here so provider layers above stay cached across normal builds
ARG CACHE_BUST=1

# ── MCP source and data ─────────────────────────────────────────
COPY MCP/ /opt/mcp-source/

# Community pokeballs catalog (read-only inside the container)
COPY pokeballs/ /opt/pokeballs/

# Pre-install MCP servers to /opt/mcp-installed (copied to /opt/nemesis8 at runtime)
RUN mkdir -p /opt/mcp-installed \
  && cp /opt/mcp-source/*.py /opt/mcp-installed/ 2>/dev/null || true \
  && cp -r /opt/mcp-source/product_search_data /opt/mcp-installed/ 2>/dev/null || true \
  && cp /opt/mcp-source/*.json /opt/mcp-installed/ 2>/dev/null || true \
  && chmod 644 /opt/mcp-installed/*.py 2>/dev/null || true

# ── nemesis8-entry binary ────────────────────────────────────────
COPY --from=builder /opt/nemesis8-build/target/release/nemesis8-entry /usr/local/bin/nemesis8-entry
RUN chmod 555 /usr/local/bin/nemesis8-entry

# ── nemesis8-monitor binary (telemetry daemon) ──────────────────
COPY --from=builder /opt/nemesis8-build/target/release/nemesis8-monitor /usr/local/bin/nemesis8-monitor
RUN chmod 555 /usr/local/bin/nemesis8-monitor

# ── nuts-files binary (MCP file tool: read/write/edit/search/diff) ──
COPY --from=builder /opt/nemesis8-build/tools/nuts-files/target/release/nuts-files /usr/local/bin/nuts-files
RUN chmod 555 /usr/local/bin/nuts-files

# ── shivvr binary (MCP embeddings client: embed / similarity / status) ──
COPY --from=builder /opt/nemesis8-build/tools/shivvr/target/release/shivvr /usr/local/bin/shivvr
RUN chmod 555 /usr/local/bin/shivvr

# ── ask binary (MCP second-opinion: Claude/Gemini/OpenAI, replaces ask.py) ──
COPY --from=builder /opt/nemesis8-build/tools/ask-rs/target/release/ask /usr/local/bin/ask
RUN chmod 555 /usr/local/bin/ask

# ── n8gw binary (MCP client for the nemesis8 gateway/control-plane) ──
COPY --from=builder /opt/nemesis8-build/tools/n8gw/target/release/n8gw /usr/local/bin/n8gw
RUN chmod 555 /usr/local/bin/n8gw

# ── Workspace and prompt files ───────────────────────────────────
# providers/ already copied earlier (used by the install step).
# Service templates (n8 spawns dependency services from these) — mirrors the
# /opt/defaults/providers layout the registry reads at runtime.
COPY services/ /opt/defaults/services/
# Socket-MCP server registry (HTTP/SSE) — mirrors the providers/services layout
# the registries read at runtime; user overrides live in ~/.nemesis8/mcp.
# Repo dir is `mcp-servers` (not `mcp`) to dodge the Windows MCP/ case clash.
COPY mcp-servers/ /opt/defaults/mcp/
# (The system prompt is now embedded in the binary — prompts/BASE.md via
# include_str! + per-provider persona — so there's no PROMPT.md to bake in.)
COPY examples/ /opt/defaults/examples/

# Default to root. Issue #8 tried `USER node` (some agents dislike root), but on
# Windows/macOS Docker Desktop the /opt/nemesis8 bind mount doesn't map Unix
# ownership cleanly, so a non-root agent can't read/write configs + the npm
# cache written by earlier (root) runs → Permission denied / EACCES. Codex runs
# fine as root here because CODEX_UNSAFE_ALLOW_NO_SANDBOX=1 is set above. Proper
# non-root needs a root entrypoint that chowns the volume then drops to node —
# tracked in #8; not a blanket USER node.
USER root

# PID 1 = tini, so the agent process (nemesis8-entry, a login shell, etc.) runs
# as its child. tini reaps the zombies that the keyring daemon, monitor, MCP
# servers, and forking provider CLIs leave behind in long sessions, and forwards
# signals (-g = to the whole process group). The container command is passed as
# args to this entrypoint by every launch path (run_it, run_capture, login), so
# all of them get a proper init.
ENTRYPOINT ["/usr/bin/tini", "-g", "--"]
