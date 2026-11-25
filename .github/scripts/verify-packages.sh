#!/usr/bin/env bash
# Checks every bundled language server of the full packages in dist/ on this runner:
# install / unpack each one and run `edittr --check-servers --bundled` (a real go to
# definition per server; PATH is ignored so the runner's own tools can't hide a broken
# bundle). Usage: verify-packages.sh <Windows|macOS|Linux>; ONLY=rs,go limits the servers.
set -eo pipefail
OS=$1
ls -la dist
check() { # <label> <edittr executable>
  echo "::group::$1"
  args=(--check-servers --bundled); [ -n "$ONLY" ] && args+=(--only "$ONLY")
  want=$(( ${#ONLY} ? $(tr ',' '\n' <<< "$ONLY" | wc -l) : 5 ))
  set +e; "$2" "${args[@]}" > out.txt 2>&1; code=$?; set -e
  cat out.txt
  echo "::endgroup::"
  # Annotations are readable without signing in to GitHub (logs are not).
  while IFS= read -r l; do
    case $l in PASS*) echo "::notice::$1: $l" ;; FAIL*) echo "::error::$1: $l" ;; esac
  done < out.txt
  passed=$(grep -c '^PASS' out.txt || true)
  if [ "$code" != 0 ] || [ "$passed" != "$want" ]; then
    echo "::error::$1: $passed of $want language servers passed (exit $code)"; failed=1
  fi
}
failed=0
case "$OS" in
  Windows)
    7z x -y -oportable dist/*-windows-x64-full-portable.zip > /dev/null
    check "portable zip" "$(ls portable/*/edittr.exe)"
    setup=$(ls dist/*-windows-x64-full-setup.exe)
    pwsh -c "Start-Process -Wait -FilePath '$setup' -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART','/CURRENTUSER','/DIR=C:\\edittr-test'"
    check "installer" /c/edittr-test/edittr.exe ;;
  macOS)
    hdiutil attach -nobrowse -mountpoint /tmp/edittr-dmg dist/*-macos-universal-full.dmg > /dev/null
    check "dmg" /tmp/edittr-dmg/edittr.app/Contents/MacOS/edittr
    ditto -x -k dist/*-macos-universal-full.zip zip
    check "zip" zip/edittr.app/Contents/MacOS/edittr ;;
  Linux)
    mkdir tgz && tar -C tgz -xzf dist/*-full.tar.gz
    check "tar.gz" "$(ls tgz/*/edittr)" ;;
esac
exit $failed
