#!/bin/sh
# Run build.sh in a Debian container, so the build needs only Docker.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
docker run --rm -v "$here":/work -w /work debian:bookworm-slim sh -c '
    dpkg --add-architecture arm64 &&
    apt-get update -qq >/dev/null &&
    apt-get install -y -qq --no-install-recommends \
        ca-certificates curl xz-utils zstd cpio fdisk dosfstools mtools >/dev/null &&
    ./build.sh "$@"' sh "$@"
