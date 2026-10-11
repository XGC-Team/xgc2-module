#!/usr/bin/env bash
# Build the host against the Ubuntu 18.04 sysroot (glibc 2.27, gcc 7.5) in a separate target
# directory and run its tests there. Proves two things: the binary links against glibc 2.27 only,
# and the tests pass when the test processes run on the glibc 2.27 loader and libc.
#
#   scripts/bionic-test.sh [extra cargo test arguments]
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/bionic-env.sh"
cd "$root"
cargo build --locked -j2 --target "$triple" --bins
echo "highest glibc symbol version needed by xgc2-module-host: $(glibc_floor "$CARGO_TARGET_DIR/$triple/debug/xgc2-module-host")"
cargo test --locked -j2 --target "$triple" "$@"
