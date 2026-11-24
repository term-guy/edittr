#!/usr/bin/env bash
# Builds the language-server bundle for the "full" downloads.
#
#   bundle-servers.sh <platform> <dest>
#   platform: windows-x64 | macos-universal | linux-x86_64 | linux-aarch64
#
# Produces <dest>/servers.json plus the servers it lists (see src/bundled.rs):
#   rust-analyzer, clangd   official GitHub releases, SHA-256 checked against GitHub's digest
#   node                    nodejs.org LTS binary, SHA-256 checked against SHASUMS256.txt
#   TypeScript + pyright    npm packages, run with the bundled node
#   gopls                   built from source with Go (cross-compiled per platform)
# Needs: curl, python3 (or python), node + npm, go. Uses GITHUB_TOKEN when set (API rate limits).
set -euo pipefail

PLATFORM=$1
mkdir -p "$2"
DEST=$(cd "$2" && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

PY=python3
if ! command -v python3 >/dev/null || ! python3 -c 1 2>/dev/null; then PY=python; fi
EXE=""
[ "$PLATFORM" = windows-x64 ] && EXE=".exe"

# Windows (Git Bash): native tools like node and 7-Zip need C:/-style paths where
# MSYS doesn't convert them automatically (inside strings, attached to flags).
native() { if command -v cygpath >/dev/null; then cygpath -m "$1"; else printf '%s' "$1"; fi; }
sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }
unzip_to() { mkdir -p "$2"; if command -v unzip >/dev/null; then unzip -q -o "$1" -d "$2"; else 7z x -y -o"$(native "$2")" "$(native "$1")" >/dev/null; fi; }

# gh_asset <owner/repo> <exact asset name regex> <out file>: download a latest-release asset, verify its digest.
gh_asset() {
  local auth=()
  [ -n "${GITHUB_TOKEN:-}" ] && auth=(-H "Authorization: Bearer $GITHUB_TOKEN")
  curl -fsSL ${auth[@]+"${auth[@]}"} "https://api.github.com/repos/$1/releases/latest" > "$TMP/release.json"
  local info
  info=$("$PY" - "$2" "$TMP/release.json" <<'EOF'
import json, re, sys
d = json.load(open(sys.argv[2]))
a = next(a for a in d["assets"] if re.fullmatch(sys.argv[1], a["name"]))
print(a["browser_download_url"], (a.get("digest") or "").removeprefix("sha256:"))
EOF
)
  local url=${info% *} digest=${info#* }
  echo "  $url"
  curl -fsSL "$url" -o "$3"
  if [ -n "$digest" ] && [ "$(sha256 "$3")" != "$digest" ]; then echo "checksum mismatch for $url" >&2; exit 1; fi
}

lipo_or_copy() { # out, inputs...: universal binary on macOS, else copy the single input
  local out=$1; shift
  if [ $# -gt 1 ]; then lipo -create -output "$out" "$@"; else cp "$1" "$out"; fi
  chmod +x "$out"
}

echo "== rust-analyzer"
mkdir -p "$DEST/rust-analyzer"
case $PLATFORM in
  windows-x64)
    gh_asset rust-lang/rust-analyzer 'rust-analyzer-x86_64-pc-windows-msvc\.zip' "$TMP/ra.zip"
    unzip_to "$TMP/ra.zip" "$TMP/ra"
    cp "$TMP/ra/rust-analyzer.exe" "$DEST/rust-analyzer/" ;;
  macos-universal)
    for t in aarch64 x86_64; do
      gh_asset rust-lang/rust-analyzer "rust-analyzer-$t-apple-darwin\.gz" "$TMP/ra-$t.gz"
      gunzip -f "$TMP/ra-$t.gz"
    done
    lipo_or_copy "$DEST/rust-analyzer/rust-analyzer" "$TMP/ra-aarch64" "$TMP/ra-x86_64" ;;
  linux-x86_64|linux-aarch64)
    t=${PLATFORM#linux-}
    gh_asset rust-lang/rust-analyzer "rust-analyzer-$t-unknown-linux-gnu\.gz" "$TMP/ra.gz"
    gunzip -f "$TMP/ra.gz"
    lipo_or_copy "$DEST/rust-analyzer/rust-analyzer" "$TMP/ra" ;;
esac

CLANGD=""
if [ "$PLATFORM" != linux-aarch64 ]; then # clangd publishes no arm64 Linux build
  echo "== clangd"
  case $PLATFORM in windows-x64) os=windows ;; macos-universal) os=mac ;; *) os=linux ;; esac
  gh_asset clangd/clangd "clangd-$os-[0-9.]+\.zip" "$TMP/clangd.zip"
  unzip_to "$TMP/clangd.zip" "$DEST/clangd"
  CLANGD=$(cd "$DEST" && ls -d clangd/clangd_*/bin/clangd"$EXE" | head -1)
  chmod +x "$DEST/$CLANGD"
fi

