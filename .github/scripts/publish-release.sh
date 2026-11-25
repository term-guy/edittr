#!/usr/bin/env bash
# Adds dist/* to the GitHub release $TAG, creating it if this part finishes first.
# $PART names this upload (windows-linux or macos): its checksums go to
# SHA256SUMS-$PART.txt, and SHA256SUMS.txt is rebuilt from every part uploaded so far.
set -eo pipefail
set -u
cd dist
sha256sum -- * > "SHA256SUMS-$PART.txt"
cat "SHA256SUMS-$PART.txt"
repo=(--repo "$GITHUB_REPOSITORY")
if ! gh release view "$TAG" "${repo[@]}" > /dev/null 2>&1; then
  # The other part may create it at the same moment: fall back to uploading.
  gh release create "$TAG" "${repo[@]}" --title "edittr $TAG" --generate-notes ||
    gh release view "$TAG" "${repo[@]}" > /dev/null
fi
gh release upload "$TAG" --clobber "${repo[@]}" -- *
# Combined checksums: this part's plus whatever the other part already uploaded.
mkdir -p ../sums
gh release download "$TAG" "${repo[@]}" --pattern 'SHA256SUMS-*.txt' --dir ../sums --clobber
cat ../sums/SHA256SUMS-*.txt | sort -k2 > SHA256SUMS.txt
gh release upload "$TAG" --clobber "${repo[@]}" SHA256SUMS.txt
