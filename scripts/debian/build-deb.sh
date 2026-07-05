#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
Usage: scripts/debian/build-deb.sh <version> <amd64|arm64> <raven-binary> <output-dir>

Build a Debian package for an already-built Raven Linux binary.
The Debian package version is <version>-1.
USAGE
  exit 2
}

fail() {
  echo "build-deb: $1" >&2
  exit 1
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "${script_dir}/lib.sh"

if [ "$#" -ne 4 ]; then
  usage
fi

version="$1"
architecture="$2"
binary="$3"
output_dir="$4"

validate_raven_debian_version "$version"

case "$architecture" in
  amd64 | arm64) ;;
  *) fail "architecture must be amd64 or arm64, got ${architecture}" ;;
esac

if [ ! -f "$binary" ]; then
  fail "raven binary not found: ${binary}"
fi
if [ ! -x "$binary" ]; then
  fail "raven binary must be executable: ${binary}"
fi

repo_root="$(cd "${script_dir}/../.." && pwd)"
license_file="${repo_root}/LICENSE"
if [ ! -f "$license_file" ]; then
  fail "LICENSE not found at ${license_file}"
fi

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is required"
}
require_command dpkg-deb

is_elf_file() {
  [ "$(od -An -tx1 -N4 "$1" | tr -d ' \n')" = "7f454c46" ]
}

derive_shlib_depends() {
  local installed_binary="$1"
  local shlib_stderr
  local shlib_output

  if ! is_elf_file "$installed_binary"; then
    return 0
  fi

  if command -v dpkg-shlibdeps >/dev/null 2>&1; then
    mkdir -p "${workdir}/debian"
    cat > "${workdir}/debian/control" <<'CONTROL'
Source: raven
Section: devel
Priority: optional
Maintainer: Jonathan Marc Bearak <jonathan@bearak.net>
Standards-Version: 4.7.0

Package: raven
Architecture: any
Depends: ${shlibs:Depends}, ca-certificates, curl
Description: Static analyzer and language server for R
 Raven resolves R scope statically for editor language intelligence and
 headless CI checks.
CONTROL

    shlib_stderr="${workdir}/dpkg-shlibdeps.stderr"
    if shlib_output="$(cd "$workdir" && dpkg-shlibdeps -O -e "$installed_binary" 2>"$shlib_stderr")"; then
      awk '/^shlibs:Depends=/ { sub(/^shlibs:Depends=/, ""); print; found = 1 } END { if (!found) print "" }' <<<"$shlib_output"
      return 0
    fi

    echo "build-deb: dpkg-shlibdeps could not resolve ${architecture} libraries; falling back to ELF NEEDED mapping" >&2
    sed 's/^/build-deb: dpkg-shlibdeps: /' "$shlib_stderr" >&2
  else
    echo "build-deb: dpkg-shlibdeps not found; falling back to ELF NEEDED mapping" >&2
  fi
  derive_known_elf_depends "$installed_binary"
}

derive_known_elf_depends() {
  local installed_binary="$1"
  local dep
  local glibc_version
  local needed
  local needs_libc=false
  local needs_libgcc=false

  require_command readelf
  require_command grep
  require_command sed
  require_command sort

  while IFS= read -r needed; do
    case "$needed" in
      libc.so.6 | libm.so.6 | libdl.so.2 | libpthread.so.0 | librt.so.1)
        needs_libc=true
        ;;
      libgcc_s.so.1)
        needs_libgcc=true
        ;;
      *)
        fail "shared library dependency ${needed} is not mapped to a Debian package"
        ;;
    esac
  done < <(LC_ALL=C readelf -d "$installed_binary" | awk -F'[][]' '/Shared library:/ { print $2 }')

  if [ "$needs_libc" = true ]; then
    glibc_version="$(
      LC_ALL=C readelf --version-info "$installed_binary" 2>/dev/null |
        grep -Eo 'GLIBC_[0-9]+([.][0-9]+)+' |
        sed 's/^GLIBC_//' |
        sort -V |
        tail -n 1 || true
    )"
    if [ -n "$glibc_version" ]; then
      printf 'libc6 (>= %s)' "$glibc_version"
    else
      printf 'libc6'
    fi
  fi

  if [ "$needs_libgcc" = true ]; then
    if [ "$needs_libc" = true ]; then
      printf ', '
    fi
    dep="libgcc-s1"
    printf '%s' "$dep"
  fi
}

package_version="${version}-1"
package_name="raven_${package_version}_${architecture}.deb"
workdir="$(mktemp -d "${TMPDIR:-/tmp}/raven-deb-${architecture}.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT

package_root="${workdir}/package"
mkdir -p \
  "${package_root}/DEBIAN" \
  "${package_root}/usr/bin" \
  "${package_root}/usr/share/doc/raven" \
  "$output_dir"

install -m 0755 "$binary" "${package_root}/usr/bin/raven"
install -m 0644 "$license_file" "${package_root}/usr/share/doc/raven/copyright"

installed_size_kb="$(du -sk "${package_root}/usr" | awk '{print $1}')"
shlib_depends="$(derive_shlib_depends "${package_root}/usr/bin/raven")"
depends="ca-certificates, curl"
if [ -n "$shlib_depends" ]; then
  depends="${depends}, ${shlib_depends}"
fi

cat > "${package_root}/DEBIAN/control" <<CONTROL
Package: raven
Version: ${package_version}
Section: devel
Priority: optional
Architecture: ${architecture}
Maintainer: Jonathan Marc Bearak <jonathan@bearak.net>
Installed-Size: ${installed_size_kb}
Depends: ${depends}
Homepage: https://github.com/jbearak/raven
Description: Static analyzer and language server for R
 Raven resolves R scope statically for editor language intelligence and
 headless CI checks.
CONTROL

dpkg-deb --build --root-owner-group "$package_root" "${output_dir}/${package_name}" >/dev/null
echo "${output_dir}/${package_name}"