echo "== node"
NODE_VERSION=$(curl -fsSL https://nodejs.org/dist/index.json | "$PY" -c 'import json,sys; print(next(r["version"] for r in json.load(sys.stdin) if r["lts"]))')
NODE_DIST=https://nodejs.org/dist/$NODE_VERSION
curl -fsSL "$NODE_DIST/SHASUMS256.txt" -o "$TMP/node-sums.txt"
node_get() { # file name in dist dir -> $TMP/<name>, verified
  echo "  $NODE_DIST/$1"
  curl -fsSL "$NODE_DIST/$1" -o "$TMP/$1"
  [ "$(sha256 "$TMP/$1")" = "$(grep " $1\$" "$TMP/node-sums.txt" | cut -d' ' -f1)" ] || { echo "checksum mismatch for $1" >&2; exit 1; }
}
mkdir -p "$DEST/node"
case $PLATFORM in
  windows-x64)
    node_get "node-$NODE_VERSION-win-x64.zip"
    unzip_to "$TMP/node-$NODE_VERSION-win-x64.zip" "$TMP/node"
    cp "$TMP/node/node-$NODE_VERSION-win-x64/node.exe" "$TMP/node/node-$NODE_VERSION-win-x64/LICENSE" "$DEST/node/" ;;
  macos-universal)
    bins=()
    for a in arm64 x64; do
      node_get "node-$NODE_VERSION-darwin-$a.tar.gz"
      tar -xzf "$TMP/node-$NODE_VERSION-darwin-$a.tar.gz" -C "$TMP"
      bins+=("$TMP/node-$NODE_VERSION-darwin-$a/bin/node")
    done
    lipo_or_copy "$DEST/node/node" "${bins[@]}"
    cp "$TMP/node-$NODE_VERSION-darwin-arm64/LICENSE" "$DEST/node/" ;;
  linux-x86_64|linux-aarch64)
    a=x64; [ "$PLATFORM" = linux-aarch64 ] && a=arm64
    node_get "node-$NODE_VERSION-linux-$a.tar.xz"
    tar -xJf "$TMP/node-$NODE_VERSION-linux-$a.tar.xz" -C "$TMP"
    lipo_or_copy "$DEST/node/node" "$TMP/node-$NODE_VERSION-linux-$a/bin/node"
    cp "$TMP/node-$NODE_VERSION-linux-$a/LICENSE" "$DEST/node/" ;;
esac

echo "== TypeScript + pyright (npm)"
npm install --prefix "$DEST/js" --omit=dev --no-fund --no-audit --loglevel=error \
  typescript-language-server "typescript@6" pyright
bin_of() { node -p "require('$(native "$DEST")/js/node_modules/$1/package.json').bin['$2']"; }
TS_BIN="js/node_modules/typescript-language-server/$(bin_of typescript-language-server typescript-language-server)"
PY_BIN="js/node_modules/pyright/$(bin_of pyright pyright-langserver)"
rm -rf "$DEST/js/node_modules/.bin" "$DEST/js/package-lock.json"

echo "== gopls (go build)"
mkdir -p "$DEST/gopls" "$TMP/gopls"
( cd "$TMP/gopls" && go mod init edittr-gopls >/dev/null 2>&1 && go get golang.org/x/tools/gopls@latest >/dev/null 2>&1 )
go_build() { ( cd "$TMP/gopls" && CGO_ENABLED=0 GOOS=$1 GOARCH=$2 go build -trimpath -ldflags="-s -w" -o "$3" golang.org/x/tools/gopls ); }
case $PLATFORM in
  windows-x64) go_build windows amd64 "$DEST/gopls/gopls.exe" ;;
  macos-universal)
    go_build darwin arm64 "$TMP/gopls-arm64"
    go_build darwin amd64 "$TMP/gopls-amd64"
    lipo_or_copy "$DEST/gopls/gopls" "$TMP/gopls-arm64" "$TMP/gopls-amd64" ;;
  linux-x86_64) go_build linux amd64 "$DEST/gopls/gopls" ;;
  linux-aarch64) go_build linux arm64 "$DEST/gopls/gopls" ;;
esac

echo "== servers.json"
"$PY" - "$DEST/servers.json" "$EXE" "$CLANGD" "$TS_BIN" "$PY_BIN" <<'EOF'
import json, sys
out, exe, clangd, ts, py = sys.argv[1:]
node = f"node/node{exe}"
m = {
    "rust-analyzer": {"exe": f"rust-analyzer/rust-analyzer{exe}"},
    "gopls": {"exe": f"gopls/gopls{exe}"},
    "typescript-language-server": {"exe": node, "args": ["@" + ts]},
    "pyright-langserver": {"exe": node, "args": ["@" + py]},
}
if clangd:
    m["clangd"] = {"exe": clangd}
json.dump(m, open(out, "w"), indent=2)
print(json.dumps(m, indent=2))
EOF

echo "== bundle size"
du -sh "$DEST"
