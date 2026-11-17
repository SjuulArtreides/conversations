#!/usr/bin/env python3
"""Real nested PTY: retain last visible size across cmux's hidden bootstrap."""

import argparse
import fcntl
import json
import os
from pathlib import Path
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import uuid

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument(
    "--cli", type=Path, default=root / "target/release/conversations"
)
a = parser.parse_args()
(root / "tmp").mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(prefix="cmux-size-", dir=root / "tmp") as scratch:
    folder = Path(scratch)
    runtime = folder / "h"
    runtime.mkdir(mode=0o700)
    ident = str(uuid.uuid4())
    surface = str(uuid.uuid4())
    cmux = folder / "cmux"
    cmux.write_text(f"""#!{sys.executable}
import json,os
print(json.dumps({{'terminals':[{{'surface_id':{surface!r},'hosted_view_in_headless_bootstrap_window':os.environ.get('TEST_BACKGROUND')=='1'}}]}}))
""")
    cmux.chmod(0o700)
    report = folder / "size.json"
    child = (
        "import os,json,time;from pathlib import Path\nwhile True:\n s=os.get_terminal_size();Path("
        + repr(str(report))
        + ").write_text(json.dumps([s.columns,s.lines]));time.sleep(.05)"
    )
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.startswith(("CONVERSATIONS_", "CMUX_"))
    }
    env.update(
        CMUX_WORKSPACE_ID=str(uuid.uuid4()),
        CMUX_SURFACE_ID=surface,
        CMUX_BUNDLED_CLI_PATH=str(cmux),
    )

    def wait_size(expected):
        until = time.monotonic() + 5
        while time.monotonic() < until:
            try:
                if json.loads(report.read_text()) == expected:
                    return
            except (FileNotFoundError, json.JSONDecodeError):
                pass
            time.sleep(0.05)
        raise AssertionError(
            f"expected {expected}; got {report.read_text() if report.exists() else 'no child'}"
        )

    for background, outer in [(False, (132, 42)), (True, (99, 35))]:
        report.unlink(missing_ok=True)
        master, slave = os.openpty()
        fcntl.ioctl(
            slave, termios.TIOCSWINSZ, struct.pack("HHHH", outer[1], outer[0], 0, 0)
        )
        p = subprocess.Popen(
            [
                str(a.cli.resolve()),
                "host",
                "--runtime-dir",
                str(runtime),
                "wrap",
                "--session-id",
                ident,
                "--",
                sys.executable,
                "-c",
                child,
            ],
            env={**env, "TEST_BACKGROUND": str(int(background))},
            stdin=slave,
            stdout=slave,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        try:
            wait_size([132, 42])
            if background:
                fcntl.ioctl(
                    slave, termios.TIOCSWINSZ, struct.pack("HHHH", 47, 143, 0, 0)
                )
                os.kill(p.pid, signal.SIGWINCH)
                wait_size([143, 47])
        finally:
            p.terminate()
            p.wait(timeout=5)
            os.close(slave)
            os.close(master)
print(
    "PASS: background restore retains last visible dimensions; real local resize takes over"
)
