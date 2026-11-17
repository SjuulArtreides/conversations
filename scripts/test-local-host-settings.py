#!/usr/bin/env python3
"""Exercise the persistent setting through the real CLI and child process."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--target-dir", type=Path, default=root / "target")
args = parser.parse_args()
cli = args.target_dir.resolve() / "debug/arrakis-conversations"
(root / "tmp").mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(prefix="host-settings-", dir=root / "tmp") as scratch:
    folder = Path(scratch)
    base = [str(cli), "host", "--runtime-dir", str(folder / "host")]
    clean = {k: v for k, v in os.environ.items() if not k.startswith(("CMUX_", "CONVERSATIONS_"))}
    clean["CONVERSATIONS_HOST_SETTINGS_FILE"] = str(folder / "local-host.json")
    def check(label, expected, env):
        result = folder / "child.json"
        result.unlink(missing_ok=True)
        program = "import json,os,sys; from pathlib import Path; Path(sys.argv[1]).write_text(json.dumps({'wrapped': bool(os.environ.get('CONVERSATIONS_WRAPPED')), 'args':sys.argv[2:]}))"
        words = ["two words", "", "$literal; 'quote'"]
        master, slave = os.openpty()
        try:
            child = subprocess.run([*base, "shell", "--", sys.executable, "-c", program, str(result), *words],
                env={**clean, **env}, timeout=10, stdin=slave, stdout=slave, stderr=subprocess.PIPE)
            assert child.returncode == 0, (label, child.stderr.decode())
        finally:
            os.close(slave)
            os.close(master)
        observed = json.loads(result.read_text())
        assert observed == {"wrapped": expected, "args": words}, (label, observed)
    cmux = {"CMUX_WORKSPACE_ID": "test-workspace"}
    check("default off", False, cmux)
    subprocess.run([*base, "configure", "--auto-wrap", "true"], env=clean, check=True, capture_output=True)
    check("enabled in cmux", True, cmux)
    check("outside cmux", False, {})
    check("workspace opt out", False, {**cmux, "CONVERSATIONS_CAPTURE": "0"})
    configured = subprocess.run([str(cli), "host", "configure", "--auto-wrap", "false"],
        env=clean, check=True, capture_output=True)
    assert str(folder / "local-host.json") in configured.stdout.decode()
    check("disabled immediately", False, cmux)
    (folder / "local-host.json").write_text("invalid")
    check("bad config keeps shell usable", False, cmux)
print("PASS: persistent auto-wrap setting, cmux-only scope, workspace override, immediate disable, literal argv, and invalid-config fallback")
