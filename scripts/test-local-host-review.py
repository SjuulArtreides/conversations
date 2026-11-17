# /// script
# requires-python = ">=3.11"
# dependencies = ["websockets>=14,<16"]
# ///
"""Focused real-server regressions from the independent cmux review."""
import argparse
import asyncio
import importlib.util
import json
import os
from pathlib import Path
import secrets
import signal
import subprocess
import sys
import tempfile
import uuid

root = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("host_test", root / "scripts/test-local-host.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--target-dir", type=Path, default=root / "target")
    parser.add_argument("case", choices=["dismiss", "end", "offline-end", "parent-offline", "self-capture", "interrupt", "server-restart", "reopen"])
    args = parser.parse_args()
    (root / "tmp").mkdir(exist_ok=True)
    folder = Path(tempfile.mkdtemp(prefix="host-review-", dir=root / "tmp"))
    test = module.Test(args.target_dir.resolve(), folder)
    child_pid = None
    parent_wrapper = None
    try:
        test.environment.update(BIND_ADDR=test.url.removeprefix("http://"),
            DATABASE_URL=f"sqlite://{folder}/test.db?mode=rwc", AUTH_ENABLED="true",
            PASSWORD_AUTH_ENABLED="true", COOKIE_SECURE="false", SHUTDOWN_DRAIN_SECS="0")
        own_id = str(uuid.uuid4())
        if args.case == "self-capture":
            test.environment.update(CONVERSATIONS_WRAPPED=own_id,
                CONVERSATIONS_HOST_DIR=str(folder / "host"))
        server = test.spawn([str(test.server)], "server")
        await test.wait(lambda: test.http("/healthz"), "server ready")
        test.http("/auth/register", {"email": "review@example.test", "password": secrets.token_urlsafe(24)})
        cookie = folder / "cookie"
        cookie.write_text(test.cookie)
        cookie.chmod(0o600)
        if args.case == "parent-offline":
            parent_dir = folder / "stopped-parent"
            parent_wrapper = subprocess.Popen([str(test.cli), "host", "--runtime-dir", str(parent_dir),
                "wrap", "--headless", "--session-id", own_id, "--", sys.executable,
                "-c", "import time; time.sleep(30)"], env=test.environment,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            test.environment.update(CONVERSATIONS_WRAPPED=own_id,
                CONVERSATIONS_HOST_DIR=str(parent_dir),
                CONVERSATIONS_WRAPPER_PID=str(parent_wrapper.pid))
        def host():
            return test.spawn([str(test.cli), "--server", test.url, "host", "--runtime-dir",
                str(test.host_dir), "--cookie-file", str(cookie)], "host", start_new_session=True)
        daemon = host()
        await test.wait(lambda: (test.host_dir / "host.sock").exists(), "host ready")
        marker = folder / "child-pid"
        program = "import os,time; from pathlib import Path; Path(" + repr(str(marker)) + ").write_text(str(os.getpid())); print('REVIEW_READY',flush=True); time.sleep(120)"
        result = subprocess.run([str(test.cli), "host", "--runtime-dir", str(test.host_dir),
            "start", "--", sys.executable, "-c", program], capture_output=True, timeout=15, check=True)
        ident = result.stdout.decode().split()[-1]
        await test.wait(marker.exists, "child started")
        child_pid = int(marker.read_text())
        await test.wait(lambda: any(s["id"] == ident for s in test.http("/api/live")["live_sessions"]), "session visible")
        if args.case == "dismiss":
            test.http(f"/api/live/{ident}/dismiss", {})
            await test.wait(lambda: all(s["launch"]["id"] != ident for s in test.status()),
                "dismissed wrapper deregisters without stopping child")
            # Must stay graduated across more than one reconnect cycle.
            for _ in range(30):
                await asyncio.sleep(.1)
                assert not any(s["id"] == ident for s in test.http("/api/live")["live_sessions"]), "dismissed session resurrected"
                os.kill(child_pid, 0)
            daemon.terminate()
            daemon.wait(timeout=5)
            host()
            await asyncio.sleep(2)
            assert not any(s["id"] == ident for s in test.http("/api/live")["live_sessions"]), "restart resurrected dismissed session"
            os.kill(child_pid, 0)
            assert test.http(f"/api/sessions/{ident}/reopen", {})["delivered"]
            await test.wait(lambda: any(s["launch"]["id"] == ident for s in test.status()),
                "dismissed conversation reopens", timeout=20)
            os.kill(child_pid, 0)
            test.http(f"/api/live/{ident}/complete", {})
            print("PASS: dismiss deregisters, stays graduated through restart, and can reopen without killing the original child")
        elif args.case == "end":
            test.http(f"/api/live/{ident}/complete", {})
            def ended():
                try:
                    os.kill(child_pid, 0)
                    return False
                except ProcessLookupError:
                    return True
            await test.wait(ended, "author End stops child despite WebSocket close")
            assert not any(s["id"] == ident for s in test.http("/api/live")["live_sessions"])
            print("PASS: author End stops child without resurrection")
        elif args.case == "parent-offline":
            assert daemon.poll() is None
            assert any(s["launch"]["id"] == ident for s in test.status())
            assert (parent_dir / "released" / f"{own_id}-{parent_wrapper.pid}").exists()
            for key in ("CONVERSATIONS_WRAPPED", "CONVERSATIONS_HOST_DIR", "CONVERSATIONS_WRAPPER_PID"):
                test.environment.pop(key)
            test.spawn([str(test.cli), "--server", test.url, "host", "--runtime-dir",
                str(parent_dir), "--cookie-file", str(cookie)], "parent-host")
            await test.wait(lambda: (parent_dir / "host.sock").exists(), "parent host started")
            await asyncio.sleep(2)
            parent_status = subprocess.run([str(test.cli), "host", "--runtime-dir", str(parent_dir),
                "status", "--json"], capture_output=True, check=True)
            assert all(s["launch"]["id"] != own_id for s in json.loads(parent_status.stdout)["sessions"])
            assert not any(s["id"] == own_id for s in test.http("/api/live")["live_sessions"])
            print("PASS: offline parent release marker prevents later capture of the new host")
        elif args.case == "self-capture":
            nested = subprocess.Popen([str(test.cli), "host", "--runtime-dir", str(test.host_dir),
                "wrap", "--headless", "--session-id", own_id, "--", sys.executable,
                "-c", "import time; time.sleep(3)"],
                env={k: v for k, v in test.environment.items() if k != "CONVERSATIONS_WRAPPED"},
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                await asyncio.sleep(1)
                assert all(s["launch"]["id"] != own_id for s in test.status())
                assert not any(s["id"] == own_id for s in test.http("/api/live")["live_sessions"])
            finally:
                nested.wait(timeout=5)
            print("PASS: a normal host never captures its own wrapped terminal")
        elif args.case == "offline-end":
            daemon.terminate()
            daemon.wait(timeout=5)
            test.http(f"/api/live/{ident}/complete", {})
            daemon = host()
            await asyncio.sleep(2)
            await test.wait(lambda: all(s["launch"]["id"] != ident for s in test.status()),
                "graduated offline session detaches on host restart")
            assert not any(s["id"] == ident for s in test.http("/api/live")["live_sessions"])
            print("PASS: End while host is down stays graduated on restart")
        elif args.case == "reopen":
            test.http(f"/api/live/{ident}/complete", {})
            response = test.http(f"/api/sessions/{ident}/reopen", {})
            assert response["delivered"]
            await test.wait(lambda: any(s["launch"]["id"] == ident and
                s["launch"]["command"][0] == "zsh" for s in test.status()),
                "same-id reopen registers", timeout=20)
            await test.wait(lambda: any(s["id"] == ident for s in test.http("/api/live")["live_sessions"]),
                "reopened session is live", timeout=20)
            test.http(f"/api/live/{ident}/complete", {})
            print("PASS: immediate End then Reopen returns the same live conversation")
        elif args.case == "server-restart":
            server.terminate()
            server.wait(timeout=5)
            test.spawn([str(test.server)], "server")
            await test.wait(lambda: test.http("/healthz"), "server restarted")
            viewer = await test.view(ident)
            try:
                await test.output(viewer, "REVIEW_READY")
                os.kill(child_pid, 0)
            finally:
                await viewer.close()
            print("PASS: server restart reconnects the same live terminal")
        else:
            os.killpg(daemon.pid, signal.SIGINT)
            daemon.wait(timeout=5)
            await asyncio.sleep(.5)
            os.kill(child_pid, 0)
            host()
            await test.wait(lambda: any(s["launch"]["id"] == ident for s in test.status()), "headless child reconnects after Ctrl-C")
            os.kill(child_pid, 0)
            print("PASS: foreground-group Ctrl-C preserves headless child and reconnects")
    finally:
        if parent_wrapper is not None and parent_wrapper.poll() is None:
            parent_wrapper.terminate()
            parent_wrapper.wait(timeout=5)
        if child_pid is not None:
            try:
                os.kill(child_pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        test.cleanup()
        print(f"Evidence: {folder}")

asyncio.run(main())
