# /// script
# requires-python = ">=3.11"
# dependencies = ["websockets>=14,<16"]
# ///
"""Opt-in native cmux + installed Codex smoke; submits one harmless marker prompt.

Run inside cmux. Uses an isolated authenticated localhost server and host HOME.
Only its own disposable agent workspace is ended/closed.
"""

import argparse
import asyncio
import importlib.util
import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parents[1]


async def main():
    assert os.environ.get("CMUX_WORKSPACE_ID"), "run inside cmux"
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--cli", type=Path, default=ROOT / "target/release/arrakis-conversations"
    )
    parser.add_argument(
        "--server", type=Path, default=ROOT / "target/debug/conversations-server"
    )
    parser.add_argument(
        "--siblings",
        action="store_true",
        help="Verify automatic capture in additional tabs and splits",
    )
    parser.add_argument(
        "--close-pane",
        action="store_true",
        help="Closing the workspace must retire its checkpoint",
    )
    a = parser.parse_args()
    spec = importlib.util.spec_from_file_location(
        "host_test", ROOT / "scripts/test-local-host.py"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    (ROOT / "tmp").mkdir(exist_ok=True)
    folder = Path(tempfile.mkdtemp(prefix="native-restart-", dir=ROOT / "tmp"))
    t = module.Test(ROOT / "target", folder)
    t.cli = a.cli.resolve()
    t.server = a.server.resolve()
    home = folder / "home"
    home.mkdir()
    t.environment.update({k: v for k, v in os.environ.items() if k.startswith("CMUX_")})
    t.environment.update(
        HOME=str(home),
        XDG_CONFIG_HOME=str(home / ".config"),
        BIND_ADDR=t.url.removeprefix("http://"),
        DATABASE_URL=f"sqlite://{folder}/test.db?mode=rwc",
        AUTH_ENABLED="true",
        PASSWORD_AUTH_ENABLED="true",
        COOKIE_SECURE="false",
        SHUTDOWN_DRAIN_SECS="0",
    )
    cmux = os.environ.get(
        "CMUX_BUNDLED_CLI_PATH", "/Applications/cmux.app/Contents/Resources/bin/cmux"
    )
    ids = set()
    workspaces = set()
    try:
        t.spawn([str(t.server)], "server")
        await t.wait(lambda: t.http("/healthz"), "server")
        t.http(
            "/auth/register",
            {"email": "native@example.test", "password": secrets.token_urlsafe(24)},
        )
        cookie = folder / "cookie"
        cookie.write_text(t.cookie)
        cookie.chmod(0o600)
        t.spawn(
            [
                str(t.cli),
                "--server",
                t.url,
                "host",
                "--runtime-dir",
                str(t.host_dir),
                "--backend",
                "cmux",
                "--cookie-file",
                str(cookie),
            ],
            "host",
        )
        await t.wait(lambda: t.host_online(), "host")
        assert t.http("/api/live/start-new", {"agent": "codex"})["delivered"]
        await t.wait(lambda: t.status(), "native wrapper", timeout=25)
        first = t.status()[0]
        ident = first["launch"]["id"]
        ids.add(ident)
        workspaces.add(first["workspace"])
        if a.siblings:
            for action in (["new-surface"], ["new-split", "right"]):
                previous = {s["launch"]["id"] for s in t.status()}
                created = subprocess.run(
                    [
                        cmux,
                        "--json",
                        "--id-format",
                        "uuids",
                        *action,
                        "--workspace",
                        first["workspace"],
                        "--focus",
                        "false",
                    ],
                    capture_output=True,
                    text=True,
                    check=True,
                    timeout=10,
                )
                created_surface = json.loads(created.stdout)["surface_id"]
                # A no-focus terminal can remain dormant until its first read.
                subprocess.run(
                    [cmux, "read-screen", "--surface", created_surface, "--lines", "1"],
                    capture_output=True,
                    check=True,
                    timeout=10,
                )
                sibling = await t.wait(
                    lambda: next(
                        (s for s in t.status() if s["launch"]["id"] not in previous),
                        None,
                    ),
                    "new tab/split automatically captured with its own identity",
                    timeout=15,
                )
                assert sibling["launch"]["agent"] == "zsh", sibling
                assert sibling["workspace"].lower() != first["workspace"].lower(), (
                    sibling
                )
                workspaces.add(sibling["workspace"])
                ids.add(sibling["launch"]["id"])
            assert any(s["launch"]["id"] == ident for s in t.status())
            print(
                "PASS: extra tab and split automatically captured with distinct identities",
                flush=True,
            )
        viewer = await t.view(ident)
        await t.output(viewer, "Ask Codex", timeout=25)
        await t.input(viewer, "Reply only CMUX_RESTART_OK. Do not use tools.")
        await asyncio.sleep(0.2)
        await t.input(viewer, "\r")
        await viewer.close()
        record = await t.wait(
            lambda: t.http(f"/api/sessions/{ident}").get("agent_session_id"),
            "real Codex native identity",
            timeout=35,
        )
        print("Captured native ID:", record, flush=True)
        t.http(f"/api/live/{ident}/complete", {})
        assert t.http(f"/api/sessions/{ident}/reopen", {})["delivered"]
        replacement = await t.wait(
            lambda: next(
                (
                    s
                    for s in t.status()
                    if s["launch"]["id"] == ident and "resume" in s["launch"]["command"]
                ),
                None,
            ),
            "native resumed wrapper",
            timeout=30,
        )
        workspaces.add(replacement["workspace"])
        assert (
            replacement["launch"]["command"][
                replacement["launch"]["command"].index("resume") + 1
            ]
            == record
        )
        await asyncio.sleep(3)
        assert any(s["launch"]["id"] == ident for s in t.status()), (
            "resumed Codex exited"
        )
        screen = subprocess.check_output(
            [cmux, "read-screen", "--surface", replacement["surface"], "--lines", "40"],
            text=True,
            timeout=10,
        )
        assert "open in another app" not in screen, screen
        assert "Ask Codex" in screen, screen
        assert t.http(f"/api/sessions/{ident}")["agent_session_id"] == record
        if a.close_pane:
            subprocess.run(
                [
                    cmux,
                    "rpc",
                    "workspace.close",
                    json.dumps(
                        {"workspace_id": replacement["workspace"], "force": True}
                    ),
                ],
                check=True,
                capture_output=True,
                timeout=10,
            )
            config = (
                home
                / (
                    "Library/Application Support"
                    if sys.platform == "darwin"
                    else ".config"
                )
                / "arrakis-conversations/open-sessions.json"
            )
            await t.wait(
                lambda: (
                    not any(
                        s["session_id"] == ident
                        for s in json.loads(config.read_text())["servers"].get(
                            t.url, []
                        )
                    )
                ),
                "closed workspace checkpoint retired",
                timeout=10,
            )
            print("PASS: closing a workspace retires its checkpoint", flush=True)
        print(
            "PASS: real cmux/Codex new-session native identity and web restart preserve the same conversation",
            flush=True,
        )
    finally:
        try:
            for s in t.status():
                ids.add(s["launch"]["id"])
                workspaces.add(s["workspace"])
        except Exception:
            pass
        for s in t.status() if t.children[-1].poll() is None else []:
            for mode, command in [
                ("screen", ["read-screen", "--surface", s["surface"], "--lines", "30"]),
                (
                    "binding",
                    ["surface", "resume", "show", "--json", "--surface", s["surface"]],
                ),
            ]:
                result = subprocess.run(
                    [cmux, *command], capture_output=True, text=True, timeout=10
                )
                (folder / (mode + ".txt")).write_text(result.stdout + result.stderr)
        for ident in ids:
            try:
                t.http(f"/api/live/{ident}/complete", {})
            except Exception:
                pass
        await asyncio.sleep(1)
        for ws in workspaces:
            subprocess.run(
                [
                    cmux,
                    "rpc",
                    "workspace.close",
                    json.dumps({"workspace_id": ws, "force": True}),
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=10,
            )
        for p in reversed(t.children):
            if p.poll() is None:
                p.terminate()
        for p in reversed(t.children):
            try:
                p.wait(timeout=5)
            except subprocess.TimeoutExpired:
                p.kill()
        print("Evidence:", folder, flush=True)


asyncio.run(main())
