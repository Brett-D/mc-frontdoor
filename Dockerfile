# syntax=docker/dockerfile:1
#
#   docker build -t mc-frontdoor .                          the image (default target)
#   docker build --target test .                            unit tests + end-to-end test
#   docker buildx build --platform linux/amd64,linux/arm64 --target binaries -o dist .
#                                                           static Linux binaries in dist/
#
# The build stage always runs on the build machine's own architecture and cross-compiles (zig as the linker),
# so arm64 builds need no emulation.

ARG RUST_VERSION=1

FROM --platform=$BUILDPLATFORM ghcr.io/rust-cross/cargo-zigbuild:0.24.0 AS build
ARG TARGETARCH
WORKDIR /src
RUN case "$TARGETARCH" in \
      amd64) echo x86_64 ;; \
      arm64) echo aarch64 ;; \
      *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac > /arch \
 && rustup target add "$(cat /arch)-unknown-linux-musl"
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,id=mc-frontdoor-target-$TARGETARCH \
    arch="$(cat /arch)" \
 && cargo zigbuild --release --locked --target "$arch-unknown-linux-musl" \
 && mkdir -p /out/bin /out/dist /out/data \
 && cp "target/$arch-unknown-linux-musl/release/mc-frontdoor" /out/bin/mc-frontdoor \
 && cp /out/bin/mc-frontdoor "/out/dist/mc-frontdoor-$arch-linux"

FROM scratch AS binaries
COPY --from=build /out/dist/ /

FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-bookworm AS test
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3-venv openssl \
 && rm -rf /var/lib/apt/lists/* \
 && python3 -m venv /venv \
 && /venv/bin/pip install --no-cache-dir "httpx[http2]"
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo test --locked \
 && cargo build --locked \
 && /venv/bin/python tests/e2e.py target/debug/mc-frontdoor

FROM scratch
LABEL org.opencontainers.image.source="https://github.com/Brett-D/mc-frontdoor" \
      org.opencontainers.image.description="Wake-on-join front door for Minecraft servers that sleep when nobody plays" \
      org.opencontainers.image.licenses="MIT"
COPY --from=build --chown=65534:65534 /out/data /data
COPY --from=build /out/bin/mc-frontdoor /mc-frontdoor
USER 65534:65534
ENV STATE_DIRECTORY=/data
VOLUME /data
EXPOSE 25565/tcp 19132/tcp 19132/udp
ENTRYPOINT ["/mc-frontdoor"]
