#!/usr/bin/env python3
"""Native proof for terminal file drops: replay the host's production paste
sequences into a real OpenCode/Codex TUI over a PTY and report what attached.

Never submits (no CR/LF is sent) and makes no model call: Codex is pointed at a
local listener whose connection count must stay 0.

Isolation: Codex runs with a fresh HOME and CODEX_HOME. OpenCode keeps the real
HOME (its background server will not start otherwise) and is isolated through
XDG_* directories only, so it can still see ~/.opencode.

Cleanup only touches what this run created: the PTY child's process group, the
probe work dir, and any background `opencode serve --service` whose own
environment contains this run's unique work dir. Pre-existing or concurrent
OpenCode services are never signalled.

  PROBE_OUT=$PWD/tmp/probe-out cargo test -p conversations-cli emit_probe -- --ignored
  scripts/probe-terminal-files.py opencode ~/.opencode/bin/opencode
  scripts/probe-terminal-files.py codex tmp/codex-0.163.0/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex

The emitter's staging dir is removed after the run only when it is a direct
child of the canonical temp dir named like the emitter's (`conv-drop-XXXXXX`)
and carries the emitter's ownership marker matching the token in the sequences
file; otherwise it is left and reported. `probe-terminal-files.py selftest`
checks that refusal logic.
"""
import argparse
import fcntl
import json
import os
import pty
import re
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import uuid

ANSI = re.compile(
    r"\x1b\[[0-9;?<>=]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[()][A-Z0-9]|\x1b[=>]"
)
EMITTER_ROOT = re.compile(r"^conv-drop-[A-Za-z0-9]{6}$")
TRANSFER_DIR = re.compile(r"^t-[0-9a-f-]{36}$")
OWNER_FILE = ".probe-owner"
TOKEN = re.compile(r"^[0-9a-f-]{36}$")


def parse_args():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("agent", choices=["opencode", "codex", "selftest"])
    ap.add_argument("binary", nargs="?")
    ap.add_argument("--sequences", default="tmp/probe-out/sequences.json")
    ap.add_argument("--gap-ms", type=int)
    ap.add_argument(
        "--edge",
        action="store_true",
        help="replay the trailing-whitespace names (Codex only); none can attach, they must land as path text",
    )
    ap.add_argument("--startup", type=float, default=15)
    return ap.parse_args()


def owned_services(work):
    """Background OpenCode services whose environment names this run's work dir."""
    pids = subprocess.run(
        ["pgrep", "-f", "opencode serve --service"], capture_output=True, text=True
    ).stdout.split()
    owned = []
    for pid in pids:
        env = subprocess.run(
            ["ps", "eww", "-o", "command=", "-p", pid], capture_output=True, text=True
        ).stdout
        if work in env:
            owned.append(int(pid))
    return owned


def safe_emitter_root(root, token):
    """Return the emitter's staging dir only if it provably belongs to the probe
    emitter: validated direct child of the canonical temp dir, no symlink, the
    emitter's name shape, only `t-<uuid>` dirs plus an `.probe-owner` marker
    file whose content equals the token in the sequences file. Production
    staging dirs share the name shape but never carry the marker."""
    if not root or not token or not TOKEN.match(token):
        return None
    tmp = os.path.realpath(tempfile.gettempdir())
    real = os.path.realpath(root)
    if os.path.islink(root) or os.path.dirname(real) != tmp:
        return None
    if not EMITTER_ROOT.match(os.path.basename(real)) or not os.path.isdir(real):
        return None
    marker = os.path.join(real, OWNER_FILE)
    if os.path.islink(marker) or not os.path.isfile(marker):
        return None
    if open(marker).read().strip() != token:
        return None
    return real if all(
        n == OWNER_FILE or TRANSFER_DIR.match(n) for n in os.listdir(real)
    ) else None


def selftest():
    tmp = os.path.realpath(tempfile.gettempdir())
    token = "0" * 8 + "-0000-0000-0000-" + "0" * 12
    prod = os.path.join(tmp, "conv-drop-" + uuid.uuid4().hex[:6])
    os.mkdir(prod)
    try:
        # Production-shaped dir (same name, t-<uuid> layout, no marker): refused.
        os.makedirs(os.path.join(prod, "t-" + "1" * 8 + "-1111-1111-1111-" + "1" * 12))
        assert safe_emitter_root(prod, token) is None
        # Marker with the wrong token: refused.
        open(os.path.join(prod, OWNER_FILE), "w").write("f" * 8 + "-ffff-ffff-ffff-" + "f" * 12)
        assert safe_emitter_root(prod, token) is None
        # Matching marker: accepted.
        open(os.path.join(prod, OWNER_FILE), "w").write(token)
        assert safe_emitter_root(prod, token) == os.path.realpath(prod)
        # Unexpected extra entry: refused.
        open(os.path.join(prod, "extra"), "w").close()
        assert safe_emitter_root(prod, token) is None
        # Missing/invalid token: refused.
        assert safe_emitter_root(prod, None) is None
        assert safe_emitter_root(prod, "not-a-token") is None
    finally:
        shutil.rmtree(prod, ignore_errors=True)
    print("selftest ok")


def resolve_binary(binary, path):
    """Absolute agent path, resolved in the parent before fork/chdir so a
    relative path or PATH command still works from the probe work dir."""
    found = binary if "/" in binary else shutil.which(binary, path=path)
    found = os.path.abspath(found) if found else None
    if not found or not os.path.isfile(found) or not os.access(found, os.X_OK):
        sys.exit("agent binary not found or not executable: " + binary)
    return found


