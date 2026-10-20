#!/usr/bin/env bash
# Build and run the tests of the frame links without cmake or cargo:
#   modules/tests/run.sh OUTDIR [--netns PRIVATE_NETNS_PY]
# Builds the link modules in both frame classes (2k and 8k) and the test programs, runs the format, module and shared
# memory tests for each class. With --netns the packet-level test runs inside a private network namespace
# (private-netns.py of the test support: loopback only, nothing on the VM changes).
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="${1:?usage: run.sh OUTDIR [--netns private-netns.py]}"
shift
netns=""
if [[ "${1:-}" == "--netns" ]]; then netns="${2:?}"; fi
CXX="${CXX:-c++}"
JSONCPP_LINK="${JSONCPP_LINK:--ljsoncpp}"   # e.g. "-Wl,-Bstatic -ljsoncpp -Wl,-Bdynamic" for a module that runs elsewhere
BUILD_ONLY="${BUILD_ONLY:-0}"
mkdir -p "$out"
common=(-std=c++14 -O2 -g -pthread -Wall -Wextra -Werror "-I$root/include" "-I$root/sdk/include")
module() {  # class
  local class="$1"
  for link in peer_link shm_bridge; do
    "$CXX" "${common[@]}" -fPIC -shared -DXGC2_LINK_FRAME_CLASS="$class" "$root/modules/$link/$link.cpp" $JSONCPP_LINK -lrt \
      -Wl,--no-undefined "-Wl,--version-script=$root/sdk/exports.map" -o "$out/libxgc2_${link}_$class.so"
  done
}
test_program() {  # class source name defines...
  local class="$1" source="$2" name="$3"
  shift 3
  "$CXX" "${common[@]}" "$@" -DFRAME_TYPE="xgc2_frame$class" "$here/$source" $JSONCPP_LINK -ldl -lrt \
    -static-libstdc++ -static-libgcc -o "$out/${name}_$class"
}
"$CXX" "${common[@]}" "$here/frame_wire_test.cpp" -o "$out/frame_wire_test"
[[ "$BUILD_ONLY" == 1 ]] || "$out/frame_wire_test"
for class in 2k 8k; do
  module "$class"
  test_program "$class" peer_link_test.cpp peer_link_test "-DPEER_LINK_PATH=\"$out/libxgc2_peer_link_$class.so\""
  test_program "$class" shm_bridge_test.cpp shm_bridge_test "-DSHM_BRIDGE_PATH=\"$out/libxgc2_shm_bridge_$class.so\""
  test_program "$class" peer_link_netns_test.cpp peer_link_netns_test "-DPEER_LINK_PATH=\"$out/libxgc2_peer_link_$class.so\""
  [[ "$BUILD_ONLY" == 1 ]] || { echo "== peer_link ($class)"; "$out/peer_link_test_$class"; }
  [[ "$BUILD_ONLY" == 1 ]] || { echo "== shm_bridge ($class)"; "$out/shm_bridge_test_$class"; }
done
if [[ -n "$netns" ]]; then
  for class in 2k 8k; do
    echo "== peer_link under netem, private namespace ($class)"
    python3 "$netns" -- "$out/peer_link_netns_test_$class"
  done
fi
