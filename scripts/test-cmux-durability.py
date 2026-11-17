# /// script
# requires-python = ">=3.11"
# dependencies = ["websockets>=14,<16"]
# ///
"""Restart-durability fault injection: real server, host, wrappers and PTYs.

Only cmux and Claude are simulated. Each scenario isolates HOME, the checkpoint
file, the runtime directory, the database and every process it starts. It never
touches the installed cmux app, the user's host or their saved sessions.

Build first, then run one scenario at a time:

    cargo build -p conversations-cli -p conversations-server
    uv run scripts/test-cmux-durability.py --browser-resize-attached
    uv run scripts/test-cmux-durability.py --restart-during-host-reconnect
    uv run scripts/test-cmux-durability.py --host-restart-without-binding
    uv run scripts/test-cmux-durability.py --end-across-second-host-restart
    uv run scripts/test-cmux-durability.py --upgrade-from-prelock-wrapper --old-cli <pre-lock CLI>
    uv run scripts/test-cmux-durability.py --crash-before-end-delivery
    uv run scripts/test-cmux-durability.py --crash-before-pending-end-delivery

The crash scenarios need a debug build: only debug builds honour
CONVERSATIONS_HOST_CRASH_AT.
"""

import argparse
import asyncio
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import secrets
import shlex
import signal
import subprocess
import sys
import tempfile
import time
import uuid

sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parents[1]
HOST_WORKSPACE = "33333333-3333-4333-8333-333333333333"
HOST_SURFACE = "44444444-4444-4444-8444-444444444444"
WINDOW = "55555555-5555-4555-8555-555555555555"


class State:
    """Fake cmux state shared by concurrent cmux CLI invocations."""

    def __init__(self, folder):
        self.path = folder / "cmux-state.json"
        self.lock = open(folder / "cmux.lock", "a")

    def __enter__(self):
        fcntl.flock(self.lock, fcntl.LOCK_EX)
        self.data = (
            json.loads(self.path.read_text())
            if self.path.exists()
            else {"workspaces": [], "bindings": {}}
        )
        return self.data

    def __exit__(self, *exc):
        if exc[0] is None:
            temporary = self.path.with_suffix(".tmp")
            temporary.write_text(json.dumps(self.data))
            temporary.replace(self.path)
        fcntl.flock(self.lock, fcntl.LOCK_UN)


def fake_claude(folder, args):
    import struct
    import termios

    ident = args[args.index("--resume") + 1] if "--resume" in args else args[args.index("--session-id") + 1]
    with (folder / "launches.jsonl").open("a") as out:
        out.write(json.dumps({"id": ident, "args": args, "pid": os.getpid()}) + "\n")
    print("AGENT_READY", flush=True)
    deadline = time.monotonic() + 120
    previous_size = None
    while time.monotonic() < deadline:
        requested = folder / f"resize-agent-{os.getpid()}.json"
        if requested.exists():
            cols, rows = json.loads(requested.read_text())
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
            requested.unlink()
        terminal_size = list(os.get_terminal_size(0))
        if terminal_size != previous_size:
            path = folder / f"agent-size-{os.getpid()}.json"
            temporary = path.with_suffix(".tmp")
            temporary.write_text(json.dumps(terminal_size))
            temporary.replace(path)
            previous_size = terminal_size
            print(f"GRID {terminal_size}", flush=True)
        crash = folder / f"crash-{os.getpid()}"
        if crash.exists():
            sys.exit(int(crash.read_text()))
        time.sleep(0.05)


def fake_terminal(folder, surface, command):
    import select
    import struct
    import termios

    master, slave = os.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    child = subprocess.Popen(["/bin/sh", "-c", command], stdin=slave, stdout=slave,
                             stderr=slave, start_new_session=True)
    os.close(slave)
    (folder / "terminals").mkdir(exist_ok=True)
    (folder / "terminals" / surface).write_text(str(child.pid))
    try:
        while child.poll() is None:
            requested = folder / f"resize-terminal-{surface}.json"
            if requested.exists():
                cols, rows = json.loads(requested.read_text())
                fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
                requested.unlink()
            if select.select([master], [], [], 0.1)[0]:
                try:
                    data = os.read(master, 65536)
                except OSError:
                    break
                with (folder / f"terminal-{surface}.log").open("ab") as out:
                    out.write(data)
    finally:
        if child.poll() is None:
            child.terminate()
        child.wait(timeout=5)
        os.close(master)
        # cmux removes a pane whose process exited.
        with State(folder) as state:
            state["workspaces"] = [w for w in state["workspaces"] if w["surface"] != surface]


