#!/usr/bin/env bash
# Build and check both packages for one distribution inside the XGC2 build image of that
# release, from a host binary that ci_build_host.sh produced. The check installs the packages
# with dpkg and runs the binary there, so it also proves the glibc 2.27 build runs on
# that release.
#
#   ci_package.sh --distribution bionic|focal|noble --architecture amd64|arm64 --binary FILE --output DIR
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"

distribution=""; architecture=""; binary=""; output=""
usage() { echo 'usage: ci_package.sh --distribution D --architecture A --binary FILE --output DIR' >&2; exit 2; }
while [[ $# -gt 0 ]]; do
  case "$1" in
    --distribution) distribution="${2:?}"; shift 2;;
    --architecture) architecture="${2:?}"; shift 2;;
    --binary) binary="${2:?}"; shift 2;;
    --output) output="${2:?}"; shift 2;;
    *) usage;;
  esac
done
[[ -n "$distribution" && -n "$architecture" && -n "$binary" && -n "$output" ]] || usage
case "$distribution" in
  bionic|focal|noble) image="$("$root/.xgc2/scripts/ci_image.sh" "$distribution")";;
  *) usage;;
esac
binary="$(cd "$(dirname "$binary")" && pwd -P)/$(basename "$binary")"
mkdir -p -- "$output"
output="$(cd "$output" && pwd -P)"

docker pull "$image"
docker run --rm -e DISTRIBUTION="$distribution" -e ARCHITECTURE="$architecture" \
  -e SOURCE_DATE_EPOCH="$(git -C "$root" log -1 --format=%ct)" \
  -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  -v "$root:/workspace/source:ro" -v "$binary:/workspace/xgc2-module-host:ro" -v "$output:/workspace/out" \
  "$image" bash -euo pipefail -c '
    cd /workspace/source
    .xgc2/scripts/build_sdk_deb.sh --distribution "$DISTRIBUTION" --output /workspace/out
    .xgc2/scripts/build_host_deb.sh --distribution "$DISTRIBUTION" --architecture "$ARCHITECTURE" \
      --binary /workspace/xgc2-module-host --output /workspace/out
    version="$(awk -v key="$DISTRIBUTION:" "\$1 == key { print \$2; exit }" .xgc2/product.yml)"
    python3 -B .xgc2/scripts/check_debs.py --installed --distribution "$DISTRIBUTION" --architecture "$ARCHITECTURE" \
      --sdk-deb "/workspace/out/libxgc2-module-dev_${version}_all.deb" \
      --host-deb "/workspace/out/xgc2-module-host_${version}_${ARCHITECTURE}.deb" --work-dir /tmp/check
    chown -R "$HOST_UID:$HOST_GID" /workspace/out
  '
