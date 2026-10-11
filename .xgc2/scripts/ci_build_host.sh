#!/usr/bin/env bash
# Release build of xgc2-module-host inside the XGC2 bionic build image (Ubuntu 18.04), so that the
# one binary needs glibc 2.27 at most and serves bionic, focal and noble. Runs on a host of the
# target architecture. The image carries the Rust toolchain; nothing is installed.
#
#   ci_build_host.sh --output DIR        writes DIR/xgc2-module-host
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
image="$("$root/.xgc2/scripts/ci_image.sh" bionic)"

output=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --output) output="${2:?}"; shift 2;;
    *) echo 'usage: ci_build_host.sh --output DIR' >&2; exit 2;;
  esac
done
[[ -n "$output" ]] || { echo 'usage: ci_build_host.sh --output DIR' >&2; exit 2; }
mkdir -p -- "$output"
output="$(cd "$output" && pwd -P)"

docker pull "$image"
docker run --rm -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  -v "$root:/workspace/source:ro" -v "$output:/workspace/out" "$image" bash -euo pipefail -c '
    cd /workspace/source
    CARGO_TARGET_DIR=/tmp/target cargo build --release --locked --bins
    install -m 0755 /tmp/target/release/xgc2-module-host /workspace/out/xgc2-module-host
    chown "$HOST_UID:$HOST_GID" /workspace/out/xgc2-module-host
  '
printf '%s\n' "$output/xgc2-module-host"
