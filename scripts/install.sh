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
#   bash scripts/install.sh --server https://conversations.example.com
#   INSTALL_DIR=/usr/local/bin bash scripts/install.sh
#
# --server takes the web origin of the conversations server this machine
# should use (scheme, host and optional port; no path). It only changes the
# next-step commands printed at the end: the installer itself never contacts
# that server or saves it anywhere. Host access in the web app passes its own
# origin here, so the printed login command targets the same server.
#
# Review this script before running it.

set -euo pipefail

REPO="SjuulArtreides/conversations"
BIN="conversations"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
TAG=""
SERVER=""

err() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

while [ $# -gt 0 ]; do
  case "$1" in
    --server)
      [ $# -ge 2 ] || err "--server needs a URL, e.g. --server https://conversations.example.com"
      SERVER="$2"
      shift 2 ;;
    --server=*)
      SERVER="${1#--server=}"
      shift ;;
    -*) err "unknown option: $1" ;;
    *)
      [ -z "$TAG" ] || err "unexpected argument: $1"
      TAG="$1"
      shift ;;
  esac
done

# Accept only a canonical web origin, exactly what a browser reports as
# `location.origin`: http(s), a lowercase DNS name, IPv4 or bracketed IPv6
# host, and an optional port. Its characters are therefore all shell-inert,
# and it is still quoted when printed. Validate before downloading anything.
if [ -n "$SERVER" ]; then
  while [ "${SERVER%/}" != "$SERVER" ]; do SERVER="${SERVER%/}"; done
  origin_regex='^https?://([a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)*|\[[0-9a-f:.]+\])(:[0-9]{1,5})?$'
  [[ "$SERVER" =~ $origin_regex ]] \
    || err "--server must be a web origin such as https://conversations.example.com (lowercase, no path, query or credentials); got '$SERVER'"
fi

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
# Resolve to an absolute path so the printed fallback command works from any
# directory, even when INSTALL_DIR was given relative.
INSTALL_DIR="$(CDPATH='' cd -- "$INSTALL_DIR" && pwd)"
mv -f "$tmp/$BIN" "$INSTALL_DIR/$BIN"

# Quote a value for pasting into any POSIX-style shell (bash, zsh, sh).
shell_quote() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

# Drop trailing slashes so "/home/me/.local/bin/" matches "/home/me/.local/bin".
strip_slashes() {
  local dir="$1"
  while [ "${#dir}" -gt 1 ] && [ "${dir%/}" != "$dir" ]; do
    dir="${dir%/}"
  done
  printf '%s' "$dir"
}

# The bare command name is only safe to suggest when PATH resolves it to the
# copy just installed; otherwise an older install elsewhere would run instead.
on_path=false
want="$(strip_slashes "$INSTALL_DIR")"
IFS=: read -r -a path_dirs <<< "$PATH"
# `${a[@]+...}` keeps `set -u` happy on macOS bash 3.2 if PATH is empty.
for dir in ${path_dirs[@]+"${path_dirs[@]}"}; do
  [ -n "$dir" ] || continue
  if [ -x "$dir/$BIN" ] && [ ! -d "$dir/$BIN" ]; then
    [ "$(strip_slashes "$dir")" = "$want" ] && on_path=true
    break
  fi
done

echo "Installed $BIN to $INSTALL_DIR/$BIN"
echo
if $on_path; then
  program="$BIN"
else
  program="$(shell_quote "$INSTALL_DIR/$BIN")"
fi
if [ -n "$SERVER" ]; then
  server_flag="--server $(shell_quote "$SERVER")"
  echo "Next, log in to $SERVER on this machine:"
  echo
  echo "  $program login $server_flag"
  echo
  echo "After login it offers to start the host. Keep the host running."
  echo
  echo "Already logged in to this server, or it does not require login? Start"
  echo "the host directly instead:"
  echo
  echo "  $program host $server_flag"
else
  echo "Next, start the host on this machine and keep it running:"
  echo
  echo "  $program host"
  echo
  echo "Without --server, CONVERSATIONS_SERVER or a server saved by login, the"
  echo "CLI uses http://localhost:8080. For any other server, log in first with"
  echo "its web address in place of the example (login offers to start the host):"
  echo
  echo "  $program login --server https://conversations.example.com"
  echo
  echo "or add the same --server <URL> to the host command above. Host access in"
  echo "the web app shows both commands with its address filled in."
fi
echo
echo "The host prints its public descriptor and where to paste it in"
echo "Host access."
if ! $on_path; then
  echo
  echo "Your PATH does not run this copy as plain '$BIN', so the commands above"
  echo "use the full path. No shell config change is needed. To use plain"
  echo "'$BIN' in this terminal only, run:"
  echo
  echo "  export PATH=$(shell_quote "$INSTALL_DIR"):\"\$PATH\""
fi
