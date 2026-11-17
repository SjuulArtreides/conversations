# /// script
# requires-python = ">=3.11"
# dependencies = ["websockets>=14,<16"]
# ///
"""Real host/wrappers/server with a fake cmux inventory; no native user windows."""
import argparse
import asyncio
import importlib.util
import json
import os
from pathlib import Path
import secrets
import shlex
import struct
import subprocess
import sys
import tempfile
import uuid

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]


async def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target")
    args = parser.parse_args()
    spec = importlib.util.spec_from_file_location("host_test", ROOT / "scripts/test-local-host.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    (ROOT / "tmp").mkdir(exist_ok=True)
    folder = Path(tempfile.mkdtemp(prefix="cmux-compat-", dir=ROOT / "tmp"))
    t = module.Test(args.target_dir.resolve(), folder)
    home = folder / "home"
    home.mkdir()
    workspace, neighbor, stale, recovery = [str(uuid.uuid4()) for _ in range(4)]
    inventory = folder / "inventory.json"
    failure = folder / "inventory-fails"
    def set_inventory():
        inventory.write_text(json.dumps({"windows": [{"id": "fixture-window", "workspaces": [
            {"id": workspace, "panes": [{"surfaces": [{"id": neighbor}]}]},
            {"id": "recovery-workspace", "panes": [{"surfaces": [{"id": recovery}]}]}]}]}))
    set_inventory()
    cmux = folder / "cmux"
    cmux.write_text("#!/bin/sh\ncase \"$*\" in\n*tree*) " +
        f"test ! -e {shlex.quote(str(failure))} || exit 1\ncat {shlex.quote(str(inventory))};;\n" +
        "*) echo '{}';;\nesac\n")
    cmux.chmod(0o700)
    t.environment.update(HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"),
        CMUX_WORKSPACE_ID="host-workspace", CMUX_SURFACE_ID="host-surface",
        CMUX_BUNDLED_CLI_PATH=str(cmux), CONVERSATIONS_CAPTURE="0",
        BIND_ADDR=t.url.removeprefix("http://"), DATABASE_URL=f"sqlite://{folder}/test.db?mode=rwc",
        AUTH_ENABLED="true", PASSWORD_AUTH_ENABLED="true", COOKIE_SECURE="false", SHUTDOWN_DRAIN_SECS="0")
    proxy_dir = folder / "proxy"
    proxy_dir.mkdir(mode=0o700)
    registrations = {}
    responses = {}
    sockets = set()
    async def proxy(client_reader, client_writer):
        sockets.add(client_writer)
        remote_reader, remote_writer = await asyncio.open_unix_connection(t.host_dir / "host.sock")
        sockets.add(remote_writer)
        ident = None
        async def relay(reader, writer, outbound):
            nonlocal ident
            try:
                while True:
                    header = await reader.readexactly(4)
                    payload = await reader.readexactly(struct.unpack("!I", header)[0])
                    event = json.loads(payload)
                    if outbound and event["type"] == "register":
                        ident = event["registration"]["launch"]["id"]
                        registrations[ident] = registrations.get(ident, 0) + 1
                    elif not outbound and ident:
                        responses.setdefault(ident, []).append(event["type"])
                    writer.write(header + payload)
                    await writer.drain()
            except (asyncio.IncompleteReadError, ConnectionError):
                pass
            finally:
                writer.close()
        await asyncio.gather(relay(client_reader, remote_writer, True), relay(remote_reader, client_writer, False))
    async def wrap(surface):
        ident = str(uuid.uuid4())
        master, slave = os.openpty()
        t.fds.append(master)
        t.environment.update(CMUX_WORKSPACE_ID=workspace, CMUX_SURFACE_ID=surface)
        process = t.spawn([str(t.cli), "host", "--runtime-dir", str(proxy_dir), "wrap",
            "--session-id", ident, "--", "/bin/sh", "-c",
            "stty -echo; while IFS= read -r line; do printf 'LOCAL:%s\\n' \"$line\"; done"], ident,
            stdin=slave, stdout=slave, start_new_session=True)
        os.close(slave)
        os.set_blocking(master, False)
        return ident, process, master
    proxy_server = None
    try:
        t.spawn([str(t.server)], "server")
        await t.wait(lambda: t.http("/healthz"), "server ready", timeout=30)
        t.http("/auth/register", {"email": "compat@example.test", "password": secrets.token_urlsafe(24)})
        cookie = folder / "cookie"
        cookie.write_text(t.cookie)
        cookie.chmod(0o600)
        t.spawn([str(t.cli), "--server", t.url, "host", "--runtime-dir", str(t.host_dir),
            "run", "--backend", "cmux", "--cookie-file", str(cookie)], "host")
        await t.wait(lambda: (t.host_dir / "host.sock").exists(), "host ready", timeout=30)
        proxy_server = await asyncio.start_unix_server(proxy, path=proxy_dir / "host.sock")
        live_id, live_child, _ = await wrap(neighbor)
        await t.wait(lambda: live_id in json.dumps(t.http("/api/live")), "neighbor registers")
        stale_id, stale_child, stale_master = await wrap(stale)
        await t.wait(lambda: "detach" in responses.get(stale_id, []), "missing surface receives Detach", timeout=10)
        attempts = registrations[stale_id]
        await asyncio.sleep(2)
        assert registrations[stale_id] == attempts, "detached wrapper keeps retrying"
        assert stale_child.poll() is None, "Detach killed local child"
        os.write(stale_master, b"still-running\n")
        output = bytearray()
        def local_echo():
            try:
                output.extend(os.read(stale_master, 65536))
            except BlockingIOError:
                pass
            return b"LOCAL:still-running" in output
        await t.wait(local_echo, "detached local child remains interactive")
        assert live_child.poll() is None and live_id in json.dumps(t.http("/api/live"))
        failure.touch()
        retry_id, retry_child, _ = await wrap(recovery)
        await t.wait(lambda: registrations.get(retry_id, 0) >= 2, "inventory failure retries")
        assert "detach" not in responses.get(retry_id, []), "failed inventory wrongly detached pane"
        assert retry_child.poll() is None
        failure.unlink()
        await t.wait(lambda: retry_id in json.dumps(t.http("/api/live")), "inventory recovery registers same wrapper")
        assert "detach" not in responses.get(retry_id, [])
        assert live_child.poll() is None and stale_child.poll() is None
        print("PASS: authoritative missing surface detaches without retries or killing local child; live neighbor unaffected")
        print("PASS: inventory failure retries without Detach and recovers the same wrapper")
    finally:
        if proxy_server:
            proxy_server.close()
        for stream in sockets:
            stream.transport.abort()
        t.cleanup()
        if proxy_server:
            await proxy_server.wait_closed()
        print("Evidence:", folder)


if __name__ == "__main__":
    asyncio.run(main())
