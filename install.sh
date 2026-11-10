#!/bin/sh
# veneer installer: download, verify, install, then run veneer with any given arguments.
#
#   curl -fsSL https://raw.githubusercontent.com/LukasParke/veneer/main/install.sh | sh -s -- apply github:acme/app@main
#
# Environment:
#   VENEER_VERSION       release tag to install (default: latest)
#   VENEER_RELEASE_URL   base URL holding release assets (default: GitHub Releases)
#   VENEER_INSTALL_DIR   install directory (default: /usr/local/bin as root, else ~/.local/bin)
set -eu

repo="LukasParke/veneer"
version="${VENEER_VERSION:-latest}"

say() { printf 'veneer-install: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

os="$(uname -s)"
arch="$(uname -m)"
case "$arch" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) die "unsupported architecture: $arch" ;;
esac
case "$os" in
  Linux) target="$arch-unknown-linux-musl" ;;
  Darwin) target="$arch-apple-darwin" ;;
  *) die "unsupported OS: $os" ;;
esac

if [ -n "${VENEER_RELEASE_URL:-}" ]; then
  base="$VENEER_RELEASE_URL"
elif [ "$version" = latest ]; then
  base="https://github.com/$repo/releases/latest/download"
else
  base="https://github.com/$repo/releases/download/$version"
fi

if [ -n "${VENEER_INSTALL_DIR:-}" ]; then
  dir="$VENEER_INSTALL_DIR"
elif [ "$(id -u)" = 0 ]; then
  dir=/usr/local/bin
else
  dir="$HOME/.local/bin"
fi

fetch() {
  if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then wget -qO "$2" "$1"
  else die "need curl or wget"; fi
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  else die "need sha256sum or shasum to verify the download"; fi
}

asset="veneer-$target.tar.xz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "downloading $base/$asset"
fetch "$base/$asset" "$tmp/$asset" || die "download failed"
fetch "$base/$asset.sha256" "$tmp/$asset.sha256" || die "checksum download failed"
expected="$(cut -d' ' -f1 < "$tmp/$asset.sha256")"
actual="$(sha256 "$tmp/$asset")"
[ "$expected" = "$actual" ] || die "checksum mismatch for $asset (expected $expected, got $actual)"

if ! command -v xz >/dev/null 2>&1 && ! tar --help 2>&1 | grep -q -- '-J'; then
  die "need xz to unpack $asset"
fi
tar -xJf "$tmp/$asset" -C "$tmp"
bin="$(find "$tmp" -type f -name veneer | head -n1)"
[ -n "$bin" ] || die "archive did not contain a veneer binary"
mkdir -p "$dir"
install -m 0755 "$bin" "$dir/veneer" 2>/dev/null || { cp "$bin" "$dir/veneer" && chmod 0755 "$dir/veneer"; }
say "installed $("$dir/veneer" --version) to $dir/veneer"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) say "note: $dir is not on PATH" ;;
esac

if [ "$#" -gt 0 ]; then
  exec "$dir/veneer" "$@"
fi
