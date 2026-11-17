# /// script
# requires-python = ">=3.11"
# dependencies = ["websockets>=14,<16"]
# ///
"""Real localhost server + host + PTYs. Run with uv run scripts/test-local-host.py.

Build first: cargo build -p conversations-cli -p conversations-server
Override build directory with --target-dir. All processes and data are isolated;
this script never changes the user's saved Conversations credentials or cmux config.

Legacy browser-control probes in this script and its manual cmux callers require
migration to signed host controls. Retired unsigned actions fail explicitly;
the imported local helpers and CI flows remain available. Signed start/reopen/end
coverage lives in `cargo test -p conversations-cli --test host_lifecycle`.
"""
import argparse
import asyncio
import base64
import fcntl
import json
import os
from pathlib import Path
import secrets
import shlex
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time
import urllib.error
import urllib.request
import uuid

from websockets.asyncio.client import connect

ROOT = Path(__file__).resolve().parents[1]


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class Test:
    def __init__(self, target, folder):
        self.folder = folder
        self.cli = target / "debug/arrakis-conversations"
        self.server = target / "debug/conversations-server"
        self.url = f"http://127.0.0.1:{free_port()}"
        self.cookie = ""
        self.restart_command = []
        self.children = []
        self.fds = []
        self.logs = []
        self.host_dir = folder / "host"
        self.host_dir.mkdir(mode=0o700)
        self.host_id = None
        self.environment = {k: v for k, v in os.environ.items() if not k.startswith(("CONVERSATIONS_", "CMUX_"))}

    def spawn(self, argv, name, **kwargs):
        log = open(self.folder / f"{name}.log", "ab")
        self.logs.append(log)
        process = subprocess.Popen(argv, cwd=ROOT, env=self.environment, stderr=log,
                                   stdout=kwargs.pop("stdout", log), **kwargs)
        self.children.append(process)
        return process

    def http(self, path, data=None, method=None):
        route = path.split("?", 1)[0]
        parts = route.split("/")
        retired_action = route == "/api/live/start-new" or (
            len(parts) == 5 and parts[:2] == ["", "api"] and bool(parts[3])
            and ((parts[2] == "sessions" and parts[4] == "reopen")
                 or (parts[2] == "live" and parts[4] in ("resize", "repaint")))
        )
        if retired_action and (method == "POST" or (method is None and data is not None)):
            raise RuntimeError(
                f"Legacy unsigned browser-control probe {route} is retired. "
                "Migrate this probe to signed host controls; current signed "
                "start/reopen/end coverage: cargo test -p conversations-cli --test host_lifecycle"
            )
        request = urllib.request.Request(self.url + path,
            data=json.dumps(data).encode() if data is not None else None,
            headers={"Content-Type": "application/json", "Cookie": self.cookie}, method=method)
        with urllib.request.urlopen(request, timeout=3) as response:
            cookie = response.headers.get("Set-Cookie")
            if cookie:
                self.cookie = cookie.split(";", 1)[0]
            return json.load(response)

    async def wait(self, predicate, label, timeout=12):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            try:
                value = predicate()
                if value:
                    return value
            except (OSError, ValueError, urllib.error.HTTPError, subprocess.CalledProcessError):
                pass
            await asyncio.sleep(.1)
        raise AssertionError(f"timed out: {label}; logs in {self.folder}")

    def host(self):
        return self.spawn([str(self.cli), "--server", self.url, "host", "--runtime-dir",
            str(self.host_dir), "run", "--cookie-file", str(self.folder / "cookie")], "host")

    def host_online(self):
        if self.host_id is None:
            self.host_id = subprocess.check_output(
                [str(self.cli), "host", "--runtime-dir", str(self.host_dir),
                 "credential", "identity"],
                cwd=ROOT, env=self.environment, timeout=3, text=True,
            ).strip()
        return self.http(f"/api/host-relay/{self.host_id}/status")["online"]

    def status(self):
        result = subprocess.run([str(self.cli), "host", "--runtime-dir", str(self.host_dir),
            "status", "--json"], capture_output=True, timeout=2, check=True)
        return json.loads(result.stdout)["sessions"]

    def wrap(self, label):
        master, slave = os.openpty()
        self.fds.append(master)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        ident = str(uuid.uuid4())
        script = f"stty -echo; printf '{label}_READY\\n'; while IFS= read -r line; do printf '{label}:%s\\n' \"$line\"; done"
        process = self.spawn([str(self.cli), "host", "--runtime-dir", str(self.host_dir),
            "wrap", "--session-id", ident, "--", "/bin/sh", "-c", script], label,
            stdin=slave, stdout=slave, start_new_session=True)
        os.close(slave)
        os.set_blocking(master, False)
        return ident, process, master

    async def view(self, ident):
        await self.wait(lambda: ident in json.dumps(self.http("/api/live")), f"register {ident}")
        return await connect(self.url.replace("http:", "ws:") + f"/api/live/{ident}/pty/ws?replay=history",
            additional_headers={"Cookie": self.cookie})

    async def output(self, ws, expected, timeout=8):
        contents = bytearray()
        async with asyncio.timeout(timeout):
            while True:
                frame = json.loads(await ws.recv())
                if "b64" in frame:
                    contents.extend(base64.b64decode(frame["b64"]))
                    if expected.encode() in contents:
                        return bytes(contents), frame

    async def input(self, ws, text):
        await ws.send(json.dumps({"type": "input", "b64": base64.b64encode(text.encode()).decode()}))

    async def run(self):
        # Hosts accept saved HTTPS origins; reject insecure remote HTTP and paths.
        for server_args in [["--server", "http://production.invalid"], ["--server", "https://production.invalid/path"]]:
            result = subprocess.run([str(self.cli), *server_args, "host", "--runtime-dir",
                str(self.host_dir), "run"], capture_output=True, timeout=3)
            assert result.returncode != 0
        self.environment.update(BIND_ADDR=self.url.removeprefix("http://"),
            DATABASE_URL=f"sqlite://{self.folder}/test.db?mode=rwc", AUTH_ENABLED="true",
            PASSWORD_AUTH_ENABLED="true", COOKIE_SECURE="false", SHUTDOWN_DRAIN_SECS="0")
        self.spawn([str(self.server)], "server")
        await self.wait(lambda: self.http("/healthz"), "server readiness")
        self.http("/auth/register", {"email": "cmux-test@example.test", "password": secrets.token_urlsafe(24)})
        cookie_path = self.folder / "cookie"
        cookie_path.write_text(self.cookie)
        cookie_path.chmod(0o600)
        # A fake ADK executable observes the real launch environment and cwd.
        bindir = self.folder / "bin"
        bindir.mkdir()
        adk = bindir / "adk"
        adk.write_text("#!/bin/sh\nprintf 'ADK_STATUS=%s CWD=%s ARGS=%s\\n' \"$OC_STATUS\" \"$PWD\" \"$*\"\nread answer\n")
        adk.chmod(0o700)
        self.environment.update(ARRAKIS_HOME=str(self.folder), OC_STATUS="1",
            PATH=str(bindir) + os.pathsep + self.environment["PATH"])
        host = self.host()
        await self.wait(lambda: (self.host_dir / "host.sock").exists(), "host socket")
        # One daemon per private runtime; second host cannot unlink the first socket.
        duplicate = self.host()
        assert duplicate.wait(timeout=3) != 0
        assert (self.host_dir / "host.sock").exists()
        a, pa, ma = self.wrap("ALPHA")
        b, pb, mb = self.wrap("BETA")
        await self.wait(lambda: len(self.status()) == 2, "two wrapper registrations")
        wa, wb = await self.view(a), await self.view(b)
        await self.output(wa, "ALPHA_READY")
        await self.output(wb, "BETA_READY")
        await self.input(wa, "remote-alpha\n")
        await self.output(wa, "ALPHA:remote-alpha")
        await self.input(wb, "remote-beta\n")
        data, _ = await self.output(wb, "BETA:remote-beta")
        assert b"remote-alpha" not in data
        # Child has echo disabled and is still waiting for the newline. The
        # daemon must observe local input before any corresponding PTY output.
        os.write(ma, b"local-alpha")
        await self.wait(lambda: any(s["launch"]["id"] == a and s["local_input_bytes"] == 11
            for s in self.status()), "silent local input independently reaches daemon")
        assert next(s for s in self.status() if s["launch"]["id"] == b)["local_input_bytes"] == 0
        os.write(ma, b"\n")
        await self.output(wa, "ALPHA:local-alpha")
        print("PASS: two real PTYs, local output and isolated remote input")

        # Browser dimensions cannot take over a locally attached terminal.
        fcntl.ioctl(ma, termios.TIOCSWINSZ, struct.pack("HHHH", 31, 101, 0, 0))
        os.kill(pa.pid, signal.SIGWINCH)
        _, frame = await self.output(wa, "ALPHA:local-alpha")
        assert (frame["cols"], frame["rows"]) == (101, 31), frame
        print("PASS: local terminal resize reaches browser mirror")

        self.http(f"/api/live/{a}/title", {"title": "Keep renamed conversation"})
        host.terminate()
        host.wait(timeout=5)
        os.write(ma, b"offline-local\n")
        local = bytearray()
        def offline_output():
            try:
                local.extend(os.read(ma, 65536))
            except BlockingIOError:
                pass
            return b"ALPHA:offline-local" in local
        await self.wait(offline_output, "local I/O while host stopped")
        assert pa.poll() is None and pb.poll() is None
        # Complete each line before sending the next to force separate stdin
        # reads, exceeding the 128-chunk observation buffer deterministically.
        for index in range(140):
            marker = f"offline-overflow-{index:03d}"
            os.write(ma, (marker + "\n").encode())
            def received_line():
                try:
                    local.extend(os.read(ma, 65536))
                except BlockingIOError:
                    pass
                return ("ALPHA:" + marker).encode() in local
            await self.wait(received_line, "shell remains responsive through input buffer overflow")
        host = self.host()
        await self.wait(lambda: len(self.status()) == 2, "wrapper reconnection")
        await self.wait(lambda: any(s["launch"]["id"] == a
            and s["local_input_bytes"] > 0 and s["local_input_dropped_chunks"] > 0
            for s in self.status()), "queued local input and explicit overflow reach restarted daemon")
        assert next(s for s in self.status() if s["launch"]["id"] == b)["local_input_bytes"] == 0
        await wa.close()
        wa = await self.view(a)
        await self.output(wa, "ALPHA:offline-local")
        assert any(item["id"] == a and item["title"] == "Keep renamed conversation"
            for item in self.http("/api/live")["live_sessions"])
        await self.input(wa, "after-restart\n")
        await self.output(wa, "ALPHA:after-restart")
        print("PASS: daemon restart preserves shells, restores terminal state, drains input observations, and reports overflow")

        # Start from the real web control endpoint, not a local launch shortcut.
        await self.wait(self.host_online, "web host presence")
        response = self.http("/api/live/start-new", {"agent": "zsh"})
        assert response["delivered"], response
        sessions = await self.wait(lambda: (s if len(s := self.status()) == 3 else None), "web-created wrapper")
        new_id = next(s["launch"]["id"] for s in sessions if s["launch"]["id"] not in (a, b))
        wc = await self.view(new_id)
        await self.input(wc, "printf 'WEB_%s\\n' STARTED\n")
        await self.output(wc, "WEB_STARTED")
        self.http(f"/api/live/{new_id}/complete", {})
        await self.wait(lambda: len(self.status()) == 2, "web end removes wrapper")
        print("PASS: browser start/end creates and stops a normal-backend session")
        assert self.http("/api/live/start-new", {"agent": "opencode", "launcher": "adk"})["delivered"]
        adk_session = await self.wait(lambda: next((s for s in self.status()
            if s["launch"]["agent"] == "opencode"), None), "ADK wrapper")
        adk_id = adk_session["launch"]["id"]
        adk_ws = await self.view(adk_id)
        await self.output(adk_ws, f"ADK_STATUS=0 CWD={self.folder} ARGS=code")
        self.http(f"/api/live/{adk_id}/complete", {})
        await adk_ws.close()
        await self.wait(lambda: len(self.status()) == 2, "ADK wrapper ends")
        print("PASS: ADK launch uses ARRAKIS_HOME and overrides ambient statusline settings")
        # A short-lived command must acknowledge registration even if it exits
        # between two host event-loop turns.
        short = subprocess.run([str(self.cli), "host", "--runtime-dir", str(self.host_dir),
            "start", "--", "/bin/sh", "-c", "printf 'short-command\\n'"],
            capture_output=True, timeout=15)
        assert short.returncode == 0, short.stderr.decode()
        await wc.close()
        await wa.close()
        await wb.close()
        for ident in (a, b):
            self.http(f"/api/live/{ident}/complete", {})
        await self.wait(lambda: pa.poll() is not None and pb.poll() is not None, "wrapper cleanup")

    async def cmux_workspace(self, ws, workspace, expected_title):
        snapshot = self.folder / f"workspace-{uuid.uuid4()}.json"
        cmux = '/Applications/cmux.app/Contents/Resources/bin/cmux'
        async with asyncio.timeout(12):
            while True:
                temporary = str(snapshot) + ".part"
                await self.input(ws, f"{shlex.quote(cmux)} --json workspace list > {shlex.quote(temporary)} && mv {shlex.quote(temporary)} {shlex.quote(str(snapshot))}\n")
                await self.wait(snapshot.exists, "cmux workspace snapshot")
                data = json.loads(snapshot.read_text())
                snapshot.unlink()
                item = next(item for item in data["workspaces"] if item["id"] == workspace)
                if item["title"] == expected_title:
                    return
                await asyncio.sleep(.2)

    async def cmux_capture_disabled(self, ws):
        cmux = "/Applications/cmux.app/Contents/Resources/bin/cmux"
        result = self.folder / "disabled-workspace.json"
        marker = self.folder / "disabled-shell-ready"
        params = json.dumps({"title": "Conversations capture disabled test", "focus": True,
            "workspace_env": {"GHOSTTY_ZSH_ZDOTDIR": str(self.host_dir / "zsh-bootstrap"),
                "CONVERSATIONS_CAPTURE": "0"}})
        await self.input(ws, shlex.join([cmux, "rpc", "workspace.create", params])
            + " > " + shlex.quote(str(result)) + "\n")
        await self.wait(lambda: result.exists() and result.stat().st_size, "disabled workspace created")
        created = json.loads(result.read_text())
        workspace = created["workspace_id"]
        try:
            command = "printf ready > " + shlex.quote(str(marker)) + "\n"
            await self.input(ws, shlex.join([cmux, "send", "--workspace", workspace,
                "--surface", created["surface_id"], "--", command]) + "\n")
            await self.wait(marker.exists, "disabled workspace shell is interactive")
            # Check after the shell has processed input and the reconnect window
            # has elapsed; absence alone before shell startup would prove nothing.
            await asyncio.sleep(1)
            assert not any(s["workspace"] == workspace for s in self.status()), "capture=0 must leave terminal unwrapped"
        finally:
            await self.input(ws, shlex.join([cmux, "workspace", "close", workspace, "--force"]) + "\n")
        print("PASS: workspace capture flag disables wrapping without disabling its shell")

    async def cmux_startup_parameters(self):
        """Observe argv/cwd in the actual process behind cmux and the wrapper."""
        cwd = self.folder / "directory with spaces and ' quotes"
        cwd.mkdir()
        recorder = self.folder / "record startup.py"
        result = self.folder / "startup.json"
        recorder.write_text("import json, os, sys, time\n"
            "from pathlib import Path\n"
            "target = Path(sys.argv[1])\n"
            "temporary = target.with_suffix('.part')\n"
            "temporary.write_text(json.dumps({'cwd': os.getcwd(), 'argv': sys.argv[2:]}))\n"
            "temporary.replace(target)\n"
            "time.sleep(60)\n")
        words = ["two words", "quote'\"", "$HOME; $(false) `false`", "", "--flag=value", "line\nbreak"]
        launched = subprocess.run([str(self.cli), "host", "--runtime-dir", str(self.host_dir),
            "start", "--cwd", str(cwd), "--", sys.executable, str(recorder), str(result), *words],
            capture_output=True, text=True, timeout=15, check=True)
        ident = str(uuid.UUID(launched.stdout.strip().split()[-1]))
        try:
            await self.wait(result.exists, "native child records startup parameters")
            observed = json.loads(result.read_text())
            assert observed["argv"] == words, observed
            assert Path(observed["cwd"]).resolve() == cwd.resolve(), observed
            session = next(s for s in self.status() if s["launch"]["id"] == ident)
            assert session["workspace"] and session["surface"] and session["local_terminal"]
            print("PASS: native cmux child receives exact startup arguments and working directory")
        finally:
            self.http(f"/api/live/{ident}/complete", {})
            await self.wait(lambda: all(s["launch"]["id"] != ident for s in self.status()),
                "startup parameter probe ends")

    async def cmux(self):
        """Test only sessions this invocation creates in an already-running local host."""
        before = self.status()
        existing = {s["launch"]["id"] for s in before}
        workspaces = {s["workspace"] for s in before}
        await self.wait(self.host_online, "cmux host presence")
        created = []
        try:
            for index in range(2):
                assert self.http("/api/live/start-new", {"agent": "zsh"})["delivered"]
                session = await self.wait(lambda: next((s for s in self.status()
                    if s["launch"]["id"] not in existing), None), "cmux-created wrapper")
                ident = session["launch"]["id"]
                created.append(ident)
                existing.add(ident)
                assert session["local_terminal"] and session["surface"] and session["workspace"]
                assert session["workspace"] not in workspaces, "Each conversation must have its own sidebar workspace"
                workspaces.add(session["workspace"])
                ws = await self.view(ident)
                try:
                    await self.input(ws, f"printf 'CMUX_%s\\n' WEB_PROOF_{index}\n")
                    await self.output(ws, f"CMUX_WEB_PROOF_{index}")
                    title = f"Conversation title {index} — quotes ' and $ are literal"
                    self.http(f"/api/sessions/{ident}/title", {"title": title}, method="PUT")
                    await self.cmux_workspace(ws, session["workspace"], title)
                    reverse_title = f"Named from cmux {index} ' $ literal"
                    cmux = "/Applications/cmux.app/Contents/Resources/bin/cmux"
                    await self.input(ws, shlex.join([cmux, "workspace", "rename", session["workspace"], "--title", reverse_title]) + "\n")
                    await self.wait(lambda: any(item["id"] == ident and item["title"] == reverse_title
                        for item in self.http("/api/live")["live_sessions"]), "cmux rename reaches Conversations")
                    await self.cmux_workspace(ws, session["workspace"], reverse_title)
                    final_title = "-option-like conversation name" if index == 0 else f"Conversations wins the next edit {index}"
                    self.http(f"/api/sessions/{ident}/title", {"title": final_title}, method="PUT")
                    await self.cmux_workspace(ws, session["workspace"], final_title)
                    if index == 0:
                        await self.cmux_capture_disabled(ws)
                        # No initial_command: this is the same default-shell path
                        # used by cmux's new-terminal UI action.
                        params = json.dumps({"workspace_id": session["workspace"], "type": "terminal", "focus": True})
                        await self.input(ws, shlex.join([cmux, "rpc", "surface.create", params]) + "\n")
                        extra = await self.wait(lambda: next((s for s in self.status()
                            if s["launch"]["id"] not in existing), None), "default cmux terminal automatically wraps")
                        extra_id = extra["launch"]["id"]
                        created.append(extra_id)
                        existing.add(extra_id)
                        assert extra["workspace"] != session["workspace"], f"Each extra conversation needs a separate sidebar workspace: original={session!r}, extra={extra!r}"
                        extra_ws = await self.view(extra_id)
                        tree_file = self.folder / "moved-workspace-tree.json"
                        await self.input(extra_ws, shlex.join([cmux, "--json", "tree", "--workspace", extra["workspace"]])
                            + " > " + shlex.quote(str(tree_file)) + "\n")
                        await self.wait(lambda: tree_file.exists() and tree_file.stat().st_size, "moved workspace tree")
                        tree = json.loads(tree_file.read_text())
                        moved_workspace = next(w for window in tree["windows"] for w in window["workspaces"]
                            if w["id"] == extra["workspace"])
                        assert sum(p["surface_count"] for p in moved_workspace["panes"]) == 1, "temporary placeholder must be removed before registration"
                        try:
                            await self.input(extra_ws, "printf 'AUTO_%s\\n' WRAPPED\n")
                            await self.output(extra_ws, "AUTO_WRAPPED")
                            extra_title = "Extra conversation has its own sidebar name"
                            self.http(f"/api/sessions/{extra_id}/title", {"title": extra_title}, method="PUT")
                            await self.cmux_workspace(extra_ws, extra["workspace"], extra_title)
                            await self.cmux_workspace(ws, session["workspace"], final_title)
                            # cmux keystrokes enter through the wrapper's local
                            # stdin, unlike browser input received over the socket.
                            command = "printf 'NATIVE_%s\\n' LOCAL_INPUT\n"
                            await self.input(ws, shlex.join([cmux, "send", "--workspace",
                                extra["workspace"], "--surface", extra["surface"], "--", command]) + "\n")
                            await self.output(extra_ws, "NATIVE_LOCAL_INPUT")
                            await self.wait(lambda: any(s["launch"]["id"] == extra_id
                                and s["local_input_bytes"] > 0 for s in self.status()),
                                "native cmux keystrokes independently reach daemon")
                            if self.restart_command:
                                subprocess.run(self.restart_command, cwd=ROOT, check=True, timeout=30)
                                expected = {ident: session["workspace"], extra_id: extra["workspace"]}
                                await self.wait(lambda: expected.items() <= {
                                    s["launch"]["id"]: s["workspace"] for s in self.status()}.items(),
                                    "moved terminals reconnect to the same sidebar workspaces", timeout=20)
                                await ws.close()
                                await extra_ws.close()
                                ws = await self.view(ident)
                                extra_ws = await self.view(extra_id)
                                await self.input(extra_ws, "printf 'RECONNECTED_%s\\n' MOVED_SHELL\n")
                                await self.output(extra_ws, "RECONNECTED_MOVED_SHELL")
                                await self.cmux_workspace(extra_ws, extra["workspace"], extra_title)
                                await self.cmux_workspace(ws, session["workspace"], final_title)
                                print("PASS: moved shell reconnects with unchanged workspace, title, and terminal I/O")
                            nested_params = json.dumps({"workspace_id": extra["workspace"],
                                "type": "terminal", "focus": True})
                            await self.input(extra_ws, shlex.join([cmux, "rpc", "surface.create", nested_params]) + "\n")
                            nested = await self.wait(lambda: next((s for s in self.status()
                                if s["launch"]["id"] not in existing), None),
                                "new terminal in moved workspace automatically wraps")
                            nested_id = nested["launch"]["id"]
                            created.append(nested_id)
                            existing.add(nested_id)
                            assert nested["workspace"] not in {extra["workspace"], session["workspace"]}
                            nested_ws = await self.view(nested_id)
                            try:
                                await self.input(nested_ws, "printf 'NESTED_%s\\n' WRAPPED\n")
                                await self.output(nested_ws, "NESTED_WRAPPED")
                            finally:
                                self.http(f"/api/live/{nested_id}/complete", {})
                                await nested_ws.close()
                        finally:
                            self.http(f"/api/live/{extra_id}/complete", {})
                            await extra_ws.close()
                        await self.wait(lambda: all(s["launch"]["id"] != extra_id for s in self.status()), "extra terminal ends")
                finally:
                    await ws.close()
            for ident in created:
                self.http(f"/api/live/{ident}/complete", {})
            await self.wait(lambda: all(s["launch"]["id"] not in created for s in self.status()), "cmux wrappers end")
            print("PASS: two web conversations create distinct sidebar workspaces, default terminals auto-wrap, browser I/O and bidirectional titles work, and web end stops wrappers")
        finally:
            for ident in created:
                try:
                    self.http(f"/api/live/{ident}/complete", {})
                except urllib.error.HTTPError:
                    pass

    def cleanup(self):
        for process in reversed(self.children):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        for fd in self.fds:
            os.close(fd)
        for log in self.logs:
            log.close()


