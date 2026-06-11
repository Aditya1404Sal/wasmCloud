# syntax=docker/dockerfile:1-labs


FROM lukemathwalker/cargo-chef:latest-rust-1.96.0-alpine3.22 AS chef
USER root
WORKDIR /src

FROM chef AS planner
COPY --exclude=rust-toolchain.toml . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder

RUN apk --no-cache add protoc protobuf protobuf-dev

COPY --from=planner /src/recipe.json recipe.json
# Notice that we are specifying the --target flag!
RUN cargo chef cook --release --target x86_64-unknown-linux-musl --recipe-path recipe.json
COPY --exclude=rust-toolchain.toml --chown=nonroot:nonroot . .

# Optional comma-separated cargo feature list (e.g. "wasip3,wasi-tls").
# Empty by default so the standard image stays on WASI Preview 2.
ARG CARGO_FEATURES=""

# build static binary
RUN cargo build --release --target x86_64-unknown-linux-musl --bin wash ${CARGO_FEATURES:+--features ${CARGO_FEATURES}}

# Release image
FROM cgr.dev/chainguard/wolfi-base
RUN apk add --no-cache git
COPY --from=builder /src/target/x86_64-unknown-linux-musl/release/wash /usr/local/bin/wash
ENTRYPOINT ["/usr/local/bin/wash"]
