ARG NEMESIS8_BASE_TAG=latest

# ── Build stage: compile nemesis8-entry ──────────────────────────────
FROM deepbluedynamics/nemesis8-base:${NEMESIS8_BASE_TAG} AS builder

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
# Vendored path dependency (FST tagger + BM25 used by session search). Must be
# present before `cargo build` or manifest resolution fails with
# "failed to read /opt/nemesis8-build/lume/Cargo.toml".
COPY lume/ /opt/nemesis8-build/lume/
RUN cd /opt/nemesis8-build \
  && cargo build --release --bin nemesis8-entry \
  && cargo build --release --bin nemesis8-monitor

# ── Runtime image ────────────────────────────────────────────────────
FROM deepbluedynamics/nemesis8-base:${NEMESIS8_BASE_TAG}

ARG TZ
ENV TZ="$TZ"

# ── Provider CLIs ────────────────────────────────────────────────
# Providers to install — comma-separated names from .nemesis8.toml
# Override at build time: docker build --build-arg INSTALL_PROVIDERS=codex,gemini
ARG INSTALL_PROVIDERS=codex,gemini,claude,openclaw,antigravity
# Include latest ffmpeg static build — false by default to keep image lean
# Enable with: nemesis8 build --ffmpeg  or  ffmpeg = true in .nemesis8.toml
ARG INCLUDE_FFMPEG=false

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

# Login helper script for OAuth callback bridging
COPY scripts/codex_login.sh /usr/local/bin/codex_login.sh
RUN chmod 555 /usr/local/bin/codex_login.sh

# Container is already sandboxed — allow Codex to run without extra sandbox
ENV CODEX_UNSAFE_ALLOW_NO_SANDBOX=1

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

# ── Workspace and prompt files ───────────────────────────────────
# providers/ already copied earlier (used by the install step).
COPY docs/PROMPT.md /opt/defaults/PROMPT.md
COPY examples/ /opt/defaults/examples/

# Run as the non-root `node` user (UID 1000) from the base image — some agents
# (notably Codex) refuse to operate as root (issue #8). npm-global is already
# node-owned; /opt/rust is world-readable; /opt/nemesis8 is the runtime bind
# mount (must be writable by UID 1000 on the host). Give node the small dirs it
# reads/copies from. NOTE: runtime pip-into-venv (`n8 mcp add --requires`) would
# still need /opt/mcp-venv chowned in the base image — deferred.
RUN chown -R node:node \
  /opt/mcp-installed \
  /opt/mcp-source \
  /opt/defaults \
  /opt/pokeballs
USER node
