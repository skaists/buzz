# syntax=docker/dockerfile:1.7
#
# Build a MinIO server or `mc` client image from a pinned upstream commit.
#
# Upstream no longer serves the prebuilt images CI pinned by digest (quay.io,
# Docker Hub and mirror.gcr.io all refuse them; see PR #9), so CI builds the
# same releases from source. Every input is pinned by content:
#   - source: an exact upstream commit (the workflow checks it is the commit
#     the release tag points at);
#   - toolchain and runtime base images: pinned by digest below;
#   - Go modules: pinned by the upstream go.sum (-mod=readonly).
# No package manager runs anywhere, and the final stage only copies the
# cross-compiled binary, so multi-arch builds need no emulation.
#
# Build args (from .github/ci-images.json):
#   REPO         upstream repo, e.g. minio/minio or minio/mc
#   COMMIT       full commit SHA the release tag points at
#   RELEASE_TAG  e.g. RELEASE.2025-09-07T16-13-09Z
#   BIN          binary name: minio or mc

ARG GO_IMAGE=golang:1.24-alpine@sha256:8bee1901f1e530bfb4a7850aa7a479d17ae3a18beb6e09064ed54cfd245b7191
ARG RUNTIME_IMAGE=alpine:3.22@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8

FROM --platform=$BUILDPLATFORM ${GO_IMAGE} AS build
ARG REPO
ARG COMMIT
ARG RELEASE_TAG
ARG BIN
ARG TARGETOS
ARG TARGETARCH
ADD --keep-git-dir=false "https://github.com/${REPO}.git#${COMMIT}" /src
WORKDIR /src
# Same flags as upstream's `make build` (buildscripts/gen-ldflags.go), with the
# version fields taken from the pinned tag and commit instead of git history.
RUN version="$(echo "${RELEASE_TAG#RELEASE.}" | sed -E 's/T([0-9]{2})-([0-9]{2})-([0-9]{2})Z$/T\1:\2:\3Z/')" \
 && year="$(echo "${RELEASE_TAG#RELEASE.}" | cut -c1-4)" \
 && CGO_ENABLED=0 GOTOOLCHAIN=local GOFLAGS=-mod=readonly GOOS="${TARGETOS}" GOARCH="${TARGETARCH}" \
    go build -tags kqueue -trimpath \
      -ldflags "-s -w -X github.com/${REPO}/cmd.Version=${version} -X github.com/${REPO}/cmd.CopyrightYear=${year} -X github.com/${REPO}/cmd.ReleaseTag=${RELEASE_TAG} -X github.com/${REPO}/cmd.CommitID=${COMMIT} -X github.com/${REPO}/cmd.ShortCommitID=$(echo "${COMMIT}" | cut -c1-12)" \
      -o "/out/${BIN}" .

FROM ${RUNTIME_IMAGE}
ARG BIN
ARG REPO
ARG COMMIT
ARG RELEASE_TAG
COPY --from=build "/out/${BIN}" "/usr/bin/${BIN}"
ENV ENTRY_BIN="/usr/bin/${BIN}"
LABEL org.opencontainers.image.source="https://github.com/${REPO}" \
      org.opencontainers.image.revision="${COMMIT}" \
      org.opencontainers.image.version="${RELEASE_TAG}"
# `exec` keeps the binary as PID 1 so it receives signals directly.
ENTRYPOINT ["/bin/sh", "-c", "exec \"$ENTRY_BIN\" \"$@\"", "entrypoint"]
