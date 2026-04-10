FROM node:24-slim

ARG TZ
ENV TZ="$TZ"
ENV TERM=xterm-256color
ENV COLORTERM=truecolor

# ── System packages ──────────────────────────────────────────────
RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    gnupg2 \
  && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
    | dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg \
  && chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg \
  && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
    > /etc/apt/sources.list.d/github-cli.list \
  && apt-get update \
  && apt-get install -y --no-install-recommends \
    bubblewrap \
    build-essential \
    ca-certificates \
    curl \
    dnsutils \
    ffmpeg \
    fzf \
    gh \
    git \
    gnupg2 \
    iproute2 \
    iputils-ping \
    iptables \
    jq \
    less \
    libssl-dev \
    pkg-config \
    procps \
    python3 \
    python3-pip \
    python3-venv \
    ripgrep \
    socat \
    tini \
    unzip \
  && rm -rf /var/lib/apt/lists/*

# Ensure `python` points to python3
RUN ln -sf /usr/bin/python3 /usr/local/bin/python

# ── Rust toolchain ───────────────────────────────────────────────
# Install to /opt/rust — NOT under /opt/codex-home which is bind-mounted
# from the host at runtime and would shadow a build-time rustup install.
ENV RUSTUP_HOME=/opt/rust/rustup
ENV CARGO_HOME=/opt/rust/cargo
RUN mkdir -p "$RUSTUP_HOME" "$CARGO_HOME" \
  && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
  && . "$CARGO_HOME/env" \
  && rustup default stable
ENV PATH="$CARGO_HOME/bin:${PATH}"

# ── Node / npm / Codex CLI ──────────────────────────────────────
RUN mkdir -p /usr/local/share/npm-global \
  && chown -R node:node /usr/local/share

ENV NPM_CONFIG_PREFIX=/usr/local/share/npm-global
ENV PATH="${PATH}:/usr/local/share/npm-global/bin"

# Cache-bust: changes every build to force fresh npm installs
ARG CACHE_BUST=1
RUN npm install -g "@openai/codex@latest" \
  && npm cache clean --force \
  && rm -rf /usr/local/share/npm-global/lib/node_modules/codex-cli/node_modules/.cache

# ── Gemini CLI ────────────────────────────────────────────────
RUN npm install -g @google/gemini-cli \
  && npm cache clean --force

# ── BAML CLI ─────────────────────────────────────────────────
RUN npm install -g @boundaryml/baml@latest \
  && npm cache clean --force

# ── Claude Code CLI ──────────────────────────────────────────────
RUN npm install -g @anthropic-ai/claude-code@latest \
  && npm cache clean --force

# ── OpenClaw CLI ─────────────────────────────────────────────────
RUN npm install -g openclaw@latest \
  && npm cache clean --force

# ── Qwen Code CLI ───────────────────────────────────────────────
RUN npm install -g @qwen-code/qwen-code@latest \
  && npm cache clean --force

# ── Python MCP venv ──────────────────────────────────────────────
COPY requirements.txt /opt/mcp-requirements/requirements.txt
ENV MCP_VENV=/opt/mcp-venv
RUN python3 -m venv "$MCP_VENV" \
  && "$MCP_VENV/bin/pip" install --no-cache-dir --upgrade pip \
  && "$MCP_VENV/bin/pip" install --no-cache-dir -r /opt/mcp-requirements/requirements.txt
ENV PATH="$MCP_VENV/bin:$PATH"
ENV VIRTUAL_ENV="$MCP_VENV"

# BAML workspace
RUN mkdir -p /opt/baml-workspace
ENV BAML_WORKSPACE=/opt/baml-workspace

# Login helper script for OAuth callback bridging
COPY scripts/codex_login.sh /usr/local/bin/codex_login.sh
RUN chmod 555 /usr/local/bin/codex_login.sh

# Container is already sandboxed — allow Codex to run without extra sandbox
ENV CODEX_UNSAFE_ALLOW_NO_SANDBOX=1

# ── MCP source and data ─────────────────────────────────────────
COPY MCP/ /opt/mcp-source/

# Pre-install MCP servers to /opt/mcp-installed (copied to codex-home at runtime)
RUN mkdir -p /opt/mcp-installed \
  && cp /opt/mcp-source/*.py /opt/mcp-installed/ 2>/dev/null || true \
  && cp -r /opt/mcp-source/product_search_data /opt/mcp-installed/ 2>/dev/null || true \
  && chmod 644 /opt/mcp-installed/*.py 2>/dev/null || true

# ── nemisis8-entry binary ────────────────────────────────────────
# Cross-compile on host and copy in, OR build inside Docker:
# For now, we copy a pre-built binary. Build with:
#   cross build --release --target x86_64-unknown-linux-gnu --bin nemisis8-entry
# Or uncomment the build stage below.
#
# COPY target/x86_64-unknown-linux-gnu/release/nemisis8-entry /usr/local/bin/nemisis8-entry
# RUN chmod 555 /usr/local/bin/nemisis8-entry

# Fallback: build nemisis8-entry inside Docker (slower but always works)
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
COPY PROMPT.md /opt/defaults/PROMPT.md
COPY examples/ /opt/defaults/examples/

# Default to root for Windows ACL compatibility
USER root
