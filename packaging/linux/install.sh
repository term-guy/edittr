#!/bin/sh
# Installs edittr for the current user into ~/.local (no root needed).
# Full downloads also install their bundled language servers into
# ~/.local/lib/edittr/servers, where edittr looks for them.
set -e
cd "$(dirname "$0")"
mkdir -p ~/.local/bin ~/.local/share/applications ~/.local/share/icons/hicolor/256x256/apps
cp edittr ~/.local/bin/edittr
cp edittr.desktop ~/.local/share/applications/edittr.desktop
cp edittr.png ~/.local/share/icons/hicolor/256x256/apps/edittr.png
if [ -d servers ]; then
  mkdir -p ~/.local/lib/edittr
  rm -rf ~/.local/lib/edittr/servers.new
  cp -R servers ~/.local/lib/edittr/servers.new
  rm -rf ~/.local/lib/edittr/servers
  mv ~/.local/lib/edittr/servers.new ~/.local/lib/edittr/servers
  echo "Installed bundled language servers."
fi
echo "Installed. Make sure ~/.local/bin is on your PATH."
