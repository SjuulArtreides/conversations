#!/usr/bin/env bash
# Install a published conversations CLI release.
#
# Downloads the release archive and its `.sha256` sidecar over HTTPS from the
# public GitHub release (no GitHub login required), verifies the checksum
# before extracting, checks `--version` on the staged binary, and only then
# replaces the installed copy. If any step fails, the existing binary (if any)
# is left untouched.
#
# Usage:
#   bash scripts/install.sh                 # latest release
#   bash scripts/install.sh v0.69.1         # a specific vX.Y.Z tag
#   INSTALL_DIR=/usr/local/bin bash scripts/install.sh
#
# Review this script before running it.

set -euo pipefail

REPO="SjuulArtreides/conversations"
BIN="conversations"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
TAG="${1:-}"

err() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

command -v curl >/dev/null 2>&1 || err "curl is required"
command -v tar  >/dev/null 2>&1 || err "tar is required"

os="$(uname -s)"
arch="$(uname -m)"
# Under Rosetta, `uname -m` reports x86_64 on Apple Silicon — detect the
# translation and correct the arch so arm64 Macs don't hit the Intel branch.
if [ "$os" = "Darwin" ] && [ "$arch" = "x86_64" ]; then
  if [ "$(sysctl -in sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
    arch="arm64"
  fi
fi
case "$os/$arch" in
  Darwin/arm64)              target="aarch64-apple-darwin" ;;
  Darwin/x86_64)             target="x86_64-apple-darwin" ;;
  Linux/x86_64)              target="x86_64-unknown-linux-gnu" ;;
  Linux/aarch64|Linux/arm64) target="aarch64-unknown-linux-gnu" ;;
  *) err "unsupported platform: $os/$arch" ;;
esac

valid_tag='^v[0-9]+\.[0-9]+\.[0-9]+$'

# Resolve the tag to install. GitHub redirects /releases/latest to the newest
# published, non-draft release's tag page; follow the redirect anonymously and
# parse the tag out of the final URL.
if [ -z "$TAG" ]; then
  effective="$(curl -fsIL --proto '=https' --tlsv1.2 \
    -o /dev/null -w '%{url_effective}' \
    "https://github.com/$REPO/releases/latest")" \
    || err "failed to resolve the latest release tag"
  TAG="${effective##*/}"
fi
[[ "$TAG" =~ $valid_tag ]] \
  || err "tag '$TAG' is not a strict vX.Y.Z release tag"

# Archive names embed the bare version (no leading 'v'); the tag carries the 'v'.
archive="${BIN}-${TAG#v}-${target}.tar.gz"
base="https://github.com/$REPO/releases/download/$TAG"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $archive ($TAG)…"
curl -fL --proto '=https' --tlsv1.2 \
  -o "$tmp/$archive"        "$base/$archive" \
  -o "$tmp/$archive.sha256" "$base/$archive.sha256" \
  || err "failed to download $archive for $TAG"

# Verify the checksum before extracting. sha256sum/shasum -c tolerates stray
# lines in the sidecar (warns and still exits 0), so parse it ourselves: it
# must be exactly one line with a 64-hex digest naming this archive.
echo "Verifying checksum…"
sidecar="$tmp/$archive.sha256"

line_count=$(awk 'END { print NR }' "$sidecar")
[ "$line_count" = "1" ] \
  || err "checksum sidecar must contain exactly one line (got $line_count)"

field_count=$(awk 'NR == 1 { print NF }' "$sidecar")
[ "$field_count" = "2" ] \
  || err "checksum sidecar line must have exactly two fields (got $field_count)"

expected_hash=$(awk 'NR == 1 { print $1 }' "$sidecar")
expected_name=$(awk 'NR == 1 { print $2 }' "$sidecar")
expected_name="${expected_name#\*}"

[ "$expected_name" = "$archive" ] \
  || err "checksum sidecar names '$expected_name', expected '$archive'"

hex_regex='^[0-9a-fA-F]{64}$'
[[ "$expected_hash" =~ $hex_regex ]] \
  || err "checksum sidecar digest is not a 64-char hex value"

if command -v sha256sum >/dev/null 2>&1; then
  actual_hash=$(sha256sum "$tmp/$archive" | awk '{print $1}')
else
  actual_hash=$(shasum -a 256 "$tmp/$archive" | awk '{print $1}')
fi

expected_lc=$(printf '%s' "$expected_hash" | tr '[:upper:]' '[:lower:]')
actual_lc=$(printf '%s' "$actual_hash" | tr '[:upper:]' '[:lower:]')
[ "$expected_lc" = "$actual_lc" ] \
  || err "checksum verification failed — refusing to extract"

tar -xzf "$tmp/$archive" -C "$tmp"
[ -f "$tmp/$BIN" ] || err "archive did not contain $BIN"
chmod +x "$tmp/$BIN"

# Check the staged binary before replacing any existing install. On Darwin
# without a Developer ID signature, Gatekeeper may refuse to launch a freshly
# downloaded binary; surface that here instead of clobbering the installed
# copy.
"$tmp/$BIN" --version >/dev/null \
  || err "staged binary failed to execute — keeping the existing $INSTALL_DIR/$BIN"

mkdir -p "$INSTALL_DIR"
mv -f "$tmp/$BIN" "$INSTALL_DIR/$BIN"

echo "Installed $BIN to $INSTALL_DIR/$BIN"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo "note: $INSTALL_DIR is not on your PATH — add it, e.g.:"
    echo "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.zshrc" ;;
esac
