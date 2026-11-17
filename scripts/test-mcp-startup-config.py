#!/usr/bin/env python3
"""Fallback prerequisites fail before initialization; ordinary fixed MCP still starts."""
import json
import os
from pathlib import Path
import select
import subprocess
import tempfile
import uuid

ROOT = Path(__file__).resolve().parents[1]
CLI = ROOT / "target/debug/arrakis-conversations"
INITIALIZE = json.dumps({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"startup-test","version":"1"}}}) + "\n"

def main():
    env = dict(os.environ)
    env.pop("CONVERSATIONS_HOST_DIR", None)
    command = [str(CLI), "--server", "http://127.0.0.1:1", "mcp", "serve", "--session", str(uuid.uuid4())]
    (ROOT / "tmp").mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="mcp-config-", dir=ROOT / "tmp") as directory:
        for runtime in [None, directory]:
            configured = dict(env)
            if runtime is not None: configured["CONVERSATIONS_HOST_DIR"] = runtime
            result = subprocess.run(command + ["--delivery", "conversations"], input=INITIALIZE,
                                    capture_output=True, text=True, env=configured, timeout=5)
            assert result.returncode != 0 and not result.stdout, result
            assert "CONVERSATIONS_HOST_DIR" in result.stderr, result.stderr
        child = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 env=env, text=True)
        try:
            child.stdin.write(INITIALIZE); child.stdin.flush()
            assert select.select([child.stdout], [], [], 5)[0]
            assert "result" in json.loads(child.stdout.readline())
        finally:
            child.stdin.close()
            try: child.wait(timeout=3)
            except subprocess.TimeoutExpired: child.kill(); child.wait()
    print("PASS fallback missing/invalid host rejects before advertisement; ordinary fixed-session MCP initializes")

if __name__ == "__main__": main()
