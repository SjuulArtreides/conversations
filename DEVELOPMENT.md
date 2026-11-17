# Local development

## Set up

Clone the repository and enter the Nix development shell. With Nix and direnv
installed, run:

```sh
direnv allow
prek install
```

The development shell provides Rust, Node.js, and database tools.

## Run locally

Start the server:

```sh
AUTH_ENABLED=false BIND_ADDR=127.0.0.1:8080 cargo run -p conversations-server
```

In another terminal, start the frontend:

```sh
cd frontend
npm ci
VITE_API_TARGET=http://127.0.0.1:8080 npm run dev
```

The frontend development server proxies API requests to the server on port 8080.
The server enables authentication by default. `AUTH_ENABLED=false` turns it off for
this local run: sessions are readable and writable without signing in. Use that only
on a trusted machine. To try login locally, omit `AUTH_ENABLED` and add
`COOKIE_SECURE=false`; you can then register with an email and password. For a
shared deployment, follow the [deployment guide](deploy/README.md).

Run the CLI against the local server:

```sh
cargo run -p conversations-cli -- --server http://localhost:8080 upload
cargo run -p conversations-cli -- --server http://localhost:8080 host
cargo run -p conversations-cli -- --server http://localhost:8080 start-agent claude
```

## Check changes

```sh
test_home="$(mktemp -d /tmp/cv.XXXXXX)"
test_home="$(cd "$test_home" && pwd -P)"
env HOME="$test_home" TMPDIR="$test_home" CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" cargo test --locked --workspace -- --test-threads=4
prek run --all-files
cd frontend
npm test
```

Commit hooks check file hygiene, Rust formatting and Clippy, and frontend
formatting and linting. The pre-push hook runs the Rust tests.

The isolated test home prevents host-settings tests from reading your installed
Conversations preferences. Preserve `CARGO_HOME` as shown so Cargo can reuse its
dependency cache.
The short, canonical temporary directory avoids Unix socket path-length limits
and `/tmp` symlink differences on macOS. Some integration tests require separately installed agents,
a browser, or a Postgres database and are skipped unless their prerequisites
are supplied. The container and local toolchain use Rust 1.96; CI currently
checks a newer compiler too.

On macOS, fixtures that invoke `/usr/bin/python3` need the real Xcode Command
Line Tools rather than Nix's SDK-only developer directory. Before running the
tests, run `export DEVELOPER_DIR="$(/usr/bin/xcode-select -p)"`. For native-agent Node
fixtures, run `export CONVERSATIONS_TEST_NODE="$(command -v node)"` before tests isolate
their child processes' `PATH`.

Historical migration files are byte-for-byte preserved to keep database
migration checksums compatible. Some comments refer to design documents that
are not distributed in this public snapshot.

For the application overview, see [README.md](README.md).
For a local Kubernetes setup, see [the deployment runbook](deploy/DEPLOY.md).

For cmux transcript and PR ingestion, build the CLI and server, then run:

```sh
uv run scripts/test-cmux-pr-ingestion.py --target-dir target
uv run scripts/test-cmux-pr-ingestion.py --target-dir target --backend cmux --cmux-window "$TEST_CMUX_WINDOW_ID"
```

The regression uses isolated credentials, synthetic native transcripts, and real
PTY wrappers. It checks late identity binding, same-directory isolation, PR
backfill, incremental messages, and host reconnect without duplicated messages
or terminated panes. The cmux variant requires an existing isolated test window. Set
`TEST_CMUX_WINDOW_ID` to its UUID. It creates and closes only its own native
workspaces and preserves the supplied window.

For stale-wrapper and cmux inventory recovery behavior, run:

```sh
uv run scripts/test-cmux-compatibility.py --target-dir target
```

This test uses a fake cmux inventory and real host, server, and wrapper
processes. It never creates native windows.
