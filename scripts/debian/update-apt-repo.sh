#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'USAGE'
Usage: scripts/debian/update-apt-repo.sh <apt-repo-root> <version> <amd64.deb> <arm64.deb>

Copy Raven .deb packages into an apt repository tree, rebuild Packages indexes,
write dists/stable/Release, and sign Release when APT_GPG_KEY_FINGERPRINT is set.
USAGE
  exit 2
}

fail() {
  echo "update-apt-repo: $1" >&2
  exit 1
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "${script_dir}/lib.sh"

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is required"
}

tmp_files=()
cleanup() {
  if [ "${#tmp_files[@]}" -gt 0 ]; then
    rm -f "${tmp_files[@]}"
  fi
}
trap cleanup EXIT

make_temp_file() {
  local target_var="$1"
  local template="$2"
  local tmp_file

  tmp_file="$(mktemp "$template")"
  tmp_files+=("$tmp_file")
  printf -v "$target_var" '%s' "$tmp_file"
}

if [ "$#" -ne 4 ]; then
  usage
fi

repo_root="$1"
version="$2"
amd64_deb="$3"
arm64_deb="$4"

validate_raven_debian_version "$version"

require_command dpkg-deb
require_command dpkg-scanpackages
require_command gzip
require_command md5sum
require_command sha256sum

for deb in "$amd64_deb" "$arm64_deb"; do
  if [ ! -f "$deb" ]; then
    fail "package not found: ${deb}"
  fi
  if [ "$(dpkg-deb --field "$deb" Package)" != "raven" ]; then
    fail "${deb} is not a raven package"
  fi
  if [ "$(dpkg-deb --field "$deb" Version)" != "${version}-1" ]; then
    fail "${deb} version must be ${version}-1"
  fi
done

if [ "$(dpkg-deb --field "$amd64_deb" Architecture)" != "amd64" ]; then
  fail "${amd64_deb} must be Architecture: amd64"
fi
if [ "$(dpkg-deb --field "$arm64_deb" Architecture)" != "arm64" ]; then
  fail "${arm64_deb} must be Architecture: arm64"
fi

mkdir -p \
  "${repo_root}" \
  "${repo_root}/pool/main/r/raven" \
  "${repo_root}/dists/stable/main/binary-amd64" \
  "${repo_root}/dists/stable/main/binary-arm64"
repo_root="$(cd "$repo_root" && pwd)"

cp "$amd64_deb" "${repo_root}/pool/main/r/raven/"
cp "$arm64_deb" "${repo_root}/pool/main/r/raven/"

for arch in amd64 arm64; do
  packages_file="${repo_root}/dists/stable/main/binary-${arch}/Packages"
  make_temp_file packages_tmp "${packages_file}.tmp.XXXXXX"
  make_temp_file packages_gz_tmp "${packages_file}.gz.tmp.XXXXXX"
  (
    cd "$repo_root"
    dpkg-scanpackages --multiversion --arch "$arch" pool /dev/null > "$packages_tmp"
  )
  gzip -9n -c "$packages_tmp" > "$packages_gz_tmp"
  mv "$packages_tmp" "$packages_file"
  mv "$packages_gz_tmp" "${packages_file}.gz"
done

release_file="${repo_root}/dists/stable/Release"
make_temp_file release_tmp "${release_file}.tmp.XXXXXX"
date_rfc2822="$(LC_ALL=C date -u '+%a, %d %b %Y %H:%M:%S +0000')"

{
  cat <<RELEASE
Origin: Raven
Label: Raven
Suite: stable
Codename: stable
Date: ${date_rfc2822}
Architectures: amd64 arm64
Components: main
Description: Raven apt repository
MD5Sum:
RELEASE

  (
    cd "${repo_root}/dists/stable"
    find main -type f \( -name 'Packages' -o -name 'Packages.gz' \) | LC_ALL=C sort | while read -r path; do
      checksum="$(md5sum "$path" | awk '{print $1}')"
      size="$(wc -c < "$path" | tr -d ' ')"
      printf ' %s %16s %s\n' "$checksum" "$size" "$path"
    done
  )

  cat <<'RELEASE'
SHA256:
RELEASE

  (
    cd "${repo_root}/dists/stable"
    find main -type f \( -name 'Packages' -o -name 'Packages.gz' \) | LC_ALL=C sort | while read -r path; do
      checksum="$(sha256sum "$path" | awk '{print $1}')"
      size="$(wc -c < "$path" | tr -d ' ')"
      printf ' %s %16s %s\n' "$checksum" "$size" "$path"
    done
  )
} > "$release_tmp"

mv "$release_tmp" "$release_file"

if [ -n "${APT_GPG_KEY_FINGERPRINT:-}" ]; then
  require_command gpg
  passphrase_args=()
  if [ -n "${APT_GPG_PASSPHRASE:-}" ]; then
    make_temp_file passphrase_file "${TMPDIR:-/tmp}/raven-apt-gpg-passphrase.XXXXXX"
    chmod 600 "$passphrase_file"
    printf '%s' "$APT_GPG_PASSPHRASE" > "$passphrase_file"
    passphrase_args=(--pinentry-mode loopback --passphrase-file "$passphrase_file")
  fi
  gpg --batch --yes "${passphrase_args[@]}" \
    --default-key "$APT_GPG_KEY_FINGERPRINT" \
    --clearsign \
    --output "${repo_root}/dists/stable/InRelease" \
    "$release_file"
  gpg --batch --yes "${passphrase_args[@]}" \
    --default-key "$APT_GPG_KEY_FINGERPRINT" \
    --armor --detach-sign \
    --output "${repo_root}/dists/stable/Release.gpg" \
    "$release_file"
else
  rm -f "${repo_root}/dists/stable/InRelease" "${repo_root}/dists/stable/Release.gpg"
  echo "update-apt-repo: APT_GPG_KEY_FINGERPRINT not set; wrote unsigned repository metadata" >&2
fi

echo "updated ${repo_root}"
