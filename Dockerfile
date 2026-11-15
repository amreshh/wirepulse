# syntax=docker/dockerfile:1.7
#
# Multi-stage build:
#   1. chef    – Rust toolchain + wasm target, Trunk and cargo-chef
#   2. planner – turns the Cargo manifests into a dependency "recipe"
#   3. build   – compiles dependencies from the recipe (a cached layer that only
#                changes when dependencies do), then the Leptos frontend to WASM
#                with Trunk and the axum backend
#   4. runtime – distroless (glibc, CA certificates, a nonroot user, no shell)
#                with just the binary and the frontend (~30 MB)
#
#   docker build -t wirepulse:latest --build-arg WIREPULSE_COMMIT="$(git rev-parse HEAD)" .
#   docker run --rm -p 8080:8080 wirepulse:latest --demo
#
# No protoc needed: the Hubble protos compile with protox (pure Rust).
# Base images are pinned by digest and written out in full (not through ARGs)
# so Dependabot can update them.

ARG TRUNK_VERSION=0.22.1
ARG CHEF_VERSION=0.1.78

FROM rust:1.99-bookworm@sha256:e6f2cff35d0f309acf8433b4f05878ed77092602c5065cc81801177da0bb3a9c AS chef
ARG TRUNK_VERSION
ARG CHEF_VERSION
ARG TARGETARCH
# A piped command (curl | tar) fails when any part of it fails.
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
WORKDIR /src

RUN case "${TARGETARCH:-amd64}" in \
      amd64) arch=x86_64 ;; \
      arm64) arch=aarch64 ;; \
      *) echo "unsupported arch ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
 && rustup target add wasm32-unknown-unknown \
 && curl -fsSL "https://github.com/trunk-rs/trunk/releases/download/v${TRUNK_VERSION}/trunk-${arch}-unknown-linux-gnu.tar.gz" \
    | tar -xz -C /usr/local/bin trunk \
 && trunk --version \
 && cargo install cargo-chef --locked --version "${CHEF_VERSION}" \
 && rm -rf /usr/local/cargo/registry

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY common common
COPY backend backend
COPY frontend frontend
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
# Dependencies only. The recipe is identical until a Cargo.toml or Cargo.lock
# changes, so in CI this layer comes straight from the registry cache. The
# commands must match the real builds below (profile, target, packages) or the
# cooked dependencies won't be reused.
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json -p backend \
 && cargo chef cook --release --locked --recipe-path recipe.json -p frontend \
      --target wasm32-unknown-unknown

COPY Cargo.toml Cargo.lock ./
COPY common common
COPY backend backend
COPY frontend frontend

# The Trunk cache mount keeps wasm-bindgen and wasm-opt between local builds.
WORKDIR /src/frontend
RUN --mount=type=cache,target=/root/.cache/trunk trunk build --release
WORKDIR /src
RUN cargo build --release --locked -p backend \
 && mkdir -p /out \
 && cp target/release/wirepulse /out/wirepulse \
 && cp -r frontend/dist /out/dist

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f AS runtime
COPY --from=build /out/wirepulse /wirepulse
COPY --from=build /out/dist /dist
ENV WIREPULSE_STATIC_DIR=/dist \
    WIREPULSE_LISTEN=0.0.0.0:8080

# Which build this is, for /api/info: the git commit and, for a release, its
# version (e.g. 1.2.3). Settings, not part of the binary, so a new commit
# doesn't rebuild anything, and a release of an image that's already built
# can set its version (see .github/release/Dockerfile).
ARG WIREPULSE_COMMIT=""
ARG WIREPULSE_VERSION=""
ENV WIREPULSE_COMMIT=${WIREPULSE_COMMIT} \
    WIREPULSE_VERSION=${WIREPULSE_VERSION}

EXPOSE 8080
USER nonroot
ENTRYPOINT ["/wirepulse"]
