# syntax=docker/dockerfile:1.7
#
# Multi-stage build:
#   1. chef    – Rust toolchain + wasm and musl targets, musl-gcc, Trunk and
#                cargo-chef
#   2. planner – turns the Cargo manifests into a dependency "recipe"
#   3. build   – compiles dependencies from the recipe (a cached layer that only
#                changes when dependencies do), then the Leptos frontend to WASM
#                with Trunk and the axum backend as a static (musl) binary
#   4. runtime – distroless static (CA certificates, a nonroot user, no shell or
#                C library) with just the binary and the frontend (~18 MB)
#
#   docker build -t wirepulse:latest --build-arg WIREPULSE_COMMIT="$(git rev-parse HEAD)" .
#   docker run --rm -p 8080:8080 wirepulse:latest --demo
#
# No protoc needed: the Hubble protos compile with protox (pure Rust).
# Base images are pinned by digest and written out in full (not through ARGs)
# so Dependabot can update them.

ARG TRUNK_VERSION=0.22.1
# SHA256 of TRUNK_VERSION's release archives (shown on its GitHub release, and
# in the .sha256 files next to the archives). Trunk builds the frontend that
# goes into the image, so it only runs if its archive matches. Change these
# together with TRUNK_VERSION.
ARG TRUNK_SHA256_X86_64=2ab8d548fd5ed9cda50906f61fd8654204e7893bd52ebbfd90533862b7d71ee1
ARG TRUNK_SHA256_AARCH64=b4017d1e0bfcffb4f4bee2be37c48def21e6e6896c3d0f503c43c42f714e57ea
ARG CHEF_VERSION=0.1.78

FROM rust:1.99-bookworm@sha256:e6f2cff35d0f309acf8433b4f05878ed77092602c5065cc81801177da0bb3a9c AS chef
ARG TRUNK_VERSION
ARG TRUNK_SHA256_X86_64
ARG TRUNK_SHA256_AARCH64
ARG CHEF_VERSION
ARG TARGETARCH
# A piped command (echo | sha256sum) fails when any part of it fails.
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
WORKDIR /src

# musl-gcc compiles the C code among the backend's dependencies (ring,
# mimalloc) for musl. Not pinned to a version: Debian drops a package's old
# version when it ships an update, so a pinned one would stop installing.
# hadolint ignore=DL3008
RUN apt-get update \
 && apt-get install -y --no-install-recommends musl-tools \
 && rm -rf /var/lib/apt/lists/*
ENV CC_x86_64_unknown_linux_musl=musl-gcc \
    CC_aarch64_unknown_linux_musl=musl-gcc

# The backend's target (x86_64-unknown-linux-musl, …) goes in /rust-target for
# the build stage.
RUN case "${TARGETARCH:-amd64}" in \
      amd64) arch=x86_64 sha=${TRUNK_SHA256_X86_64} ;; \
      arm64) arch=aarch64 sha=${TRUNK_SHA256_AARCH64} ;; \
      *) echo "unsupported arch ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
 && echo "${arch}-unknown-linux-musl" > /rust-target \
 && rustup target add wasm32-unknown-unknown "${arch}-unknown-linux-musl" \
 && curl -fsSL -o /tmp/trunk.tar.gz \
      "https://github.com/trunk-rs/trunk/releases/download/v${TRUNK_VERSION}/trunk-${arch}-unknown-linux-gnu.tar.gz" \
 && echo "${sha}  /tmp/trunk.tar.gz" | sha256sum -c - \
 && tar -xzf /tmp/trunk.tar.gz -C /usr/local/bin trunk \
 && rm /tmp/trunk.tar.gz \
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
RUN cargo chef cook --release --locked --recipe-path recipe.json -p backend --target "$(cat /rust-target)" \
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
RUN target=$(cat /rust-target) \
 && cargo build --release --locked -p backend --target "$target" \
 && mkdir -p /out \
 && cp "target/$target/release/wirepulse" /out/wirepulse \
 && cp -r frontend/dist /out/dist

# A static binary needs no C library. Distroless static adds CA certificates
# for TLS, time zone data and the nonroot user, and has no shell.
FROM gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab AS runtime
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
# The nonroot user, by number: Kubernetes can only check that a numeric user
# isn't root (runAsNonRoot), and won't start a container whose user is a name.
USER 65532:65532
ENTRYPOINT ["/wirepulse"]
