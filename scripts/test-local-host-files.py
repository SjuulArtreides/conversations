# /// script
# requires-python = ">=3.11"
# dependencies = ["websockets>=14,<16", "cryptography>=44,<47"]
# ///
"""Isolated signed-transfer regression; no installed terminals or credentials.

Managed normal/cmux hosts authorize each batch through the real signed relay.
The legacy interactive path has no local signing authority and must reject
unsigned transfers while keeping local terminal input usable.
"""
import argparse
import asyncio
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import signal
import subprocess
import tempfile
import time
import uuid
import urllib.error

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey, Ed25519PublicKey
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

os.sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("local_host", Path(__file__).with_name("test-local-host.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def isolated_test(target, folder):
    test = module.Test(target, folder)
    test.environment = {key: value for key, value in os.environ.items()
        if key in {"PATH", "TMPDIR", "LANG", "LC_ALL", "TERM", "SSL_CERT_FILE", "NIX_SSL_CERT_FILE"}}
    return test


def status(test):
    with module.socket.socket(module.socket.AF_UNIX) as socket:
        socket.settimeout(1)
        socket.connect(str(test.host_dir / "host.sock"))
        message = b'{"type":"status"}'
        socket.sendall(module.struct.pack(">I", len(message)) + message)
        def read_exact(length):
            data = bytearray()
            while len(data) < length:
                chunk = socket.recv(length - len(data))
                if not chunk:
                    raise OSError("host closed status connection")
                data.extend(chunk)
            return data
        length = module.struct.unpack(">I", read_exact(4))[0]
        assert length < 1024 * 1024
        return json.loads(read_exact(length))["sessions"]


def b64url(data):
    return base64.urlsafe_b64encode(data).decode().rstrip("=")


def unb64url(text):
    return base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))


class HostAuthority:
    """Task-owned key, locally approved for only this smoke's sessions."""

    def __init__(self, test, sessions):
        self.test = test
        self.key = Ed25519PrivateKey.generate()
        public = self.key.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
        self.kid = b64url(hashlib.sha256(public).digest())
        command = [str(test.cli), "host", "--runtime-dir", str(test.host_dir), "credential"]
        descriptor = subprocess.check_output(command + ["descriptor"],
            cwd=module.ROOT, env=test.environment, timeout=5, text=True).strip()
        assert descriptor.startswith("chd1."), descriptor
        host_public = unb64url(descriptor.removeprefix("chd1."))
        self.host_id = b64url(hashlib.sha256(host_public).digest())
        self.host_key = Ed25519PublicKey.from_public_bytes(host_public)
        grants = [item for ident in sessions for item in ("--grant", f"session.write:{ident}")]
        subprocess.run(command + ["add", "--kind", "owner", "--public-key",
            base64.b64encode(public).decode(), "--label", "file smoke", *grants],
            cwd=module.ROOT, env=test.environment, timeout=5, check=True, capture_output=True)

    def operation(self, ident, op, args, *, valid_signature=True):
        now = int(time.time())
        payload = json.dumps({"v": 1, "typ": "op", "aud": self.host_id,
            "kid": self.kid, "dlg": None, "op": op, "sess": ident,
            "jti": secrets.token_hex(16), "iat": now, "exp": now + 60,
            "args": {"session": ident, **args}}, separators=(",", ":")).encode()
        signature = self.key.sign(b"conversations-op/v1\n" + payload)
        if not valid_signature:
            signature = bytes([signature[0] ^ 1]) + signature[1:]
        answer = self.test.http(f"/api/host-relay/{self.host_id}/request", {
            "type": "signed_op", "p": b64url(payload), "s": b64url(signature), "k": self.kid})
        assert answer["type"] == "op", answer
        signed = answer["statement"]
        reply = unb64url(signed["p"])
        self.host_key.verify(unb64url(signed["s"]), b"conversations-host/v1\n" + reply)
        statement = json.loads(reply)
        assert (statement["v"], statement["typ"], statement["host"], statement["kid"],
                statement["payload_sha256"], statement["status"]) == (
            1, "op_result", self.host_id, self.kid, b64url(hashlib.sha256(payload).digest()),
            "executed" if valid_signature else "refused"), statement

    def files(self, ident, name, content):
        transfer = secrets.token_hex(16)
        self.operation(ident, "session.send_files", {"transfer": transfer, "files": [
            {"name": name, "size": len(content), "sha256": hashlib.sha256(content).hexdigest()}]})
        return {"host_transfer": transfer,
                "files": [{"name": name, "b64": base64.b64encode(content).decode()}]}


