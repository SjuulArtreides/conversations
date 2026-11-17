#!/usr/bin/env python3
"""Real server/host startup recovery after interrupted local account creation."""
import argparse
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import sys
import tempfile
import time

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--target-dir", type=Path, default=root / "target")
args = parser.parse_args()
(root / "tmp").mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(prefix="host-dev-recovery-", dir=root / "tmp") as scratch:
    sandbox = Path(scratch)
    # Reproduce interruption between writing credentials and registering them.
    credentials = json.dumps({"email": "local-cmux@example.test", "password": secrets.token_urlsafe(24)})
    login = sandbox / "login.json"
    login.write_text(credentials)
    login.chmod(0o600)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    command = [sys.executable, str(root / "scripts/local-host-dev.py"), "--backend", "normal",
        "--target-dir", str(args.target_dir.resolve()), "--sandbox-dir", str(sandbox), "--port", str(port)]
    environment = {k: v for k, v in os.environ.items() if not k.startswith(("CONVERSATIONS_", "CMUX_"))}
    status_command = [str(args.target_dir.resolve() / "debug/conversations"),
        "host", "--runtime-dir", str(sandbox / "host"), "status", "--json"]
    def host_alive():
        return subprocess.run(status_command, capture_output=True, timeout=3).returncode == 0
    for iteration in range(2):
        with open(sandbox / "runner.log", "w+") as log:
            child = subprocess.Popen(command, cwd=root, env=environment, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline:
                    if child.poll() is not None:
                        log.seek(0)
                        raise AssertionError(f"runner exited before host startup (attempt {iteration + 1}): {log.read()[-1500:]}")
                    if host_alive():
                        break
                    time.sleep(.1)
                else:
                    raise AssertionError("runner never started its host")
                assert login.read_text() == credentials, "recovery must retain saved credentials"
                assert (sandbox / "cookie").stat().st_mode & 0o077 == 0
            finally:
                child.terminate()
                child.wait(timeout=12)
        assert not host_alive(), "runner must stop its host"
        with socket.socket() as sock:
            assert sock.connect_ex(("127.0.0.1", port)) != 0, "runner must stop its server"
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        occupied = listener.getsockname()[1]
        collision = subprocess.run(command[:-1] + [str(occupied)], cwd=root, env=environment,
            capture_output=True, text=True, timeout=5)
        assert collision.returncode != 0
        assert "already has a listener" in collision.stderr
print("PASS: account recovery, clean shutdown, and occupied-port refusal")
