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
target/debug/arrakis-conversations login
target/debug/arrakis-conversations host
```

`CONVERSATIONS_SERVER` sets the same server as the global `--server` option.
Without either, commands use the default saved by `login`, then
`http://localhost:8080`. `login` only offers to save a default when it can
prompt in a terminal.

The host runs in the foreground and serves its local management UI on
loopback. Open it from another shell that has the same
`CONVERSATIONS_SERVER` with `target/debug/arrakis-conversations open`. To
connect your browser and give
someone else access to your host, follow the
[trust onboarding walkthrough](docs/trust-onboarding.md).

The executable retains its existing name, `arrakis-conversations`. Local
Claude Code, Codex, OpenCode, and cmux installations are separate prerequisites
for the integrations you choose to use. The optional `adk code` launcher is
an existing Arrakis DevKit integration, not a requirement for this application.

## Docker

Build the bundled server and frontend locally:

```sh
docker build -f deploy/Dockerfile -t conversations:local .
```

The build selects your machine's architecture. Both Linux ARM64 and x86-64
are supported; see [deployment](deploy/README.md#image-architecture) for a
multi-platform build published under one tag.

Prebuilt public images and release binaries are not published by this initial
snapshot. See [deployment](deploy/README.md) for storage, authentication, and
Kubernetes configuration. Run one server replica; Postgres alone does not
provide multi-replica live-session support.

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
