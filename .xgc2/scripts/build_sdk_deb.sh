#!/usr/bin/env bash
# Build libxgc2-module-dev (the header-only SDK, architecture all) for one distribution.
# No compiler is involved, so the bytes are the same for every distribution; only the version
# suffix differs.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

usage() { echo 'usage: build_sdk_deb.sh --distribution bionic|focal|noble --output DIR' >&2; exit 2; }
distribution=""; output=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --distribution) distribution="${2:?}"; shift 2;;
    --output) output="${2:?}"; shift 2;;
    *) usage;;
  esac
done
[[ -n "$distribution" && -n "$output" ]] || usage

package=libxgc2-module-dev
version="$(apt_version "$distribution")"
mkdir -p -- "$output"
output="$(cd "$output" && pwd -P)"
work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT

# CMake 3.10 (Ubuntu 18.04) has neither -S/-B nor --install: configure from the build directory and
# install through the build tool.
mkdir -p "$work/build"
(cd "$work/build" && cmake "$root/sdk" -DCMAKE_INSTALL_PREFIX=/usr > /dev/null)
DESTDIR="$work/package" cmake --build "$work/build" --target install > /dev/null
mkdir -p "$work/package/DEBIAN"
cat > "$work/package/DEBIAN/control" <<CONTROL
Package: $package
Version: $version
Section: libdevel
Priority: optional
Architecture: all
Maintainer: XGC2 <apt@example.com>
Description: xgc2-module SDK: the module ABI header
 Header-only C and C++ SDK for modules of the xgc2-module host. Installs
 <xgc2/module.h> and the Xgc2Module CMake package with the imported
 target Xgc2Module::SDK. No library, no runtime.
CONTROL
reproducible "$work/package"
deb="$output/${package}_${version}_all.deb"
install_deb "$work/package" "$deb"
printf '%s\n' "$deb"