def files_ready(test, ident):
    # Admission checks receiver presence before validating this empty batch.
    # This probe cannot stage or paste anything; valid uploads are never retried.
    try:
        test.http(f"/api/live/{ident}/files", {"files": []})
        raise AssertionError("empty file batch was accepted")
    except urllib.error.HTTPError as error:
        code = json.loads(error.read())["code"]
        if error.code == 400 and code == "invalid_files":
            return True
        if error.code == 503 and code == "host_unavailable":
            return False
        raise


async def run(target, backend):
    with tempfile.TemporaryDirectory(prefix="conversations-files-") as temporary:
        folder = Path(temporary)
        test = isolated_test(target, folder)
        home = folder / "home"
        home.mkdir()
        test.environment.update(HOME=str(home), XDG_CONFIG_HOME=str(home),
            CONVERSATIONS_SERVER=test.url, BIND_ADDR=test.url.removeprefix("http://"),
            DATABASE_URL=f"sqlite://{folder}/test.db?mode=rwc", AUTH_ENABLED="true",
            PASSWORD_AUTH_ENABLED="true", COOKIE_SECURE="false", SHUTDOWN_DRAIN_SECS="0")
        headless_done = False
        try:
            server = test.spawn([str(test.server)], "server")
            await test.wait(lambda: test.http("/healthz"), "server")
            password = secrets.token_urlsafe(24)
            test.http("/auth/register", {"email": "files@example.test", "password": password})
            cookie = folder / "cookie"
            cookie.write_text(test.cookie)
            cookie.chmod(0o600)
            host_args = [str(test.cli), "--server", test.url, "host", "--runtime-dir",
                str(test.host_dir), "run", "--cookie-file", str(cookie), "--backend", backend]
            if backend == "cmux":
                # Controlled cmux RPC fixture only. Never connect to the user's cmux socket.
                cmux = folder / "cmux"
                cmux.write_text('#!' + os.sys.executable + '\nimport json,sys\n'
                    'workspaces=[{"id":f"workspace-{i}","panes":[{"surfaces":[{"id":f"surface-{i}","type":"terminal"}]}]} for i in range(2)]\n'
                    'print(json.dumps({"windows":[{"workspaces":workspaces}]} if "tree" in sys.argv else {"ok":True,"result":{"workspaces":[],"terminals":[]}}))\n')
                cmux.chmod(0o700)
                test.environment["CMUX_WORKSPACE_ID"] = "fixture-host"
                test.environment["CMUX_BUNDLED_CLI_PATH"] = str(cmux)
                host_args += ["--cmux-bin", str(cmux)]
            host = test.spawn(host_args, "host")
            await test.wait(lambda: (test.host_dir / "host.sock").exists(), "host socket")
            ids, captures, wrappers = [], [], []
            count = 3 if backend == "normal" else 2
            for index in range(count):
                ident = str(uuid.uuid4())
                capture = folder / f"input-{index}"
                ready = folder / f"ready-{index}"
                # Child reads raw PTY input, including bracketed-paste markers and any Enter.
                child = ("import os,tty; from pathlib import Path; tty.setraw(0); "
                    "import signal; "
                    f"Path({str(folder / f'wrapper-pid-{index}')!r}).write_text(os.environ['CONVERSATIONS_WRAPPER_PID']); "
                    f"signal.signal(signal.SIGUSR1, lambda *_: Path({str(folder / f'read-result-{index}')!r}).write_bytes(Path(Path({str(folder / f'read-request-{index}')!r}).read_text()).read_bytes())); "
                    f"Path({str(ready)!r}).write_text(str(os.getpid())); "
                    f"f=open({str(capture)!r},'ab',buffering=0); "
                    "exec('while True:\\n f.write(os.read(0,65536))')")
                if backend == "cmux":
                    test.environment.update(CMUX_SURFACE_ID=f"surface-{index}",
                        CMUX_WORKSPACE_ID=f"workspace-{index}")
                if index == 2:
                    started = await asyncio.to_thread(subprocess.run,
                        [str(test.cli), "--server", test.url, "host", "--runtime-dir", str(test.host_dir),
                         "start", "--cwd", str(folder), "--", os.sys.executable, "-c", child],
                        env=test.environment, cwd=folder, capture_output=True, timeout=20, check=True)
                    ident = started.stdout.decode().split()[-1]
                    uuid.UUID(ident)
                else:
                    master, slave = os.openpty()
                    test.fds.append(master)
                    module.fcntl.ioctl(slave, module.termios.TIOCSWINSZ, module.struct.pack("HHHH", 24, 80, 0, 0))
                    # The daemon, not the shell's saved/default server, owns routing and auth.
                    wrapper = test.spawn([str(test.cli), "--server", "http://127.0.0.1:1", "host", "--runtime-dir",
                        str(test.host_dir), "wrap", "--session-id", ident, "--",
                        os.sys.executable, "-c", child], f"wrapper-{index}",
                        stdin=slave, stdout=slave, start_new_session=True)
                    wrappers.append(wrapper)
                    os.close(slave)
                ids.append(ident)
                captures.append(capture)
                await test.wait(ready.exists, "raw child ready")
            test.environment.pop("CMUX_SURFACE_ID", None)
            if backend == "cmux":
                test.environment["CMUX_WORKSPACE_ID"] = "fixture-host"
            await test.wait(lambda: len(status(test)) == count, "wrapper registration")
            assert sum(s["local_terminal"] for s in status(test)) == 2
            if backend == "cmux":
                assert {s["surface"] for s in status(test)} == {"surface-0", "surface-1"}
            await test.wait(lambda: len(test.http("/api/live")["live_sessions"]) == count, "server registration")
            for ident in ids:
                await test.wait(lambda: files_ready(test, ident), "file receiver admission")
            authority = HostAuthority(test, ids)
            await test.wait(test.host_online, "signed host relay")
            await asyncio.to_thread(authority.operation, ids[0], "session.send_files", {
                "transfer": secrets.token_hex(16), "files": [
                    {"name": "forged.bin", "size": 1, "sha256": hashlib.sha256(b"x").hexdigest()}]},
                valid_signature=False)
            assert not captures[0].exists() or captures[0].read_bytes() == b"", "forged authorization typed input"
            roots = []
            stage_roots = {}
            async def drop(index, name, content):
                await test.wait(test.host_online, "signed host relay ready for transfer")
                previous = captures[index].read_bytes() if captures[index].exists() else b""
                expected_pastes = previous.count(b"\x1b[201~") + 1
                try:
                    batch = await asyncio.to_thread(authority.files, ids[index], name, content)
                    response = await asyncio.to_thread(test.http, f"/api/live/{ids[index]}/files", batch)
                except urllib.error.HTTPError as error:
                    raise AssertionError(f"file delivery returned {error.code}: {error.read().decode()}") from error
                assert response["status"] == "sent", response
                await test.wait(lambda: captures[index].exists()
                    and captures[index].read_bytes().count(b"\x1b[201~") >= expected_pastes, "actual PTY paste")
                data = captures[index].read_bytes()
                assert b"\r" not in data and b"\n" not in data, data
                paths = re.findall(rb"\x1b\[200~(.*?)\x1b\[201~", data)
                assert len(paths) == expected_pastes and data.startswith(previous), "duplicate or reordered paste"
                path = Path(shlex.split(paths[-1].decode())[0])
                assert stage_roots.setdefault(index, path.parents[2]) == path.parents[2], "receiver replacement reset terminal staging"
                assert path.read_bytes() == content
                assert path.stat().st_mode & 0o077 == 0
                assert path.parent.stat().st_mode & 0o077 == 0
                assert path.parents[2].stat().st_mode & 0o077 == 0
                roots.append(path)
                return data
            first = await drop(0, "a space ' ü.bin", bytes(range(256)))
            assert not captures[1].exists() or not captures[1].read_bytes()
            await drop(1, "second.bin", b"second terminal\x00")
            if backend == "normal":
                await drop(2, "headless.bin", b"actual host start -> headless wrapper\x00")
                assert not next(s for s in status(test) if s["launch"]["id"] == ids[2])["local_terminal"]
            assert captures[0].read_bytes() == first
            host.terminate()
            host.wait(timeout=6)
            old_cookie, test.cookie = test.cookie, ""
            test.http("/auth/login", {"email": "files@example.test", "password": password})
            assert test.cookie != old_cookie, "auth replacement fixture reused the original cookie"
            cookie.write_text(test.cookie)
            class NoRedirect(module.urllib.request.HTTPRedirectHandler):
                def redirect_request(self, *_):
                    return None
            logout = module.urllib.request.Request(test.url + "/auth/logout", method="POST",
                headers={"Cookie": old_cookie})
            try:
                module.urllib.request.build_opener(NoRedirect).open(logout, timeout=3)
                raise AssertionError("logout did not return its expected redirect")
            except urllib.error.HTTPError as error:
                assert error.code in (302, 303)
            # Close the old streams as well as invalidating their cookie. An
            # admitted probe after restart can now only see the new receiver.
            server.terminate()
            server.wait(timeout=8)
            server = test.spawn([str(test.server)], "server-auth-restarted")
            await test.wait(lambda: test.http("/healthz"), "server after auth rotation")
            test.spawn(host_args, "host-restarted")
            await test.wait(lambda: len(status(test)) == count, "daemon reconnection", timeout=30)
            for ident in ids:
                await test.wait(lambda: files_ready(test, ident), "receiver using changed credentials")
            assert captures[0].read_bytes() == first, "transfer replayed after reconnect"
            await drop(0, "after.bin", b"after reconnect")
            before = captures[0].read_bytes()
            stage = roots[0].parents[2]
            stage.chmod(0o500)
            try:
                try:
                    batch = await asyncio.to_thread(authority.files, ids[0], "unwritable.bin", b"x")
                    await asyncio.to_thread(test.http, f"/api/live/{ids[0]}/files", batch)
                    raise AssertionError("staging failure was not reported")
                except urllib.error.HTTPError as error:
                    assert json.loads(error.read())["code"] == "host_failed"
                assert captures[0].read_bytes() == before, "failed drop pasted input"
                assert "live: file drop" not in (folder / "wrapper-0.log").read_text(), "receiver diagnostics corrupted the agent terminal"
            finally:
                stage.chmod(0o700)
            server.terminate()
            server.wait(timeout=8)
            test.spawn([str(test.server)], "server-restarted")
            await test.wait(lambda: test.http("/healthz"), "server restart")
            for ident in ids:
                await test.wait(lambda: files_ready(test, ident), "file stream reconnected")
            assert captures[0].read_bytes() == before, "transfer replayed after SSE reconnect"
            await drop(0, "after-sse.bin", b"after SSE reconnect")
            reader, writer = await asyncio.open_unix_connection(test.host_dir / "host.sock")
            message = json.dumps({"type": "release", "id": ids[0]}).encode()
            writer.write(module.struct.pack(">I", len(message)) + message)
            await writer.drain()
            length = module.struct.unpack(">I", await reader.readexactly(4))[0]
            assert json.loads(await reader.readexactly(length))["type"] == "released"
            writer.close()
            await writer.wait_closed()
            assert wrappers[0].poll() is None
            assert roots[0].exists(), "detach invalidated the still-running terminal's attachment"
            (folder / "read-request-0").write_text(str(roots[0]))
            os.kill(int((folder / "ready-0").read_text()), signal.SIGUSR1)
            await test.wait((folder / "read-result-0").exists, "detached child reads its pasted attachment")
            assert (folder / "read-result-0").read_bytes() == bytes(range(256))
            detached_input = captures[0].read_bytes()
            try:
                await asyncio.to_thread(test.http, f"/api/live/{ids[0]}/files",
                    {"files": [{"name": "detached.bin", "b64": "eA=="}]})
                raise AssertionError("detached terminal accepted a new file")
            except urllib.error.HTTPError as error:
                assert json.loads(error.read())["code"] == "session_ended"
            assert captures[0].read_bytes() == detached_input
            assert roots[1].exists(), "detaching one terminal removed another terminal's files"
            if backend == "normal":
                await asyncio.to_thread(authority.operation, ids[2], "session.end", {})
                test.http(f"/api/live/{ids[2]}/complete", {})
            for wrapper in wrappers:
                wrapper.terminate()
                wrapper.wait(timeout=8)
            await test.wait(lambda: all(not path.parents[2].exists() for path in roots), "stage cleanup")
            headless_done = True
            if backend == "normal":
                print("PASS normal host start -> headless wrapper: actual staged bytes, raw PTY paste, acknowledgement, no Enter, exit cleanup")
                # Simulate the old wrapper wire format, which has no receiver capability.
                old_id = str(uuid.uuid4())
                reader, writer = await asyncio.open_unix_connection(test.host_dir / "host.sock")
                message = json.dumps({"type": "register", "registration": {
                    "launch": {"id": old_id, "agent": "zsh", "cwd": str(folder), "command": ["old-wrapper-fixture"]},
                    "workspace": None, "surface": None, "local_terminal": False, "reconnecting": False}}).encode()
                writer.write(module.struct.pack(">I", len(message)) + message)
                await writer.drain()
                try:
                    await test.wait(lambda: old_id in json.dumps(test.http("/api/live")), "old-wrapper registration")
                    try:
                        await asyncio.wait_for(reader.read(4), .4)
                        raise AssertionError("daemon sent an unsupported message to an old wrapper")
                    except TimeoutError:
                        pass
                    try:
                        await asyncio.to_thread(test.http, f"/api/live/{old_id}/files",
                            {"files": [{"name": "old.bin", "b64": "eA=="}]})
                        raise AssertionError("old wrapper falsely advertised a file receiver")
                    except urllib.error.HTTPError as error:
                        assert json.loads(error.read())["code"] == "host_unavailable"
                finally:
                    writer.close()
                    await writer.wait_closed()
                print("PASS old-wrapper wire fixture: daemon reconnect remains compatible, transfers require wrapper relaunch")
            print(f"PASS {backend}: byte-identical private staging, real PTY paste, ack, no Enter, routing, auth replacement/SSE reconnect without replay, failed-stage ack/no paste, detach retention/no reception, exit cleanup")
        except Exception:
            for path in folder.glob("*.log"):
                print(path.name, path.read_text(errors="replace"))
            raise
        finally:
            # `host start` owns its wrapper process, not the short-lived start client.
            # Stop that exact fixture too if the test fails before signed session.end.
            pid_file = folder / "wrapper-pid-2"
            if not headless_done and pid_file.exists():
                pid = int(pid_file.read_text())
                try:
                    os.kill(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                for _ in range(100):
                    try:
                        os.kill(pid, 0)
                    except ProcessLookupError:
                        break
                    await asyncio.sleep(.1)
            for process in reversed(test.children):
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=8)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
            for log in test.logs:
                log.close()
            for fd in test.fds:
                os.close(fd)


