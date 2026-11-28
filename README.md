# Conversations

Self-hosted coding-agent sessions for Claude Code, Codex, and OpenCode.
Browse and search transcripts, share sessions, and connect a local host for
live terminals and agent-to-agent messaging. The application includes a Rust
server and CLI, and a React frontend. SQLite and Postgres are supported.

Conversations is developed as a personal side project. It is not sponsored or
maintained by Arrakis as a company project.

## Run from source

Install [Nix](https://nixos.org/download/) and enter the development shell:

```sh
git clone https://github.com/SjuulArtreides/conversations.git
cd conversations
nix develop
```

Build the frontend, then run the server:

```sh
npm --prefix frontend ci
npm --prefix frontend run build
STATIC_DIR=frontend/dist BIND_ADDR=127.0.0.1:8080 COOKIE_SECURE=false cargo run --locked -p conversations-server
```

Open <http://localhost:8080>. Authentication is enabled by default. The
`COOKIE_SECURE=false` setting is only for this loopback HTTP example; use HTTPS
and secure cookies for shared deployments. Password registration does not
verify email ownership; use Google SSO for a shared deployment where identity
verification matters.

In another development shell, point every CLI command at that server,
authenticate, and start the host:

```sh
export CONVERSATIONS_SERVER=http://localhost:8080
cargo build --locked -p conversations-cli
target/debug/conversations login
target/debug/conversations host
```

`CONVERSATIONS_SERVER` sets the same server as the global `--server` option.
Without either, commands use the default saved by `login`, then
`http://localhost:8080`. `login` only offers to save a default when it can
prompt in a terminal.

The host runs in the foreground and serves its local management UI on
loopback. Open it from another shell that has the same
`CONVERSATIONS_SERVER` with `target/debug/conversations open`. To
connect your browser and give
someone else access to your host, follow the
[trust onboarding walkthrough](docs/trust-onboarding.md).

To delegate work to another agent or exchange messages between sessions, see
[handoffs and agent-to-agent communication](docs/agent-handoffs-and-messaging.md).
To let your own machines control each other, pair them once: see
[Pair your own machines](docs/pair-your-machines.md).

The CLI executable is `conversations`. Its configuration and data
directories keep their existing `arrakis-conversations` names, so existing
logins and host state carry over. Local
Claude Code, Codex, OpenCode, and cmux installations are separate prerequisites
for the integrations you choose to use. The optional `adk code` launcher is
an existing Arrakis DevKit integration, not a requirement for this application.

## Install the CLI from a release

Tagged releases publish prebuilt `conversations` binaries as GitHub release
assets: see the
[latest release](https://github.com/SjuulArtreides/conversations/releases/latest).
Pick the archive for your platform from the table below, verify the `.sha256`
sidecar before extracting, and install the binary under `$HOME/.local/bin`
(no sudo required). The example below uses the 0.69.1 archive for macOS Apple
Silicon; replace `ARCHIVE` with the filename that matches your platform and
release version:

```sh
ARCHIVE=conversations-0.69.1-aarch64-apple-darwin.tar.gz
shasum -a 256 -c "$ARCHIVE.sha256"
tar -xzf "$ARCHIVE"
mkdir -p "$HOME/.local/bin"
install -m 0755 conversations "$HOME/.local/bin/conversations"
"$HOME/.local/bin/conversations" --version
```

Make sure `$HOME/.local/bin` is on your `PATH`. The sidecar uses
`shasum -a 256` output that both `sha256sum -c` and `shasum -a 256 -c` accept.

For a one-shot install on Linux or macOS (Apple Silicon or Intel), download
`scripts/install.sh` first so you can review it, then run it with `bash`:

```sh
curl -fLO https://raw.githubusercontent.com/SjuulArtreides/conversations/main/scripts/install.sh
bash install.sh                 # latest release
# bash install.sh v0.69.1       # a specific vX.Y.Z tag
# INSTALL_DIR=/usr/local/bin bash install.sh
```

The script picks the archive for your platform (with Rosetta detection on
Apple Silicon), resolves the latest tag by following the public
`/releases/latest` redirect, verifies the `.sha256` sidecar before extracting,
checks `--version` on the staged binary, and only then replaces the installed
copy — so a failed download never clobbers a working binary. It does not strip
the macOS quarantine attribute; remove it deliberately after reviewing the
download if Gatekeeper blocks the binary.

Supported targets:

| Platform | Target triple |
|----------|---------------|
| Linux x86-64 | `x86_64-unknown-linux-gnu` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |

### Update an installed CLI

```sh
conversations self-update --check   # report whether a newer release exists; change nothing
conversations self-update           # install the latest release over this binary
```

`self-update` downloads from the public
`SjuulArtreides/conversations` GitHub release only, anonymously over HTTPS
with bounded sizes and timeouts. It installs the latest strict `vX.Y.Z`
release only when it is newer than the running version; it never downgrades
or reinstalls the same version. It picks the archive for the architecture of
the running executable (so an Intel binary under Rosetta stays Intel), checks
the `.sha256` sidecar before extracting only the regular `conversations`
member, stages it in a temporary file next to the running executable, checks
that the staged binary's `--version` reports the expected release, and then
atomically renames it over the old binary. If any step fails, the existing
binary is left in place and the staged file is removed. Nothing is written
outside the executable's directory.

It never uses `sudo`. If the executable's directory is not writable by you
(for example a system-wide install in `/usr/local/bin`, or a read-only or
package-manager-managed location such as a Nix store), `self-update` stops
before downloading the release; update that install the way it was installed,
for example by rerunning `scripts/install.sh` with the same `INSTALL_DIR`. It
also does not restart anything: an already-running `conversations host` or
agent keeps running the old version until you restart it.

### Tagging a release

The release workflow (`.github/workflows/release.yml`) builds natively for
every target, verifies each binary, and uploads archives plus checksums to a
draft release that it publishes once every build succeeds. To cut a release,
bump `workspace.package.version` in `Cargo.toml`, merge the bump to `main`,
then tag that commit with a strict `vX.Y.Z` tag matching the version and
push it:

```sh
git tag -a v0.70.0 -m "v0.70.0"
git push origin v0.70.0
```

The workflow refuses to run unless the tag is strict `vX.Y.Z` and matches
the workspace version.

## Docker

Build the bundled server and frontend locally:

```sh
docker build -f deploy/Dockerfile -t conversations:local .
```

The build selects your machine's architecture. Both Linux ARM64 and x86-64
are supported; see [deployment](deploy/README.md#image-architecture) for a
multi-platform build published under one tag.

CI publishes the server image to `ghcr.io/sjuulartreides/conversations` for
Linux amd64 and arm64 after checks pass on `main`, tagged `sha-<full-commit-sha>`.
Each release also tags its image `vX.Y.Z`; see [releases](DEVELOPMENT.md#releases).
See [deployment](deploy/README.md) for storage, authentication, and Kubernetes
configuration. Run one server replica; Postgres
alone does not provide multi-replica live-session support.

## Security

Live terminal write access allows commands to run on the host. Approve only
people and agent peers you trust. Session transcripts and terminal output can
contain credentials and private data: review them before uploading or sharing.
Do not expose an authentication-disabled server to other users.

This initial public snapshot is not a security certification. See
[SECURITY.md](SECURITY.md) before exposing a deployment.

## Development and license

See [DEVELOPMENT.md](DEVELOPMENT.md) for local checks. This repository starts
from a sanitized source snapshot without private development history or
internal release automation.

The application is MIT licensed. See [LICENSE](LICENSE). Third-party code
under `vendor/` retains its own license and attribution.
