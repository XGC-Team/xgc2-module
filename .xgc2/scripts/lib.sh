# Shared by the package scripts: read product metadata and set up a reproducible build.

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"

# apt_version DISTRIBUTION: the Debian version declared for the distribution in product.yml.
apt_version() {
  local distribution="$1" version base
  version="$(awk -v key="$distribution:" '$1 == key { print $2; exit }' "$root/.xgc2/product.yml")"
  base="$(awk '/^version:/ { print $2; exit }' "$root/.xgc2/product.yml")"
  [[ -n "$version" && -n "$base" && "$version" == "$base~$distribution" ]] || {
    echo "product.yml: version and apt_versions.$distribution are missing or inconsistent" >&2
    return 1
  }
  dpkg --validate-version "$version"
  printf '%s\n' "$version"
}

# reproducible DIRECTORY: fix the timestamps of a package tree.
reproducible() {
  local epoch="${SOURCE_DATE_EPOCH:-$(git -C "$root" log -1 --format=%ct)}"
  [[ "$epoch" =~ ^[0-9]+$ ]] || { echo 'invalid SOURCE_DATE_EPOCH' >&2; return 1; }
  export SOURCE_DATE_EPOCH="$epoch"
  find "$1" -exec touch -h -d "@$epoch" {} +
}

# The four frame-link modules the host package installs. The cmake component of the same name is
# the only install path. Package paths are relative to the package root.
link_module_paths() {
  printf '%s\n' \
    usr/lib/xgc2/modules/libxgc2_peer_link.so \
    usr/lib/xgc2/modules/libxgc2_peer_link_8k.so \
    usr/lib/xgc2/modules/libxgc2_shm_bridge.so \
    usr/lib/xgc2/modules/libxgc2_shm_bridge_8k.so
}

# install_link_modules DESTDIR BUILD: compile the four modules against this tree's SDK (0.3.0) and
# install only that component under DESTDIR. BUILD is removed by the caller's cleanup. At most two
# compile jobs, and only while holding the host edge build lock. Files already in DESTDIR stay;
# the install must add these four libraries and nothing else (the SDK in particular).
install_link_modules() {
  local dest="$1" build="$2" name before after added expected
  [[ -d "$dest" && -n "$build" ]] || { echo "install_link_modules: need a destination and a build directory" >&2; return 1; }
  mkdir -p -- "$build"
  before="$(mktemp)"
  after="$(mktemp)"
  find "$dest" -mindepth 1 ! -type d -printf '%P\n' | sort > "$before"
  {
    flock 9
    (
      cd "$build"
      # This tree, not a previously installed Xgc2Module: the modules must match the SDK of this source.
      cmake "$root/modules" \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr \
        -DCMAKE_DISABLE_FIND_PACKAGE_Xgc2Module=ON
    )
    cmake --build "$build" --target xgc2_link_modules -- -j2
    (
      cd "$build"
      DESTDIR="$dest" cmake -DCOMPONENT=xgc2_link_modules -DCMAKE_INSTALL_DO_STRIP=1 -P cmake_install.cmake
    )
    flock -u 9
  } 9>/tmp/xgc2-host-edge-build.lock
  find "$dest" -mindepth 1 ! -type d -printf '%P\n' | sort > "$after"
  added="$(comm -13 "$before" "$after")"
  rm -f -- "$before" "$after"
  expected="$(link_module_paths | sort)"
  [[ "$added" == "$expected" ]] || {
    echo "link install added unexpected files: ${added:-<nothing>}" >&2
    return 1
  }
  while IFS= read -r name; do
    [[ -s "$dest/$name" ]] || { echo "link module is missing or empty: $name" >&2; return 1; }
  done < <(link_module_paths)
}

# shlib_depends ELF...: the Depends line for these binaries, from the distro that is building them.
# dpkg-shlibdeps reads debian/control from the working directory, so the call has its own directory.
shlib_depends() {
  local line dir
  [[ "$#" -ge 1 ]] || { echo "shlib_depends: no binaries" >&2; return 1; }
  dir="$(mktemp -d)"
  mkdir -p -- "$dir/debian"
  printf '%s\n' 'Source: xgc2-module' '' 'Package: xgc2-module-host' 'Architecture: any' > "$dir/debian/control"
  line="$(
    set -o pipefail
    cd "$dir"
    dpkg-shlibdeps -O "$@" | awk -F= '/^shlibs:Depends=/ { value = substr($0, index($0, "=") + 1) } END { print value }'
  )" || { rm -rf -- "$dir"; return 1; }
  rm -rf -- "$dir"
  [[ -n "$line" ]] || { echo "dpkg-shlibdeps produced no Depends" >&2; return 1; }
  printf '%s\n' "$line"
}

# install_deb TREE DEB: build the package and refuse to overwrite an existing artifact.
install_deb() {
  local tree="$1" deb="$2" temporary
  [[ ! -e "$deb" && ! -L "$deb" ]] || { echo "refusing existing artifact: $deb" >&2; return 1; }
  temporary="$(mktemp)"
  dpkg-deb --root-owner-group --build "$tree" "$temporary" > /dev/null
  chmod 0644 "$temporary"
  cp --no-clobber "$temporary" "$deb"
  cmp -s "$temporary" "$deb" || { echo "artifact appeared during the build: $deb" >&2; return 1; }
  rm -f -- "$temporary"
}
