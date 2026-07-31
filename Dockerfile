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
ARG TARGETARCH
# The caches keep the registry and target dir between builds, so a source change
# does not recompile every dependency. A later RUN cannot see the cached target
# dir, so the binaries are copied out in the same step.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=shared \
    --mount=type=cache,id=cargo-target-${TARGETARCH},target=/src/target,sharing=locked \
    cargo build \
        --release \
        --locked \
        --package radicle-artifact \
        --package radicle-artifact-node && \
    mkdir -p /out/ && \
    cp -v target/release/rad-artifact target/release/rad-artifact-node /out/


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
