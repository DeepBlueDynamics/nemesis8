ARG NEMESIS8_BASE_TAG=latest
FROM kord/nemesis8-base:${NEMESIS8_BASE_TAG}

ARG TZ
ENV TZ="$TZ"

# ── Provider CLIs ────────────────────────────────────────────────
# Providers to install — comma-separated names from .nemesis8.toml
# Override at build time: docker build --build-arg INSTALL_PROVIDERS=codex,gemini
ARG INSTALL_PROVIDERS=codex,gemini,claude,openclaw
# Optional extras — e.g. "baml" (empty by default)
ARG INSTALL_EXTRAS=
# Include latest ffmpeg static build — false by default to keep image lean
# Enable with: nemesis8 build --ffmpeg  or  ffmpeg = true in .nemesis8.toml
ARG INCLUDE_FFMPEG=false

COPY scripts/install-providers.sh /tmp/install-providers.sh
RUN chmod +x /tmp/install-providers.sh \
  && /tmp/install-providers.sh "${INSTALL_PROVIDERS}" "${INSTALL_EXTRAS}" \
  && rm -f /tmp/install-providers.sh

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

# BAML workspace
RUN mkdir -p /opt/baml-workspace
ENV BAML_WORKSPACE=/opt/baml-workspace

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

# Pre-install MCP servers to /opt/mcp-installed (copied to codex-home at runtime)
RUN mkdir -p /opt/mcp-installed \
  && cp /opt/mcp-source/*.py /opt/mcp-installed/ 2>/dev/null || true \
  && cp -r /opt/mcp-source/product_search_data /opt/mcp-installed/ 2>/dev/null || true \
  && cp /opt/mcp-source/*.json /opt/mcp-installed/ 2>/dev/null || true \
  && chmod 644 /opt/mcp-installed/*.py 2>/dev/null || true

# ── nemisis8-entry binary ────────────────────────────────────────
COPY Cargo.toml /opt/nemisis8-build/Cargo.toml
COPY src/ /opt/nemisis8-build/src/
COPY providers/ /opt/nemisis8-build/providers/
RUN cd /opt/nemisis8-build \
  && . "$CARGO_HOME/env" \
  && cargo build --release --bin nemisis8-entry \
  && cp target/release/nemisis8-entry /usr/local/bin/nemisis8-entry \
  && chmod 555 /usr/local/bin/nemisis8-entry \
  && rm -rf /opt/nemisis8-build

# ── Workspace and prompt files ───────────────────────────────────
COPY providers/ /opt/defaults/providers/
COPY docs/PROMPT.md /opt/defaults/PROMPT.md
COPY examples/ /opt/defaults/examples/

# Default to root for Windows ACL compatibility
USER root
