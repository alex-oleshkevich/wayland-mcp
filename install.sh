#!/bin/sh
# Install wayland-mcp from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/alex-oleshkevich/wayland-mcp/main/install.sh | sh
#
# Options (or the matching environment variables):
#   --version vX.Y.Z   release to install (WAYLAND_MCP_VERSION; default: latest)
#   --dir DIR          install directory (WAYLAND_MCP_INSTALL_DIR; default: ~/.local/bin)
set -eu

REPO="alex-oleshkevich/wayland-mcp"
version="${WAYLAND_MCP_VERSION:-}"
dir="${WAYLAND_MCP_INSTALL_DIR:-$HOME/.local/bin}"

die() { echo "install.sh: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || die "--version needs a value"; version="$2"; shift 2 ;;
    --dir) [ $# -ge 2 ] || die "--dir needs a value"; dir="$2"; shift 2 ;;
    -h|--help)
      echo "usage: install.sh [--version vX.Y.Z] [--dir DIR]"
      echo "  --version  release to install (WAYLAND_MCP_VERSION; default: latest)"
      echo "  --dir      install directory (WAYLAND_MCP_INSTALL_DIR; default: ~/.local/bin)"
      exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

[ "$(uname -s)" = "Linux" ] || die "wayland-mcp runs only on Linux with a Wayland session (this is $(uname -s))"
case "$(uname -m)" in
  x86_64|amd64) arch=amd64 ;;
  aarch64|arm64) arch=arm64 ;;
  *) die "no release build for CPU $(uname -m) (available: x86_64, aarch64)" ;;
esac

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

if [ -z "$version" ]; then
  # /releases/latest redirects to /releases/tag/<tag>; no API token needed.
  url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest") \
    || die "could not reach GitHub to find the latest release"
  version="${url##*/}"
  case "$version" in v*) ;; *) die "could not determine the latest release (got '$url')" ;; esac
fi
case "$version" in v*) ;; *) version="v$version" ;; esac

name="wayland-mcp-$version-linux-$arch"
base="https://github.com/$REPO/releases/download/$version"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading wayland-mcp $version (linux-$arch)..."
curl -fsSL -o "$tmp/$name.tar.gz" "$base/$name.tar.gz" || die "download failed: $base/$name.tar.gz"

if curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" 2>/dev/null; then
  expected=$(grep " $name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
  [ -n "$expected" ] || die "SHA256SUMS has no entry for $name.tar.gz"
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)
  else
    actual=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1)
  fi
  [ "$actual" = "$expected" ] || die "checksum mismatch for $name.tar.gz (expected $expected, got $actual)"
  echo "Checksum verified."
else
  echo "Note: $version publishes no SHA256SUMS; skipping checksum verification." >&2
fi

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$dir"
install -m 0755 "$tmp/$name/wayland-mcp" "$dir/wayland-mcp"
echo "Installed $dir/wayland-mcp ($("$dir/wayland-mcp" --version))"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "Note: $dir is not on your PATH. Add it, or use the full path in your MCP client config." ;;
esac