def build_command(args, work, home, port):
    env = {k: v for k, v in os.environ.items() if k in ("PATH", "TMPDIR", "LANG")}
    env.update(
        TERM="xterm-256color",
        XDG_DATA_HOME=home + "/d",
        XDG_CONFIG_HOME=home + "/c",
        XDG_CACHE_HOME=home + "/k",
        XDG_STATE_HOME=home + "/s",
    )
    if args.agent == "opencode":
        env["HOME"] = os.environ["HOME"]
        return [args.binary], env
    env.update(HOME=home, CODEX_HOME=home + "/codex", PROBE_KEY="dummy")
    os.makedirs(env["CODEX_HOME"], exist_ok=True)
    cwd = os.path.realpath(work)
    provider = (
        '{name="probe",base_url="http://127.0.0.1:%d/v1",'
        'env_key="PROBE_KEY",wire_api="responses"}' % port
    )
    argv = [
        args.binary,
        "-c", 'model_provider="probe"',
        "-c", "model_providers.probe=" + provider,
        "-c", 'projects."%s".trust_level="trusted"' % cwd,
        "-c", "check_for_update_on_startup=false",
    ]
    return argv, env


def run_probe(args, seq, work, home, listener, hits):
    gap = (args.gap_ms if args.gap_ms is not None else seq["gap_ms"]) / 1000
    pastes = [bytes.fromhex(h) for h in seq["edge_pastes" if args.edge else "pastes"]]
    argv, env = build_command(args, work, home, listener.getsockname()[1])

    argv[0] = resolve_binary(argv[0], env.get("PATH"))

    pid, fd = pty.fork()
    if pid == 0:
        try:
            os.chdir(work)
            os.execve(argv[0], argv, env)
        except BaseException as e:
            try:
                os.write(2, ("probe: cannot exec %s: %s\n" % (argv[0], e)).encode())
            finally:
                os._exit(127)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 45, 140, 0, 0))
    buf = bytearray()
    write_failed = False
    try:

        def drain(seconds):
            end = time.time() + seconds
            while time.time() < end:
                ready, _, _ = select.select([fd], [], [], 0.1)
                if not ready:
                    continue
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    return
                buf.extend(data)
                if b"\x1b[6n" in data:
                    os.write(fd, b"\x1b[1;1R")
                if b"\x1b[c" in data or b"\x1b[0c" in data:
                    os.write(fd, b"\x1b[?1;2c")
                if b"\x1b[?u" in data:
                    os.write(fd, b"\x1b[?0u")

        drain(args.startup)
        mark = len(buf)
        for paste in pastes:
            try:
                os.write(fd, paste)
            except OSError:
                write_failed = True
                break
            drain(gap)
        drain(2.5)
        shown = ANSI.sub("", buf[mark:].decode("utf8", "replace"))
    finally:
        try:
            os.killpg(pid, signal.SIGKILL)  # pty.fork child leads its own group
        except (ProcessLookupError, PermissionError):  # macOS: EPERM once the child exited
            pass
        _, status = os.waitpid(pid, 0)
        os.close(fd)
        for service in owned_services(work):
            os.kill(service, signal.SIGTERM)

    # Reached only when the probe body did not raise, so no original error is masked.
    tail = ANSI.sub("", buf.decode("utf8", "replace"))[-600:]
    if os.WIFEXITED(status) and os.WEXITSTATUS(status) == 127 and "probe: cannot exec" in tail:
        sys.exit("agent failed to start: " + tail.strip())
    if write_failed:
        sys.exit("agent exited before paste; startup tail: " + tail)

    lines = [ln.strip() for ln in shown.replace("\r", "\n").split("\n") if ln.strip()]
    images = sorted(
        set(re.findall(r"\[Image #?\d+\]", shown)),
        key=lambda s: int(re.sub(r"\D", "", s)),
    )
    result = {
        "agent": args.agent,
        "gap_ms": int(gap * 1000),
        "pastes": len(pastes),
        "startup_bytes": mark,
        "image_placeholders": images,
        "path_text_leaked": [ln[:100] for ln in lines if "conv-drop-" in ln],
        "model_connections": len(hits),
        "cr_or_lf_sent": any(b"\r" in p or b"\n" in p for p in pastes),
    }
    return result, lines


def main():
    args = parse_args()
    if args.agent == "selftest":
        return selftest()
    if not args.binary:
        sys.exit("binary path required")
    if args.edge and args.agent != "codex":
        sys.exit("--edge is Codex only: OpenCode attaches by extension, so names ending in whitespace never attach")
    seq = json.load(open(args.sequences))
    work = tempfile.mkdtemp(prefix="probe-")
    home = os.path.join(work, "home")
    os.makedirs(home)

    hits = []
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(5)

    def accept():
        while True:
            try:
                conn, _ = listener.accept()
            except OSError:
                return
            hits.append(1)
            conn.close()

    threading.Thread(target=accept, daemon=True).start()
    try:
        result, lines = run_probe(args, seq, work, home, listener, hits)
    finally:
        listener.close()
        shutil.rmtree(work, ignore_errors=True)
        root = safe_emitter_root(seq.get("root"), seq.get("token"))
        if root:
            shutil.rmtree(root, ignore_errors=True)
        elif seq.get("root"):
            print("left in place (not a verified emitter dir):", seq["root"], file=sys.stderr)

    print(json.dumps(result, indent=1, ensure_ascii=False))
    print("tail of screen text:", " | ".join(lines)[-400:])
    if args.edge:
        # Codex sniffs images by file extension, so a name ending in whitespace
        # never attaches whatever the paste syntax; it must stay visible text.
        ok = not result["image_placeholders"] and bool(result["path_text_leaked"])
    else:
        ok = len(result["image_placeholders"]) == result["pastes"] and not result["path_text_leaked"]
    ok = ok and not hits
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
