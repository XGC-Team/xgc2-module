#!/usr/bin/env bash
# Build xgc2-module-host for one distribution and architecture.
#
# Artifact interface:
#   --binary FILE   the xgc2-module-host ELF. This script does not build or invent it. CI passes the
#                   output of ci_build_host.sh; a host release passes the final host build. The binary
#                   must be linked against glibc 2.27 so one host ELF serves bionic, focal and noble.
#   the four frame links are not an input. This script compiles them from modules/ against this
#                   tree's SDK 0.3.0 and installs only
#                   /usr/lib/xgc2/modules/libxgc2_{peer_link,peer_link_8k,shm_bridge,shm_bridge_8k}.so.
#                   ci_package.sh runs this inside the distro image, so each distro's package links
#                   its own libjsoncpp. Depends is dpkg-shlibdeps of the host ELF and those four libraries.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

usage() { echo 'usage: build_host_deb.sh --distribution D --architecture amd64|arm64 --binary FILE --output DIR' >&2; exit 2; }
distribution=""; architecture=""; binary=""; output=""
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
[[ "$architecture" == amd64 || "$architecture" == arm64 ]] || usage
[[ -x "$binary" ]] || { echo "not an executable file: $binary" >&2; exit 2; }

package=xgc2-module-host
version="$(apt_version "$distribution")"
mkdir -p -- "$output"
output="$(cd "$output" && pwd -P)"
work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT

tree="$work/package"
install -D -m 0755 -s "$binary" "$tree/usr/bin/xgc2-module-host"
install -D -m 0644 "$root/process-definitions/xgc2-module-host.json" \
  "$tree/usr/share/xgc2/process-definitions/xgc2-module-host.json"
install -D -m 0644 "$root/README.md" "$tree/usr/share/doc/$package/README.md"
for doc in architecture manifest control-api; do
  install -D -m 0644 "$root/docs/$doc.md" "$tree/usr/share/doc/$package/$doc.md"
done
install -D -m 0644 "$root/docs/examples/entity.toml" "$tree/usr/share/doc/$package/examples/entity.toml"
install_link_modules "$tree" "$work/modules"
depends="$(shlib_depends "$tree/usr/bin/xgc2-module-host" \
  "$tree/usr/lib/xgc2/modules/libxgc2_peer_link.so" \
  "$tree/usr/lib/xgc2/modules/libxgc2_peer_link_8k.so" \
  "$tree/usr/lib/xgc2/modules/libxgc2_shm_bridge.so" \
  "$tree/usr/lib/xgc2/modules/libxgc2_shm_bridge_8k.so")"
install -D -m 0644 /dev/stdin "$tree/usr/share/doc/$package/copyright" <<'COPYRIGHT'
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: xgc2-module
Source: https://github.com/XGC-Team/xgc2-module

Files: *
Copyright: XGC-Team
License: Apache-2.0
 On Debian systems the full text is in /usr/share/common-licenses/Apache-2.0.
COPYRIGHT
mkdir -p "$tree/DEBIAN"
cat > "$tree/DEBIAN/control" <<CONTROL
Package: $package
Version: $version
Section: utils
Priority: optional
Architecture: $architecture
Maintainer: XGC2 <apt@example.com>
Depends: $depends
Suggests: libxgc2-module-dev
Description: Same-entity module host
 Runs the algorithm modules of one entity (one robot) in one process:
 typed zero-copy channels, an event-driven scheduler, step budgets with hang
 isolation, hot-plug and an XRPC control plane on a Unix socket. Installs the
 frame links peer_link and shm_bridge, each for the 2k and 8k frame classes.
CONTROL
reproducible "$tree"
deb="$output/${package}_${version}_${architecture}.deb"
install_deb "$tree" "$deb"
printf '%s\n' "$deb"
