# /// script
# requires-python = ">=3.11"
# dependencies = ["websockets>=14,<16"]
# ///
"""Real localhost host/server transcript regression, using explicit native identities.

Run with uv run scripts/test-cmux-pr-ingestion.py --target-dir target.
The native agent and terminal are inert fixtures; ingestion, authenticated server,
PR auto-detection and reconnect are real. No GitHub writes or user state changes.
"""
import argparse
import asyncio
import importlib.util
import json
import os
from pathlib import Path
import secrets
import shlex
import sqlite3
import subprocess
import sys
import tempfile
import uuid

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]


def record(kind, payload, second=0):
    return json.dumps({"timestamp": f"2026-10-19T12:00:{second:02d}.000Z",
                       "type": kind, "payload": payload}) + "\n"


def message(text, second):
    return record("response_item", {"type": "message", "role": "assistant",
        "content": [{"type": "output_text", "text": text}]}, second)


async def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target")
    parser.add_argument("--backend", choices=["normal", "cmux"], default="normal")
    parser.add_argument("--cmux-window", help="Existing isolated cmux test window UUID; caller owns it")
    args = parser.parse_args()
    if args.backend == "cmux" and not args.cmux_window:
        parser.error("--backend cmux requires --cmux-window (an existing isolated test window)")
    spec = importlib.util.spec_from_file_location("host_test", ROOT / "scripts/test-local-host.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    (ROOT / "tmp").mkdir(exist_ok=True)
    folder = Path(tempfile.mkdtemp(prefix="cmux-pr-ingestion-", dir=ROOT / "tmp"))
    t = module.Test(args.target_dir.resolve(), folder)
    def status():
        try:
            result = subprocess.run([str(t.cli), "host", "--runtime-dir", str(t.host_dir),
                "status", "--json"], capture_output=True, timeout=8, check=True)
            return json.loads(result.stdout)["sessions"]
        except subprocess.TimeoutExpired:
            return []
    t.status = status
    home = folder / "home"
    home.mkdir()
    codex = home / ".codex"
    transcripts = codex / "sessions"
    transcripts.mkdir(parents=True)
    config = home / ("Library/Application Support" if sys.platform == "darwin" else ".config") / "arrakis-conversations/open-sessions.json"
    config.parent.mkdir(parents=True)
    t.environment.update(HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"),
        CODEX_HOME=str(codex), BIND_ADDR=t.url.removeprefix("http://"),
        DATABASE_URL=f"sqlite://{folder}/test.db?mode=rwc", AUTH_ENABLED="true",
        PASSWORD_AUTH_ENABLED="true", COOKIE_SECURE="false", SHUTDOWN_DRAIN_SECS="0")
    # Deliberately same cwd for all three transcripts, including a newer decoy.
    native = [str(uuid.uuid4()) for _ in range(3)]
    paths = []
    for index, identity in enumerate(native):
        path = transcripts / f"rollout-2026-10-19-{identity}.jsonl"
        path.write_text(record("session_meta", {"id": identity, "cwd": str(folder),
            "timestamp": "2026-10-19T12:00:00.000Z"}) +
            message(f"BOUND_{index} https://github.com/example/fixture/pull/{index + 1}", 1))
        paths.append(path)
    def rows(sql, params=()):
        with sqlite3.connect(folder / "test.db") as db:
            return db.execute(sql, params).fetchall()
    def texts(identity):
        return [json.loads(row[0])["text"] for row in rows(
            "SELECT payload FROM live_messages WHERE live_id=? ORDER BY seq", (identity,))]
    wrappers = []
    surfaces = []
    native_window = None
    native_workspaces = []
    cmux = "/Applications/cmux.app/Contents/Resources/bin/cmux"
    def cmux_rpc(method, params):
        result = subprocess.run([cmux, "--json", "rpc", method, json.dumps(params)],
            capture_output=True, text=True, timeout=10, check=True)
        return json.loads(result.stdout)
    def start_host():
        return t.spawn([str(t.cli), "--server", t.url, "host", "--runtime-dir",
            str(t.host_dir), "run", "--backend", args.backend,
            "--cookie-file", str(cookie)], "host")
    try:
        t.spawn([str(t.server)], "server")
        await t.wait(lambda: t.http("/healthz"), "server readiness")
        t.http("/auth/register", {"email": "ingestion@example.test", "password": secrets.token_urlsafe(24)})
        cookie = folder / "cookie"
        cookie.write_text(t.cookie)
        cookie.chmod(0o600)
        if args.backend == "cmux":
            native_window = args.cmux_window
            context = cmux_rpc("workspace.create", {"window_id": native_window,
                "initial_command": "/bin/sleep 120", "workspace_env": {"CONVERSATIONS_CAPTURE": "0"},
                "title": "Conversations isolated PR host", "focus": False})
            native_workspaces.append(context["workspace_id"])
            t.environment.update(CMUX_WORKSPACE_ID=context["workspace_id"],
                CMUX_SURFACE_ID=context["surface_id"], CMUX_BUNDLED_CLI_PATH=cmux)
        host = start_host()
        await t.wait(lambda: (t.host_dir / "host.sock").exists(), "host socket", timeout=30)
        identities = [str(uuid.uuid4()) for _ in range(2)]
        for identity in identities:
            if args.backend == "cmux":
                command = ["/usr/bin/env", f"HOME={home}", f"CODEX_HOME={codex}",
                    f"XDG_CONFIG_HOME={home / '.config'}", str(t.cli), "host", "--runtime-dir",
                    str(t.host_dir), "wrap", "--session-id", identity, "--agent", "codex",
                    "--cwd", str(folder), "--", "/bin/sh", "-c", "read answer"]
                created = cmux_rpc("workspace.create", {"window_id": native_window,
                    "title": "Conversations isolated PR regression", "initial_command": shlex.join(command),
                    "workspace_env": {"CONVERSATIONS_CAPTURE": "0"}, "focus": False})
                assert created["workspace_id"] and created["surface_id"]
                native_workspaces.append(created["workspace_id"])
                surfaces.append(created)
                continue
            master, slave = os.openpty()
            t.fds.append(master)
            wrappers.append(t.spawn([str(t.cli), "host", "--runtime-dir", str(t.host_dir),
                "wrap", "--session-id", identity, "--agent", "codex", "--cwd", str(folder),
                "--", "/bin/sh", "-c", "read answer"], identity,
                stdin=slave, stdout=slave, start_new_session=True))
            os.close(slave)
        await t.wait(lambda: len(t.status()) == 2, "two registered agent panes", timeout=30)
        await t.wait(lambda: t.http("/api/prs/sync/available")["available"], "PR sync worker connected")
        assert t.http(f"/api/sessions/{identities[0]}/prs/sync", {})["delivered"], "Sync now not delivered to host"
        await asyncio.sleep(1)
        assert not rows("SELECT payload FROM live_messages"), "unbound pane selected transcript by cwd"
        manual_title = "Keep my chosen pane title"
        t.http(f"/api/sessions/{identities[0]}/title", {"title": manual_title}, method="PUT")
        for created, bound in zip(surfaces, native):
            subprocess.run([cmux, "surface", "resume", "set", "--workspace", created["workspace_id"],
                "--surface", created["surface_id"], "--kind", "codex", "--checkpoint", bound,
                "--cwd", str(folder), "--", "codex", "resume", bound],
                capture_output=True, timeout=10, check=True)
        config.write_text(json.dumps({"servers": {t.url: [
            {"session_id": identity, "agent": "codex", "cwd": str(folder), "resume_id": bound}
            for identity, bound in zip(identities, native)]}}))
        for index, identity in enumerate(identities):
            await t.wait(lambda: f"BOUND_{index}" in " ".join(texts(identity)),
                         f"bound native transcript {index} history backfill", timeout=20)
            assert len(texts(identity)) == 1, texts(identity)
            assert all(f"BOUND_{index}" in text for text in texts(identity))
            assert rows("SELECT number, detected FROM session_prs WHERE session_id=?", (identity,)) == [(index + 1, 1)]
        assert t.http(f"/api/sessions/{identities[0]}")["title"] == manual_title
        print("PASS: late explicit native identities backfill separate same-cwd histories and auto-link PRs; newer decoy ignored")
        for index, path in enumerate(paths[:2]):
            with path.open("a") as out:
                out.write(message(f"DELTA_{index} https://github.com/example/fixture/pull/{index + 11}", 2))
        for index, identity in enumerate(identities):
            await t.wait(lambda: len(texts(identity)) == 2, f"incremental transcript {index}")
            assert f"DELTA_{index}" in texts(identity)[1]
            assert sorted(row[0] for row in rows("SELECT number FROM session_prs WHERE session_id=?", (identity,))) == [index + 1, index + 11]
        host.terminate()
        host.wait(timeout=5)
        start_host()
        await t.wait(lambda: len(t.status()) == 2, "host reconnects existing wrappers", timeout=30)
        with paths[0].open("a") as out:
            out.write(message("AFTER_RECONNECT", 3))
        await t.wait(lambda: len(texts(identities[0])) == 3, "ingestion after reconnect", timeout=20)
        await asyncio.sleep(1)
        assert len(texts(identities[0])) == 3 and len(texts(identities[1])) == 2, "reconnect duplicated or cross-wired messages"
        assert all(process.poll() is None for process in wrappers), "host reconnect killed agent pane"
        print("PASS: incremental PR pickup and reconnect backfill are deduplicated; both agent panes survive")
        t.http(f"/api/live/{identities[0]}/complete", {})
        await t.wait(lambda: all(item["launch"]["id"] != identities[0] for item in t.status()),
                     "End detaches only its pane")
        with paths[0].open("a") as out:
            out.write(message("ENDED_SESSION_MUST_NOT_INGEST", 4))
        with paths[1].open("a") as out:
            out.write(message("NEIGHBOR_STILL_INGESTS", 4))
        await t.wait(lambda: "NEIGHBOR_STILL_INGESTS" in " ".join(texts(identities[1])),
                     "neighbor still ingests after End")
        await asyncio.sleep(1)
        assert not rows("SELECT payload FROM live_messages WHERE live_id=? AND payload LIKE ?",
                        (identities[0], "%ENDED_SESSION_MUST_NOT_INGEST%"))
        print("PASS: End stops its transcript worker while the neighboring pane continues")
    finally:
        for workspace_id in native_workspaces:
            try:
                cmux_rpc("workspace.close", {"workspace_id": workspace_id})
            except (OSError, subprocess.SubprocessError):
                pass
        t.cleanup()
        print("Evidence:", folder)


if __name__ == "__main__":
    asyncio.run(main())
