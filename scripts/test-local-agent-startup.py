# /// script
# requires-python = ">=3.11"
# dependencies = ["websockets>=14,<16", "pyte>=0.8,<0.9"]
# ///
"""Check installed Codex/OpenCode TUIs through a running localhost host.

Types a marker without submitting a model prompt. Ends only the sessions it
creates. --opencode-pure skips external plugins for this invocation only.
"""
import argparse
import asyncio
import base64
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import urllib.request

import pyte
from websockets.asyncio.client import connect

ROOT = Path(__file__).resolve().parents[1]


async def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", required=True)
    parser.add_argument("--runtime-dir", required=True, type=Path)
    parser.add_argument("--cookie-file", required=True, type=Path)
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target")
    parser.add_argument("--opencode-pure", action="store_true")
    args = parser.parse_args()
    if not args.server.startswith("http://127.0.0.1:"):
        parser.error("agent smoke tests require an explicit 127.0.0.1 HTTP server")
    cookie = args.cookie_file.read_text().strip()
    cli = args.target_dir.resolve() / "debug/conversations"
    (ROOT / "tmp").mkdir(exist_ok=True)
    folder = Path(tempfile.mkdtemp(prefix="agent-startup-", dir=ROOT / "tmp"))

    def http(path, body=None):
        request = urllib.request.Request(args.server + path,
            data=json.dumps(body).encode() if body is not None else None,
            headers={"Cookie": cookie, "Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=5) as response:
            return json.load(response)

    for agent, prompt in (("codex", "Ask Codex"), ("opencode", "Ask anything")):
        program = shutil.which(agent)
        if not program:
            raise RuntimeError(f"{agent} is not installed on PATH")
        command = [program]
        if agent == "opencode" and args.opencode_pure:
            command.append("--pure")
        result = subprocess.check_output([str(cli), "host", "--runtime-dir", str(args.runtime_dir),
            "start", "--agent", agent, "--cwd", str(ROOT), "--", *command], text=True, timeout=20)
        ident = result.strip().split()[-1]
        try:
            deadline = time.monotonic() + 15
            while not any(s["id"] == ident for s in http("/api/live")["live_sessions"]):
                if time.monotonic() > deadline:
                    raise TimeoutError(f"{agent} failed to register")
                await asyncio.sleep(.1)
            async with connect(args.server.replace("http:", "ws:") + f"/api/live/{ident}/pty/ws?replay=history",
                               additional_headers={"Cookie": cookie}, close_timeout=1) as ws:
                screen = pyte.Screen(80, 24)
                stream = pyte.Stream(screen)
                marker = f"conversations-{agent}-input-proof"
                typed = False
                async with asyncio.timeout(30):
                    while True:
                        frame = json.loads(await ws.recv())
                        if "b64" not in frame:
                            continue
                        screen.resize(lines=frame.get("rows", screen.lines), columns=frame.get("cols", screen.columns))
                        stream.feed(base64.b64decode(frame["b64"]).decode(errors="replace"))
                        text = "\n".join(screen.display)
                        (folder / f"{agent}-screen.txt").write_text(text)
                        if prompt in text and not typed:
                            # Escape dismisses a possible update notice; no upgrade
                            # or model submission is part of this smoke test.
                            await ws.send(json.dumps({"type": "input", "b64": base64.b64encode(b"\x1b").decode()}))
                            await asyncio.sleep(.2)
                            await ws.send(json.dumps({"type": "input", "b64": base64.b64encode(marker.encode()).decode()}))
                            typed = True
                        if typed and marker in text:
                            print(f"PASS: {agent} prompt renders and remote text input reaches its TUI", flush=True)
                            break
        finally:
            http(f"/api/live/{ident}/complete", {})
    print(f"Screen evidence: {folder}")


if __name__ == "__main__":
    asyncio.run(main())
