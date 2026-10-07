#!/bin/sh
# Builds the container bundle: rmux, the btrfs tools marsh-setup.sh needs, their non-libc
# libraries, and the container scripts.
#
#   bench/harbor/build-bundle.sh OUT_DIR
#
# The build runs in Debian bullseye so the binaries need only glibc 2.31 and run in the
# glibc-based images Terminal-Bench and SWE-bench use (not musl/Alpine images).
set -eu

OUT=${1:?usage: build-bundle.sh OUT_DIR}
HERE=$(cd "$(dirname "$0")" && pwd -P)
REPO=$(cd "$HERE/../.." && pwd -P)
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd -P)

docker run --rm \
    -v "$REPO:/src:ro" \
    -v "$OUT:/out" \
    -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    debian:bullseye sh -euc "
        apt-get update -qq
        DEBIAN_FRONTEND=noninteractive apt-get install -qq -y --no-install-recommends \
            build-essential ca-certificates clang curl git libclang-dev \
            libbtrfsutil-dev btrfs-progs >/dev/null
        curl -sSf https://sh.rustup.rs | sh -s -- -q -y --profile minimal --default-toolchain 1.95
        . \$HOME/.cargo/env
        export CARGO_TARGET_DIR=/tmp/target CARGO_NET_GIT_FETCH_WITH_CLI=true
        export RUSTFLAGS='-C link-args=-Wl,-rpath,\$ORIGIN/../lib'
        cp -a /src /work
        cd /work
        cargo build -q --release -p marsh --bin rmux
        mkdir -p /out/bin /out/lib /out/libexec
        install -m 0755 /tmp/target/release/rmux /out/bin/rmux
        for tool in btrfs mkfs.btrfs; do
            path=\$(command -v \$tool)
            install -m 0755 \$path /out/bin/\$tool
        done
        for binary in /out/bin/*; do
            ldd \$binary | awk '/=> \\// { print \$3 }'
        done | sort -u | grep -Ev '/(libc|libm|libdl|libpthread|librt|ld-linux[^/]*)\\.so' \
            | xargs -r -I{} cp -L {} /out/lib/
        install -m 0755 /work/bench/harbor/container/marsh-setup.sh /out/libexec/
        install -m 0755 /work/bench/harbor/container/tmux /out/libexec/
        chown -R \$HOST_UID:\$HOST_GID /out
    "
echo "bundle: $OUT"