def fake_cmux(folder, args):
    if args[:2] == ["rpc", "system.ping"]:
        print("{}")
    elif "tree" in args:
        delay = folder / "tree-delay"
        if delay.exists():
            time.sleep(float(delay.read_text()))
        with State(folder) as state:
            workspaces = [{"id": HOST_WORKSPACE, "panes": [{"surfaces": [{"id": HOST_SURFACE}]}]}]
            workspaces += [{"id": w["id"], "panes": [{"surfaces": [{"id": w["surface"]}]}]}
                           for w in state["workspaces"]]
        print(json.dumps({"windows": [{"id": WINDOW, "workspaces": workspaces}]}))
    elif args[:2] == ["surface", "resume"]:
        surface = args[args.index("--surface") + 1].lower()
        if args[2] == "set" and (folder / "fail-bind").exists():
            sys.exit(1)
        with State(folder) as state:
            if args[2] == "set":
                state["bindings"][surface] = {
                    "kind": args[args.index("--kind") + 1],
                    "checkpoint_id": args[args.index("--checkpoint") + 1],
                }
            elif args[2] == "clear":
                state["bindings"].pop(surface, None)
            print(json.dumps({"resume_binding": state["bindings"].get(surface)}))
    elif args[:2] == ["rpc", "workspace.create"]:
        params = json.loads(args[2])
        workspace, surface = str(uuid.uuid4()), str(uuid.uuid4())
        with State(folder) as state:
            state["workspaces"].append({"id": workspace, "surface": surface})
        env = {**os.environ, **params["workspace_env"],
               "CMUX_SURFACE_ID": surface, "CMUX_WORKSPACE_ID": workspace}
        subprocess.Popen([sys.executable, __file__, "--terminal", surface, params["initial_command"]],
                         env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL, start_new_session=True)
        print(json.dumps({"workspace_id": workspace, "surface_id": surface}))
    elif args[:2] == ["rpc", "debug.terminals"]:
        print(json.dumps({"terminals": []}))
    elif args[:2] == ["--json", "workspace"]:
        print(json.dumps({"workspaces": []}))
    else:
        print("{}")


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


def checkpoint_path(home):
    base = "Library/Application Support" if sys.platform == "darwin" else ".config"
    return home / base / "arrakis-conversations/open-sessions.json"


