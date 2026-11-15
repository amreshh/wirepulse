# syntax=docker/dockerfile:1.7
#
# Builds the WASM frontend with Trunk and the Rust backend, then ships both
# in a distroless image (~30 MB). No protoc needed: protos compile with protox.

FROM rust:1-bookworm AS build
ARG TRUNK_VERSION=0.22.1
RUN rustup target add wasm32-unknown-unknown \
 && cargo install trunk --locked --version ${TRUNK_VERSION}
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cd frontend && trunk build --release \
 && cd .. && cargo build --release -p backend \
 && cp target/release/wirepulse /wirepulse

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /wirepulse /wirepulse
COPY --from=build /src/frontend/dist /dist
ENV WIREPULSE_STATIC_DIR=/dist \
    WIREPULSE_LISTEN=0.0.0.0:8080
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/wirepulse"]
