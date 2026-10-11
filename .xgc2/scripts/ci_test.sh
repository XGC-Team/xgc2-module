#!/usr/bin/env bash
# Documentation, unit and integration tests and the SDK check of this checkout inside the XGC2
# build image of one Ubuntu release. The integration tests compile the C and C++ test modules in
# tests/modules with the compilers of the image, so each release also checks the headers with the
# compilers it ships. The source is mounted read-only; every output goes to a temporary directory.
#
#   ci_test.sh --distribution bionic|focal|noble
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"

distribution=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --distribution) distribution="${2:?}"; shift 2;;
    *) echo 'usage: ci_test.sh --distribution bionic|focal|noble' >&2; exit 2;;
  esac
done
image="$("$root/.xgc2/scripts/ci_image.sh" "${distribution:-}")"
work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT

docker pull "$image"
docker run --rm -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  -v "$root:/workspace/source:ro" -v "$work:/workspace/work" "$image" bash -euo pipefail -c '
    trap "chown -R $HOST_UID:$HOST_GID /workspace/work" EXIT
    cd /workspace/source
    export CARGO_TARGET_DIR=/workspace/work/target
    RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
    cargo test --locked --no-fail-fast
    python3 sdk/tests/test_sdk.py --work-dir /workspace/work/sdk-check
  '