async def main():
    spec = importlib.util.spec_from_file_location("host_test", ROOT / "scripts/test-local-host.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target")
    scenarios = parser.add_mutually_exclusive_group(required=True)
    scenarios.add_argument("--browser-resize-attached", action="store_true",
                           help="browser sizing applies to an attached PTY until its local window resizes")
    scenarios.add_argument("--restart-during-host-reconnect", action="store_true",
                           help="Restart Session while a wrapper reconnects to a restarted host must not run two agents")
    scenarios.add_argument("--host-restart-without-binding", action="store_true",
                           help="a host restart must not relaunch a running conversation whose pane lacks restore metadata")
    scenarios.add_argument("--end-across-second-host-restart", action="store_true",
                           help="an End that a wrapper missed must survive another host restart before it reconnects")
    scenarios.add_argument("--upgrade-from-prelock-wrapper", action="store_true",
                           help="a CLI upgrade must not relaunch a conversation whose wrapper predates the liveness lock")
    scenarios.add_argument("--crash-before-end-delivery", action="store_true",
                           help="a host crash between accepting End and writing it to a registered wrapper must not lose it")
    scenarios.add_argument("--crash-before-pending-end-delivery", action="store_true",
                           help="a host crash while handing a missed End to a reconnecting wrapper must not lose it")
    parser.add_argument("--old-cli", type=Path,
                        help="a CLI built before wrapper liveness locks, for --upgrade-from-prelock-wrapper")
    args = parser.parse_args()
    if args.upgrade_from_prelock_wrapper:
        parser.error(
            "The prelock host probe depends on retired unsigned control routes and has no "
            "signed host relay. Rewrite it for the signed-host upgrade boundary; current "
            "signed lifecycle coverage: cargo test -p conversations-cli --test host_lifecycle"
        )
    (ROOT / "tmp").mkdir(exist_ok=True)
    folder = Path(tempfile.mkdtemp(prefix="durability-", dir=ROOT / "tmp"))
    t = module.Test(args.target_dir.resolve(), folder)
    home = folder / "home"
    home.mkdir()
    bins = folder / "bin"
    bins.mkdir()
    for name, mode in [("cmux", "--cmux"), ("claude", "--claude")]:
        path = bins / name
        path.write_text("#!/bin/sh\nexec " + shlex.join([sys.executable, str(Path(__file__).resolve()), mode]) + ' "$@"\n')
        path.chmod(0o700)
    (home / ".zshrc").write_text(f'export PATH="{bins}:$PATH"\n')
    t.environment.update(
        HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"), SHELL="/bin/zsh",
        DURABILITY_DIR=str(folder), PATH=f"{bins}:{os.environ['PATH']}",
        CMUX_WORKSPACE_ID=HOST_WORKSPACE, CMUX_SURFACE_ID=HOST_SURFACE,
        CMUX_BUNDLED_CLI_PATH=str(bins / "cmux"),
        BIND_ADDR=t.url.removeprefix("http://"), DATABASE_URL=f"sqlite://{folder}/test.db?mode=rwc",
        AUTH_ENABLED="true", PASSWORD_AUTH_ENABLED="true", COOKIE_SECURE="false", SHUTDOWN_DRAIN_SECS="0",
    )
    checkpoints = checkpoint_path(home)

    def launches():
        path = folder / "launches.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def saved():
        return json.loads(checkpoints.read_text())["servers"].get(t.url, []) if checkpoints.exists() else []

    host_argv = [str(t.cli), "--server", t.url, "host", "--runtime-dir", str(t.host_dir),
                 "--backend", "cmux", "--cookie-file", str(folder / "cookie")]
    try:
        t.spawn([str(t.server)], "server")
        await t.wait(lambda: t.http("/healthz"), "server ready")
        t.http("/auth/register", {"email": "durability@example.test", "password": secrets.token_urlsafe(24)})
        (folder / "cookie").write_text(t.cookie)
        (folder / "cookie").chmod(0o600)

        if args.host_restart_without_binding or args.upgrade_from_prelock_wrapper:
            # cmux metadata writes can fail (timeout, older cmux). The pane then
            # carries no Conversations binding while its wrapper keeps running.
            (folder / "fail-bind").touch()
        if args.upgrade_from_prelock_wrapper:
            # The old host launches old wrappers: they hold no liveness lock.
            old_argv = [str(args.old_cli.resolve()), *host_argv[1:]]
            host = t.spawn(old_argv, "old-host")
        else:
            host = t.spawn(host_argv, "host")
        await t.wait(lambda: t.host_online(), "host relay connected")
        assert t.http("/api/live/start-new", {"agent": "claude"})["delivered"]
        await t.wait(lambda: launches(), "agent started")
        first = launches()[0]
        ident = first["id"]
        await t.wait(lambda: any(s["launch"]["id"] == ident for s in t.status()), "session registered")
        assert any(s["session_id"] == ident for s in saved()), "session not checkpointed"

        if args.browser_resize_attached:
            size_file = folder / f"agent-size-{first['pid']}.json"
            def child_grid():
                return json.loads(size_file.read_text()) if size_file.exists() else None
            await t.wait(lambda: child_grid() == [120, 40], "child starts at local terminal size")
            session = next(s for s in t.status() if s["launch"]["id"] == ident)
            assert session["local_terminal"], "scenario needs an attached terminal"
            wrapper_pid = int((t.host_dir / "wrappers" / f"{ident}.lock").read_text())
            async def drift_child_then_reconcile(expected):
                requested = folder / f"resize-agent-{first['pid']}.json"
                temporary = requested.with_suffix(".tmp")
                temporary.write_text(json.dumps([91, 29]))
                temporary.replace(requested)
                await t.wait(lambda: child_grid() == [91, 29], "child changed its own PTY size")
                os.kill(wrapper_pid, signal.SIGWINCH)
                await t.wait(lambda: child_grid() == expected, "local control reconciles unchanged outer size")
            await drift_child_then_reconcile([120, 40])
            assert t.http(f"/api/live/{ident}/resize", {"cols": 83, "rows": 27})["delivered"]
            await t.wait(lambda: child_grid() == [83, 27], "browser size reaches attached child PTY")
            wrapper_pid = int((t.host_dir / "wrappers" / f"{ident}.lock").read_text())
            os.kill(wrapper_pid, signal.SIGWINCH)
            # A focus/redraw can signal WINCH without changing outer dimensions.
            # Give the real wrapper a scheduling window, then observe its child.
            await asyncio.sleep(0.4)
            assert child_grid() == [83, 27], "unchanged local dimensions cancelled the browser grid"
            requested = folder / f"resize-terminal-{session['surface']}.json"
            temporary = requested.with_suffix(".tmp")
            temporary.write_text(json.dumps([102, 34]))
            temporary.replace(requested)
            await t.wait(lambda: not requested.exists(), "local terminal window resized")
            wrapper_pid = int((t.host_dir / "wrappers" / f"{ident}.lock").read_text())
            os.kill(wrapper_pid, signal.SIGWINCH)
            await t.wait(lambda: child_grid() == [102, 34], "next local resize regains grid ownership")
            await drift_child_then_reconcile([102, 34])
            assert launches() == [first] and alive(first["pid"]), launches()
            print("PASS: browser resizes attached PTY, next local resize takes over; same agent and conversation")
            return

        if args.host_restart_without_binding or args.upgrade_from_prelock_wrapper:
            host.send_signal(signal.SIGTERM)
            host.wait(timeout=10)
            host = t.spawn(host_argv, "restarted-host")
            await t.wait(lambda: any(s["launch"]["id"] == ident for s in t.status()), "wrapper reconnected")
            await asyncio.sleep(3)
            running = [launch for launch in launches() if alive(launch["pid"])]
            assert running == [first], (
                f"host restart started {len(running) - 1} extra agents for a running conversation: {running}"
            )
            if args.upgrade_from_prelock_wrapper:
                assert not (t.host_dir / "wrappers").exists(), "the old wrapper held a liveness lock"
                print("PASS: a CLI upgrade reconnects a pre-lock wrapper without relaunching its conversation")
            else:
                print("PASS: host restart reconnects the running conversation without relaunching it")
            return

        def spawn_crashing_host(name):
            t.environment["CONVERSATIONS_HOST_CRASH_AT"] = "end-delivery"
            try:
                return t.spawn(host_argv, name)
            finally:
                del t.environment["CONVERSATIONS_HOST_CRASH_AT"]

        async def reopen_once():
            # Durable End reached the old agent; Re-open then starts exactly one.
            assert not [launch for launch in launches() if alive(launch["pid"])], launches()
            assert not any(s["session_id"] == ident for s in saved()), "ended checkpoint survived"
            # A host that dies after delivery but before the wrapper's exit
            # report leaves End journaled for that dead wrapper.
            journal = t.host_dir / "pending-end" / ident
            journal.write_text(f"{first['pid']}\n")
            assert t.http(f"/api/sessions/{ident}/reopen", {})["delivered"]
            await t.wait(lambda: len(launches()) >= 2, "re-opened agent started", timeout=20)
            await t.wait(lambda: any(s["launch"]["id"] == ident for s in t.status()), "re-opened agent registered")
            await asyncio.sleep(1)
            running = [launch for launch in launches() if alive(launch["pid"])]
            assert len(running) == 1 and "--resume" in running[0]["args"], running
            assert not journal.exists(), "the replacement left the old wrapper's End journaled"

        if args.crash_before_end_delivery:
            host.send_signal(signal.SIGTERM)
            host.wait(timeout=10)
            await t.wait(lambda: not t.host_online(), "old host relay released")
            host = spawn_crashing_host("crashing-host")
            await t.wait(lambda: any(s["launch"]["id"] == ident for s in t.status()), "wrapper reconnected")
            t.http(f"/api/live/{ident}/complete", {})
            assert host.wait(timeout=10) == 86, "host did not stop at the End delivery point"
            host = t.spawn(host_argv, "recovered-host")
            await t.wait(lambda: not alive(first["pid"]), "ended agent stopped after the host crash", timeout=30)
            await reopen_once()
            print("PASS: End accepted by a host that crashed before delivery still ends the old agent")
            return

        if args.crash_before_pending_end_delivery:
            (folder / "tree-delay").write_text("60")
            host.send_signal(signal.SIGTERM)
            host.wait(timeout=10)
            await t.wait(lambda: not t.host_online(), "old host relay released")
            host = t.spawn(host_argv, "restarted-host")
            await t.wait(lambda: t.host_online(), "restarted host relay connected")
            t.http(f"/api/live/{ident}/complete", {})
            await t.wait(lambda: not any(s["session_id"] == ident for s in saved()), "End accepted")
            host.send_signal(signal.SIGTERM)
            host.wait(timeout=10)
            (folder / "tree-delay").unlink()
            host = spawn_crashing_host("crashing-host")
            assert host.wait(timeout=30) == 86, "host did not stop at the End delivery point"
            host = t.spawn(host_argv, "recovered-host")
            await t.wait(lambda: not alive(first["pid"]), "ended agent stopped after the host crash", timeout=30)
            await reopen_once()
            print("PASS: a missed End survives a host crash while it is handed to the reconnecting wrapper")
            return

        if args.end_across_second_host_restart:
            # The wrapper cannot re-register while cmux inventory hangs.
            (folder / "tree-delay").write_text("60")
            host.send_signal(signal.SIGTERM)
            host.wait(timeout=10)
            await t.wait(lambda: not t.host_online(), "old host relay released")
            host = t.spawn(host_argv, "restarted-host")
            await t.wait(lambda: t.host_online(), "restarted host relay connected")
            t.http(f"/api/live/{ident}/complete", {})
            assert t.http(f"/api/sessions/{ident}/reopen", {})["delivered"]
            # This host has now handled End and given up on Reopen.
            await t.wait(lambda: "Local reopen still waiting" in (folder / "restarted-host.log").read_text(errors="replace"),
                         "reopen deferred for the live wrapper", timeout=20)
            host.send_signal(signal.SIGTERM)
            host.wait(timeout=10)
            await t.wait(lambda: not t.host_online(), "second host relay released")
            (folder / "tree-delay").unlink()
            host = t.spawn(host_argv, "third-host")
            await t.wait(lambda: not alive(first["pid"]), "ended agent stopped after the wrapper reconnected", timeout=30)
            assert not [launch for launch in launches() if alive(launch["pid"])], launches()
            # The conversation is re-openable, once.
            assert t.http(f"/api/sessions/{ident}/reopen", {})["delivered"]
            await t.wait(lambda: len(launches()) >= 2, "re-opened agent started", timeout=20)
            await t.wait(lambda: any(s["launch"]["id"] == ident for s in t.status()), "re-opened agent registered")
            running = [launch for launch in launches() if alive(launch["pid"])]
            assert len(running) == 1 and "--resume" in running[0]["args"], running
            print("PASS: an End missed across two host restarts still ends the old agent, and Re-open starts one")
            return

        # A host restart (for example after a CLI upgrade) leaves wrappers running.
        # cmux inventory is slow, so the old wrapper is still re-registering
        # when the browser issues Restart Session (complete, then reopen).
        (folder / "tree-delay").write_text("3")
        host.send_signal(signal.SIGTERM)
        host.wait(timeout=10)
        assert alive(first["pid"]), "host restart stopped the agent"
        await t.wait(lambda: not t.host_online(), "old host relay released")
        host = t.spawn(host_argv, "restarted-host")
        await t.wait(lambda: t.host_online(), "restarted host relay connected")
        t.http(f"/api/live/{ident}/complete", {})
        assert t.http(f"/api/sessions/{ident}/reopen", {})["delivered"]
        await t.wait(lambda: len(launches()) >= 2 or not alive(first["pid"]), "restart progressed", timeout=20)
        (folder / "tree-delay").unlink()
        await t.wait(lambda: len(launches()) >= 2, "replacement agent started", timeout=25)
        await asyncio.sleep(3)
        running = [launch for launch in launches() if launch["id"] == ident and alive(launch["pid"])]
        assert len(running) == 1, (
            f"Restart Session during host reconnect left {len(running)} agents running "
            f"for one conversation: {running}"
        )
        assert running[0]["pid"] != first["pid"] and "--resume" in running[0]["args"], running
        await t.wait(lambda: any(s["launch"]["id"] == ident for s in t.status()), "replacement registered")
        print("PASS: Restart Session during host reconnect replaces the agent exactly once")
    finally:
        for pid_file in (folder / "terminals").glob("*") if (folder / "terminals").exists() else []:
            try:
                os.kill(int(pid_file.read_text()), signal.SIGTERM)
            except (ProcessLookupError, ValueError):
                pass
        for launch in launches():
            try:
                os.kill(launch["pid"], signal.SIGTERM)
            except ProcessLookupError:
                pass
        for process in reversed(t.children):
            if process.poll() is None:
                process.terminate()
        for process in reversed(t.children):
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
        print("Evidence:", folder)


if __name__ == "__main__":
    folder = Path(os.environ.get("DURABILITY_DIR", "."))
    if len(sys.argv) > 1 and sys.argv[1] == "--claude":
        fake_claude(folder, sys.argv[2:])
    elif len(sys.argv) > 1 and sys.argv[1] == "--cmux":
        fake_cmux(folder, sys.argv[2:])
    elif len(sys.argv) > 1 and sys.argv[1] == "--terminal":
        fake_terminal(folder, sys.argv[2], sys.argv[3])
    else:
        asyncio.run(main())
