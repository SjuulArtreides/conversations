#!/usr/bin/env python3
"""Record a coding agent's RAW PTY output as a mirror fixture, resizes included.

The CLI's own `CONVERSATIONS_PTY_CAPTURE` tee is the normal way to record a
fixture (see `crates/server/tests/fixtures/pty/README.md`), but it records what
the CLI *mirrors* — already past `PtyEraseTracker::process`, so `ESC[3J` is gone
— and it cannot resize a terminal you are typing in. This harness sits one level
lower: it spawns the agent on its own PTY, logs every raw chunk, and drives
`TIOCSWINSZ` + `SIGWINCH` on a timer. Use it when the behaviour under test
involves raw erase semantics or a reflow.

    python3 scripts/capture-agent-pty.py out.jsonl -- codex

Output is one `{"cols","rows","b64"}` line per chunk: the fixture shape the
server mirror consumes. No input is ever sent to the agent, so nothing is
submitted to a model. Run it from a directory the agent already trusts —
otherwise you capture a trust prompt instead of the startup you wanted. Keep the
recorded screen free of anything private; the bytes get committed verbatim.
"""

import argparse
import base64
import fcntl
import json
import os
import pty
import select
import signal
import struct
import sys
import termios
import time

# (delay_seconds, cols, rows). The first entry is the initial size; later
# entries resize the PTY at that offset to force a reflow.
SCHEDULE = ((0.0, 100, 30), (7.0, 120, 34), (10.0, 84, 26))
DURATION = 14.0
POLL = 0.05
TERM_GRACE = 0.5


def set_size(fd, cols, rows):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def capture(command, schedule, duration):
    """Run `command` on a PTY and return its output chunks as fixture frames."""
    pid, fd = pty.fork()
    if pid == 0:
        cols, rows = schedule[0][1], schedule[0][2]
        env = dict(
            os.environ, TERM="xterm-256color", COLUMNS=str(cols), LINES=str(rows)
        )
        try:
            os.execvpe(command[0], command, env)
        finally:
            os._exit(1)

    frames = []
    pending = list(schedule)
    cols, rows = schedule[0][1], schedule[0][2]
    started = time.monotonic()
    try:
        while True:
            elapsed = time.monotonic() - started
            if elapsed >= duration:
                break
            if pending and elapsed >= pending[0][0]:
                _, cols, rows = pending.pop(0)
                set_size(fd, cols, rows)
                # The agent only reflows when it hears about the new size.
                os.kill(pid, signal.SIGWINCH)
            if not select.select([fd], [], [], POLL)[0]:
                continue
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break  # the child closed its end
            if not chunk:
                break
            frames.append(
                {
                    "cols": cols,
                    "rows": rows,
                    "b64": base64.b64encode(chunk).decode("ascii"),
                }
            )
    finally:
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                os.kill(pid, sig)
            except ProcessLookupError:
                break
            time.sleep(TERM_GRACE)
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:
            pass
        os.close(fd)
    return frames


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", help="JSONL fixture path to write")
    parser.add_argument(
        "command",
        nargs=argparse.REMAINDER,
        help="agent to spawn, after `--` (e.g. -- codex)",
    )
    parser.add_argument(
        "--duration",
        type=float,
        default=DURATION,
        help=f"seconds to record before terminating the agent (default {DURATION})",
    )
    args = parser.parse_args()

    command = [arg for arg in args.command if arg != "--"]
    if not command:
        parser.error("name the agent to spawn after `--`, e.g. -- codex")

    frames = capture(command, SCHEDULE, args.duration)
    if not frames:
        sys.exit(f"{command[0]} produced no output")

    with open(args.output, "w", encoding="utf-8") as handle:
        for frame in frames:
            handle.write(json.dumps(frame) + "\n")
    total = sum(len(base64.b64decode(frame["b64"])) for frame in frames)
    print(f"{len(frames)} frames, {total} bytes -> {args.output}")


if __name__ == "__main__":
    main()
