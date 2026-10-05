# syntax=docker/dockerfile:1.4

# Build on the native builder platform and cross-compile to the target arch
# (pure-Go, CGO disabled) so arm64 builds avoid slow QEMU emulation.
FROM --platform=$BUILDPLATFORM golang:1.27.1 AS builder
WORKDIR /workspace/runtime-gateway

# buildx injects these for the requested --platform target(s).
ARG TARGETOS
ARG TARGETARCH

# dependencies cache
COPY go.mod go.sum ./
RUN go mod download

# source code
COPY . .
RUN CGO_ENABLED=0 GOOS=${TARGETOS} GOARCH=${TARGETARCH} go build -o /go/bin/runtime-gateway ./

FROM cgr.dev/chainguard/static:latest AS release
COPY --from=builder /go/bin/runtime-gateway /usr/local/bin/runtime-gateway
ENTRYPOINT ["/usr/local/bin/runtime-gateway"]
