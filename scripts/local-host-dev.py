#!/usr/bin/env python3
"""Serve an isolated localhost sandbox and generate a cmux host launch command.

Build Rust binaries and frontend/dist first. No production credentials/configuration
are read or changed. Stop this foreground process with Ctrl-C.
"""
import argparse
import json
import os
from pathlib import Path
import secrets
import shlex
import signal
import socket
import subprocess
import time
import urllib.error
import urllib.request


def main():
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser()
    parser.add_argument("--target-dir", type=Path, default=root / "target")
    parser.add_argument("--port", type=int, default=8097)
    parser.add_argument("--backend", choices=("normal", "cmux"),
                        help="Start the host too; cmux must launch this runner inside a terminal")
    parser.add_argument("--sandbox-dir", type=Path, default=root / "tmp/cmux-dev")
    args = parser.parse_args()
    if args.backend == "cmux" and not os.environ.get("CMUX_WORKSPACE_ID"):
        parser.error("cmux backend must be launched inside cmux so its socket ancestry is preserved")
    target = args.target_dir.resolve()
    sandbox = args.sandbox_dir.resolve()
    sandbox.mkdir(parents=True, exist_ok=True, mode=0o700)
    sandbox.chmod(0o700)
    runtime = sandbox / "host"
    runtime.mkdir(exist_ok=True, mode=0o700)
    url = f"http://127.0.0.1:{args.port}"
    cli = target / "debug/arrakis-conversations"
    server = target / "debug/conversations-server"
    if not cli.is_file() or not server.is_file() or not (root / "frontend/dist/index.html").is_file():
        raise SystemExit("Build conversations-cli, conversations-server and frontend first; see DEVELOPMENT.md")
    credential_file = sandbox / "login.json"
    fresh = not credential_file.exists()
    if fresh:
        credentials = {"email": "local-cmux@example.test", "password": secrets.token_urlsafe(24)}
        with open(credential_file, "x", opener=lambda p, f: os.open(p, f, 0o600)) as output:
            json.dump(credentials, output)
    else:
        credentials = json.loads(credential_file.read_text())
    # Keep the localhost sandbox independent of the caller's server/auth config.
    environment = {key: os.environ[key] for key in ("PATH", "HOME", "TMPDIR", "LANG", "LC_ALL")
                   if key in os.environ}
    environment.update(BIND_ADDR=f"127.0.0.1:{args.port}",
                       DATABASE_URL=f"sqlite://{sandbox}/conversations.db?mode=rwc",
                       STATIC_DIR=str(root / "frontend/dist"), AUTH_ENABLED="true",
                       PASSWORD_AUTH_ENABLED="true", COOKIE_SECURE="false", SHUTDOWN_DRAIN_SECS="0")
    # Never send the generated test account to a server already using this port.
    with socket.socket() as probe:
        if probe.connect_ex(("127.0.0.1", args.port)) == 0:
            raise SystemExit(f"Localhost port {args.port} already has a listener")
    with open(sandbox / "server.log", "a") as log:
        process = subprocess.Popen([str(server)], env=environment, cwd=root, stdout=log, stderr=log)
        def stop(*_):
            raise KeyboardInterrupt
        signal.signal(signal.SIGTERM, stop)
        host = None
        try:
            for _ in range(100):
                if process.poll() is not None:
                    raise SystemExit(f"Server failed; see {sandbox}/server.log")
                try:
                    with urllib.request.urlopen(url + "/healthz", timeout=.5):
                        # A competing binder can race the initial port probe.
                        # Give our process time to report bind failure before login.
                        time.sleep(.2)
                        if process.poll() is not None:
                            raise SystemExit(f"Server failed; see {sandbox}/server.log")
                        break
                except OSError:
                    time.sleep(.1)
            else:
                raise SystemExit("Local server did not become ready")
            def authenticate(path):
                request = urllib.request.Request(url + path, data=json.dumps(credentials).encode(),
                                                 headers={"Content-Type": "application/json"})
                with urllib.request.urlopen(request, timeout=10) as response:
                    return response.headers["Set-Cookie"].split(";", 1)[0]
            try:
                cookie = authenticate("/auth/register" if fresh else "/auth/login")
            except urllib.error.HTTPError as error:
                if fresh or error.code != 401:
                    raise
                # A previous first run may have stopped after saving credentials
                # but before registration. Reuse those credentials; registration
                # rejects an existing account rather than changing its password.
                cookie = authenticate("/auth/register")
            cookie_file = sandbox / "cookie"
            with open(cookie_file, "w", opener=lambda p, f: os.open(p, f, 0o600)) as output:
                output.write(cookie)
            command = [str(cli), "--server", url, "host", "--runtime-dir", str(runtime),
                       "--backend", args.backend or "cmux", "--cookie-file", str(cookie_file)]
            launch = sandbox / "start-host.sh"
            launch.write_text("#!/bin/sh\ncd " + shlex.quote(str(root)) + "\nexec " + shlex.join(command) + "\n")
            launch.chmod(0o700)
            if args.backend:
                host_environment = environment.copy()
                host_environment.update({k: v for k, v in os.environ.items() if k.startswith("CMUX_")})
                # The server must not inherit Conversations state, but a host
                # launched inside a wrapper needs its parent identity so it
                # can release its own terminal from the previous host.
                for key in ("CONVERSATIONS_WRAPPED", "CONVERSATIONS_HOST_DIR", "CONVERSATIONS_WRAPPER_PID"):
                    if key in os.environ:
                        host_environment[key] = os.environ[key]
                host = subprocess.Popen(command, env=host_environment, cwd=root)
                print(f"Local app: {url}\nLocal sign-in details: {credential_file}\n"
                      f"Starting {args.backend} host. Ctrl-C stops this host and local server.", flush=True)
                while process.poll() is None and host.poll() is None:
                    time.sleep(.2)
                if host.poll() is not None:
                    raise SystemExit(f"Host exited with status {host.returncode}")
                raise SystemExit(f"Server exited with status {process.returncode}")
            print(f"Local app: {url}\nLocal sign-in details: {credential_file}\n"
                  f"Run this in a NEW cmux workspace: {launch}\n"
                  "Ctrl-C stops only this local server.", flush=True)
            process.wait()
        except KeyboardInterrupt:
            pass
        finally:
            if host is not None and host.poll() is None:
                host.terminate()
                host.wait(timeout=10)
            process.terminate()
            process.wait(timeout=10)


if __name__ == "__main__":
    main()
