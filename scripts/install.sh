#!/usr/bin/env bash
# Install the CLI AFTER binary releases have been published in this repository.
# The initial source snapshot has no binary releases; build from source instead
# (see README.md). This script is retained for future compatible release assets.
#
# The macOS binaries aren't notarized (no Apple Developer ID), so a raw download
# trips Gatekeeper ("Apple could not verify ... is free of malware"). This
# script downloads a compatible release archive via `gh`, verifies its checksum,
# strips the `com.apple.quarantine` attribute, and drops the binary on your
# PATH — so you never see that prompt.
#
# Usage:
#   bash scripts/install.sh                 # latest release
#   bash scripts/install.sh v0.2.21         # a specific tag
#   INSTALL_DIR=/usr/local/bin bash scripts/install.sh
#
# Review this script before running it; releases are not yet available.

set -euo pipefail

REPO="SjuulArtreides/conversations"
BIN="arrakis-conversations"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
TAG="${1:-}"

err() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

command -v gh >/dev/null 2>&1 || err "the GitHub CLI (gh) is required — install it, then run 'gh auth login'"
gh auth status >/dev/null 2>&1 || err "gh is not authenticated — run 'gh auth login' first"

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
  Darwin/arm64) target="aarch64-apple-darwin" ;;
  Linux/x86_64) target="x86_64-unknown-linux-gnu" ;;
  Darwin/x86_64)
    err "no Intel macOS archive format is configured — build from source; see README.md" ;;
  Linux/aarch64 | Linux/arm64) target="aarch64-unknown-linux-gnu" ;;
  *) err "unsupported platform: $os/$arch" ;;
esac

# Resolve the tag to install (default: the latest published release).
if [ -z "$TAG" ]; then
  TAG="$(gh release view --repo "$REPO" --json tagName --jq .tagName)" \
    || err "no usable release found; the initial snapshot is source-only — see README.md"
fi

# Archive names embed the bare version (no leading 'v'); the tag carries the 'v'.
archive="${BIN}-${TAG#v}-${target}.tar.gz"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $archive ($TAG)…"
gh release download "$TAG" --repo "$REPO" \
  --pattern "$archive" --pattern "$archive.sha256" --dir "$tmp" \
  || err "failed to download $archive for $TAG"

# Verify the checksum. The sidecar is 'shasum -a 256' output ('<hash>  <file>'),
# which both sha256sum and shasum accept with -c.
echo "Verifying checksum…"
(
  cd "$tmp"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$archive.sha256"
  else
    shasum -a 256 -c "$archive.sha256"
  fi >/dev/null
) || err "checksum verification failed"

tar -xzf "$tmp/$archive" -C "$tmp"
[ -f "$tmp/$BIN" ] || err "archive did not contain $BIN"

# Strip the quarantine attribute so macOS Gatekeeper doesn't block the binary.
if [ "$os" = "Darwin" ]; then
  xattr -d com.apple.quarantine "$tmp/$BIN" 2>/dev/null || true
fi

mkdir -p "$INSTALL_DIR"
chmod +x "$tmp/$BIN"
mv -f "$tmp/$BIN" "$INSTALL_DIR/$BIN"

echo "Installed $BIN to $INSTALL_DIR/$BIN"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo "note: $INSTALL_DIR is not on your PATH — add it, e.g.:"
    echo "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.zshrc" ;;
esac

"$INSTALL_DIR/$BIN" --version \
  || err "installed binary failed to execute: $INSTALL_DIR/$BIN"
