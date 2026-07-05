#!/usr/bin/env bash

if ! declare -F fail >/dev/null 2>&1; then
  echo "scripts/debian/lib.sh must be sourced after defining fail()" >&2
  exit 1
fi

readonly RAVEN_DEBIAN_VERSION_REGEX='^[0-9]+[.][0-9]+[.][0-9]+([+~A-Za-z0-9._-]+)?$'

validate_raven_debian_version() {
  local version="$1"

  case "$version" in
    v*) fail "version must not include a leading v: ${version}" ;;
  esac
  if ! [[ "$version" =~ $RAVEN_DEBIAN_VERSION_REGEX ]]; then
    fail "version must be a Debian-compatible Raven version such as 0.12.0"
  fi
}
