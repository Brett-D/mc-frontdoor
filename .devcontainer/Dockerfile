# Rust plus what the end-to-end test needs (Python with httpx, openssl).
FROM mcr.microsoft.com/devcontainers/rust:1-bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3-venv openssl \
 && rm -rf /var/lib/apt/lists/* \
 && python3 -m venv /opt/venv \
 && /opt/venv/bin/pip install --no-cache-dir "httpx[http2]" \
 && chown -R vscode:vscode /opt/venv
ENV PATH="/opt/venv/bin:${PATH}"