async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target")
    parser.add_argument("--cmux-server", help="Test an already-running localhost cmux host")
    parser.add_argument("--runtime-dir", type=Path)
    parser.add_argument("--cookie-file", type=Path)
    parser.add_argument("--cmux-restart-command", nargs="+", help="Optional argv to restart the isolated test host; never use a shared host")
    args = parser.parse_args()
    (ROOT / "tmp").mkdir(exist_ok=True)
    folder = Path(tempfile.mkdtemp(prefix="host-test-", dir=ROOT / "tmp"))
    test = Test(args.target_dir.resolve(), folder)
    try:
        if args.cmux_server:
            if not args.cmux_server.startswith("http://127.0.0.1:") or not args.runtime_dir or not args.cookie_file:
                parser.error("cmux checks require a 127.0.0.1 URL, --runtime-dir, and --cookie-file")
            test.restart_command = args.cmux_restart_command or []
            test.url = args.cmux_server
            test.host_dir = args.runtime_dir.resolve()
            test.cookie = args.cookie_file.read_text().strip()
            await test.cmux_startup_parameters()
            await test.cmux()
        else:
            await test.run()
        print(f"Local host lifecycle checks passed. Logs: {folder}")
    finally:
        test.cleanup()


if __name__ == "__main__":
    asyncio.run(main())
