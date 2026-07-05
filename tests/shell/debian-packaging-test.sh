#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
workdir="${TMPDIR:-/tmp}/raven-debian-packaging-test"

fail() {
  echo "FAIL: $1" >&2
  exit 1
}

require_file() {
  test -f "$1" || fail "missing file: $1"
}

require_executable() {
  test -x "$1" || fail "missing executable: $1"
}

require_executable "$repo_root/scripts/debian/build-deb.sh"
require_executable "$repo_root/scripts/debian/update-apt-repo.sh"

if ! command -v dpkg-deb >/dev/null 2>&1 || ! command -v dpkg-scanpackages >/dev/null 2>&1; then
  echo "SKIP: dpkg-deb and dpkg-scanpackages are required for Debian packaging behavior checks"
  exit 0
fi

rm -rf "$workdir"
mkdir -p "$workdir/bin" "$workdir/dist" "$workdir/apt"

cat > "$workdir/bin/raven" <<'FAKE_RAVEN'
#!/usr/bin/env bash
set -euo pipefail

if [ "${1:-}" = "--version" ]; then
  echo "raven 1.2.3-test"
  exit 0
fi

echo "unexpected fake raven args: $*" >&2
exit 1
FAKE_RAVEN
chmod +x "$workdir/bin/raven"

"$repo_root/scripts/debian/build-deb.sh" 1.2.3 amd64 "$workdir/bin/raven" "$workdir/dist"
"$repo_root/scripts/debian/build-deb.sh" 1.2.3 arm64 "$workdir/bin/raven" "$workdir/dist"

amd64_deb="$workdir/dist/raven_1.2.3-1_amd64.deb"
arm64_deb="$workdir/dist/raven_1.2.3-1_arm64.deb"
require_file "$amd64_deb"
require_file "$arm64_deb"

test "$(dpkg-deb --field "$amd64_deb" Package)" = "raven" || fail "amd64 package name mismatch"
test "$(dpkg-deb --field "$amd64_deb" Version)" = "1.2.3-1" || fail "amd64 version mismatch"
test "$(dpkg-deb --field "$amd64_deb" Architecture)" = "amd64" || fail "amd64 architecture mismatch"
depends="$(dpkg-deb --field "$amd64_deb" Depends)"
case "$depends" in
  *ca-certificates*curl* | *curl*ca-certificates*) ;;
  *) fail "amd64 package dependency list lacks ca-certificates and curl: ${depends}" ;;
esac
dpkg-deb --contents "$amd64_deb" | grep -F "./usr/bin/raven" >/dev/null || fail "amd64 deb lacks /usr/bin/raven"
dpkg-deb --contents "$amd64_deb" | grep -F "./usr/share/doc/raven/copyright" >/dev/null || fail "amd64 deb lacks copyright"

"$repo_root/scripts/debian/update-apt-repo.sh" "$workdir/apt" 1.2.3 "$amd64_deb" "$arm64_deb"

require_file "$workdir/apt/pool/main/r/raven/raven_1.2.3-1_amd64.deb"
require_file "$workdir/apt/pool/main/r/raven/raven_1.2.3-1_arm64.deb"
require_file "$workdir/apt/dists/stable/main/binary-amd64/Packages"
require_file "$workdir/apt/dists/stable/main/binary-amd64/Packages.gz"
require_file "$workdir/apt/dists/stable/main/binary-arm64/Packages"
require_file "$workdir/apt/dists/stable/main/binary-arm64/Packages.gz"
require_file "$workdir/apt/dists/stable/Release"

grep -F "Filename: pool/main/r/raven/raven_1.2.3-1_amd64.deb" "$workdir/apt/dists/stable/main/binary-amd64/Packages" >/dev/null \
  || fail "amd64 Packages index lacks Raven deb"
grep -F "Architectures: amd64 arm64" "$workdir/apt/dists/stable/Release" >/dev/null \
  || fail "Release file lacks architecture list"
grep -F "Components: main" "$workdir/apt/dists/stable/Release" >/dev/null \
  || fail "Release file lacks main component"

if command -v gpg >/dev/null 2>&1 && command -v gpgv >/dev/null 2>&1; then
  export GNUPGHOME="$workdir/gnupg"
  mkdir -p "$GNUPGHOME"
  chmod 700 "$GNUPGHOME"
  cat > "$workdir/gpg-batch" <<'GPG_BATCH'
Key-Type: eddsa
Key-Curve: ed25519
Name-Real: Raven apt test
Name-Email: raven-apt-test@example.invalid
Expire-Date: 0
%no-protection
%commit
GPG_BATCH
  gpg --batch --generate-key "$workdir/gpg-batch" >/dev/null 2>&1
  fingerprint="$(gpg --batch --list-secret-keys --with-colons | awk -F: '/^fpr:/ { print $10; exit }')"
  test -n "$fingerprint" || fail "could not read generated gpg fingerprint"

  APT_GPG_KEY_FINGERPRINT="$fingerprint" \
    "$repo_root/scripts/debian/update-apt-repo.sh" "$workdir/apt" 1.2.3 "$amd64_deb" "$arm64_deb"

  require_file "$workdir/apt/dists/stable/InRelease"
  require_file "$workdir/apt/dists/stable/Release.gpg"
  gpg --batch --yes --output "$workdir/raven-archive-keyring.gpg" --export "$fingerprint"
  gpgv --keyring "$workdir/raven-archive-keyring.gpg" "$workdir/apt/dists/stable/InRelease" >/dev/null 2>&1 \
    || fail "InRelease signature verification failed"
  gpgv --keyring "$workdir/raven-archive-keyring.gpg" "$workdir/apt/dists/stable/Release.gpg" "$workdir/apt/dists/stable/Release" >/dev/null 2>&1 \
    || fail "Release.gpg signature verification failed"
else
  echo "SKIP: gpg and gpgv are required for apt repository signature checks"
fi

echo "debian packaging tests passed"
