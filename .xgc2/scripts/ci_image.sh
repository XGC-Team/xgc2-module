#!/usr/bin/env bash
# The XGC2 build image of an Ubuntu release, one place for the CI scripts: ci_image.sh bionic|focal|noble
# The images carry the compilers, cmake, the packaging tools and the Rust toolchain; CI installs nothing.
set -euo pipefail
case "${1:-}" in
  bionic|focal|noble) echo "ghcr.io/xgc-team/xgc2-images/xgc2-build-$1-dev:1.0.0" ;;
  *) echo 'usage: ci_image.sh bionic|focal|noble' >&2; exit 2 ;;
esac
