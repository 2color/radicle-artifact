# syntax=docker/dockerfile:1
# Builds runtime images for radicle-artifact
# For simple non-split builds, --build-arg=TARGET= determines which image is output
# rad-artifact-full includes all binaries and some runtime utils handy for troubleshooting
ARG TARGET=rad-artifact
# ARG TARGET=rad-artifact-node
# ARG TARGET=rad-artifact-full

ARG ALPINE_VERSION="3.23"
ARG RUST_VERSION="1.95"
ARG BUILD_IMAGE=docker.io/rust:${RUST_VERSION}-alpine${ALPINE_VERSION}

FROM ${BUILD_IMAGE} AS builder
WORKDIR /src
COPY . .
# perform single-architecture build.
# TARGETARCH automatically set by buildx.
ARG TARGETARCH=amd64
# map Docker arch names (amd64/arm64) to a Rust musl triple
RUN cargo build \
        --release \
        --locked \
        --package radicle-artifact \
        --package radicle-artifact-node \
        --target "$(echo "${TARGETARCH}" | sed -e s/arm64/aarch64/ -e s/amd64/x86_64/)-unknown-linux-musl"
RUN mkdir -p /out/ && \
  find target \(  \
      -path '*/release/rad-artifact-node' -o \
      -path '*/release/rad-artifact'  \
    \) -exec mv -v {} /out/ \;


# shared runtime base: unprivileged user, home and working dir
FROM docker.io/alpine:${ALPINE_VERSION} AS base
ARG UID=65534
ARG GID=65534
RUN mkdir -p /opt/radicle && chown ${UID}:${GID} /opt/radicle
USER ${UID}:${GID}
ENV HOME=/opt/radicle
WORKDIR /opt/radicle


FROM base AS rad-artifact
COPY --from=builder /out/rad-artifact /usr/local/bin/
ENTRYPOINT ["/usr/local/bin/rad-artifact"]


FROM base AS rad-artifact-node
COPY --from=builder /out/rad-artifact-node /usr/local/bin/
ENTRYPOINT ["/usr/local/bin/rad-artifact-node"]


FROM rad-artifact-node AS rad-artifact-full
USER root
ARG EXTRA_PACKAGES="bash curl git grep jq netcat-openbsd psmisc sed sqlite strace tar xz"
RUN apk --no-cache add ${EXTRA_PACKAGES}
COPY --from=builder /out/rad-artifact /usr/local/bin/
ARG UID=65534
ARG GID=65534
USER ${UID}:${GID}
ENTRYPOINT ["/bin/bash"]


FROM ${TARGET}
