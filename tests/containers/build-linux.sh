#!/usr/bin/env bash
# Builds a static (musl) Linux veneer binary for the host architecture in a container.
# Output: target/linux/release/veneer
set -euo pipefail
repo="$(cd "$(dirname "$0")/../.." && pwd)"
docker run --rm \
  -v "$repo":/src \
  -v veneer-cargo-registry:/usr/local/cargo/registry \
  -v veneer-cargo-git:/usr/local/cargo/git \
  -w /src \
  rust:1-alpine \
  sh -c 'apk add --no-cache musl-dev gcc g++ git >/dev/null && cargo build --release --locked -p veneer --target-dir /src/target/linux'
file "$repo/target/linux/release/veneer" 2>/dev/null || ls -la "$repo/target/linux/release/veneer"