async def legacy(target):
    with tempfile.TemporaryDirectory(prefix="conversations-legacy-files-") as temporary:
        folder = Path(temporary)
        test = isolated_test(target, folder)
        home = folder / "home"
        home.mkdir()
        test.environment.update(HOME=str(home), XDG_CONFIG_HOME=str(home),
            CONVERSATIONS_SERVER=test.url, BIND_ADDR=test.url.removeprefix("http://"),
            DATABASE_URL=f"sqlite://{folder}/test.db?mode=rwc", AUTH_ENABLED="false",
            SHUTDOWN_DRAIN_SECS="0")
        capture, ready = folder / "input", folder / "ready"
        agent = folder / "claude"
        agent.write_text(f"#!{os.sys.executable}\nimport os,tty\nfrom pathlib import Path\n"
            f"tty.setraw(0)\nPath({str(ready)!r}).touch()\n"
            f"with open({str(capture)!r},'ab',buffering=0) as f:\n"
            " while True: f.write(os.read(0,65536))\n")
        agent.chmod(0o700)
        test.environment["PATH"] = str(folder) + os.pathsep + test.environment["PATH"]
        master, slave = os.openpty()
        module.fcntl.ioctl(slave, module.termios.TIOCSWINSZ, module.struct.pack("HHHH", 24, 80, 0, 0))
        try:
            test.spawn([str(test.server)], "server")
            await test.wait(lambda: test.http("/healthz"), "legacy server")
            live = test.spawn([str(test.cli), "--server", test.url, "live"],
                "legacy", stdin=slave, stdout=slave, start_new_session=True)
            os.close(slave)
            slave = None
            os.set_blocking(master, False)
            async def drain_terminal():
                while live.poll() is None:
                    try:
                        os.read(master, 65536)
                    except BlockingIOError:
                        await asyncio.sleep(.05)
                    except OSError:
                        return
            drain = asyncio.create_task(drain_terminal())
            await test.wait(ready.exists, "legacy raw agent ready")
            sessions = await test.wait(lambda: test.http("/api/live")["live_sessions"], "legacy registration")
            ident = sessions[0]["id"]
            content = bytes(range(256))
            try:
                response = await asyncio.to_thread(test.http, f"/api/live/{ident}/files",
                    {"files": [{"name": "legacy ü.bin", "b64": base64.b64encode(content).decode()}]})
                raise AssertionError(f"unsigned legacy transfer was acknowledged: {response}")
            except urllib.error.HTTPError as error:
                assert error.code == 503 and json.loads(error.read())["code"] == "host_unavailable"
            await asyncio.sleep(.5)
            assert live.poll() is None, "unsigned transfer stopped the local terminal"
            assert not capture.exists() or capture.read_bytes() == b"", "unsigned file paths reached the PTY"
            os.write(master, b"LOCAL_STILL_WORKS")
            await test.wait(lambda: capture.exists() and capture.read_bytes() == b"LOCAL_STILL_WORKS",
                "local input remains usable")
            live.send_signal(signal.SIGINT)
            await test.wait(lambda: live.poll() is not None, "legacy teardown", timeout=20)
            await drain
            print("PASS legacy live/interactive: unsigned files never paste; same local terminal remains usable")
        except Exception:
            for path in folder.glob("*.log"):
                print(path.name, path.read_text(errors="replace"))
            raise
        finally:
            for process in reversed(test.children):
                if process.poll() is None:
                    process.kill()
                    process.wait()
            os.close(master)
            if slave is not None:
                os.close(slave)
            for log in test.logs:
                log.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--target-dir", type=Path, default=module.ROOT / "target")
    parser.add_argument("--backend", choices=["normal", "cmux", "legacy", "all"], default="all")
    args = parser.parse_args()
    if os.geteuid() == 0:
        parser.error("run as a non-root user: this test verifies staging permission failures")
    for backend in (["normal", "cmux", "legacy"] if args.backend == "all" else [args.backend]):
        asyncio.run(legacy(args.target_dir.resolve()) if backend == "legacy" else run(args.target_dir.resolve(), backend))
