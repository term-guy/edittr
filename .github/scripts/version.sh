#!/usr/bin/env bash
# Derives the version from the git tag (v1.2.3 -> 1.2.3), falling back to
# Cargo.toml, writes it into Cargo.toml so the binary and packages agree,
# and exports VERSION for later steps.
set -euo pipefail
if [[ "${GITHUB_REF:-}" == refs/tags/v* ]]; then
  VERSION="${GITHUB_REF#refs/tags/v}"
  perl -0pi -e "s/^version = \"[^\"]*\"/version = \"${VERSION}\"/m" Cargo.toml
  perl -0pi -e "s/(name = \"edittr\"\r?\nversion = \")[^\"]*/\${1}${VERSION}/" Cargo.lock
else
  VERSION="$(perl -ne 'print $1 and exit if /^version = "([^"]+)"/' Cargo.toml)-dev"
fi
echo "VERSION=${VERSION}" >> "$GITHUB_ENV"
echo "Building edittr ${VERSION}"
