#!/usr/bin/env python3
"""Copy native fixtures through two real localhost hosts, with optional native loaders.

All homes, browser credentials, transcripts, and databases are disposable.
No user credentials or model requests are used.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import selectors
import shutil
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
MARKER = "Isolated migration context " * 800


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class Host:
    def __init__(self, cli, home, tool_dir):
        self.cli, self.home = cli, home
        home.mkdir(mode=0o700)
        self.runtime = home / "host"
        self.project = home / "project"
        self.project.mkdir()
        self.env = {key: value for key, value in os.environ.items()
                    if key in {"PATH", "LANG", "LC_ALL", "SSL_CERT_FILE", "NIX_SSL_CERT_FILE"}}
        self.env.update(HOME=str(home), CODEX_HOME=str(home / ".codex"),
                        XDG_DATA_HOME=str(home / "data"), XDG_CONFIG_HOME=str(home / "config"),
                        XDG_CACHE_HOME=str(home / "cache"), XDG_RUNTIME_DIR=str(home / "run"),
                        CONVERSATIONS_HOST_SETTINGS_FILE=str(home / "settings.json"))
        (home / "run").mkdir(mode=0o700)
        if tool_dir:
            self.env["PATH"] = str(tool_dir) + os.pathsep + self.env["PATH"]
        opener = home / "browser"
        opener.write_text("#!/usr/bin/env python3\nimport pathlib,sys\npathlib.Path(__file__).with_name('bootstrap').write_text(sys.argv[1])\n")
        opener.chmod(0o700)
        self.env["CONVERSATIONS_BROWSER"] = str(opener)
        self.process = None
        self.cookie = None
        self.start()

    def start(self):
        bootstrap = self.home / "bootstrap"
        bootstrap.unlink(missing_ok=True)
        self.base = f"http://127.0.0.1:{port()}"
        settings = self.home / "settings.json"
        values = json.loads(settings.read_text()) if settings.exists() else {}
        values["web"] = dict(values.get("web", {}), port=int(self.base.rsplit(":", 1)[1]))
        settings.write_text(json.dumps(values))
        self.process = subprocess.Popen([str(self.cli), "--server", "http://127.0.0.1:1", "host",
            "--runtime-dir", str(self.runtime)],
            cwd=self.project, env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 15
        # The host never opens a browser itself; ask it for a fresh owner link.
        while subprocess.run([str(self.cli), "open", "--runtime-dir",
                str(self.runtime)], cwd=self.project, env=self.env, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL, timeout=20).returncode != 0:
            assert self.process.poll() is None, "test host failed to start"
            assert time.monotonic() < deadline, "test host did not answer open"
            time.sleep(0.1)
        while not bootstrap.exists():
            assert self.process.poll() is None, "test host failed to start"
            assert time.monotonic() < deadline, "test host did not produce a bootstrap link"
            time.sleep(0.05)
        token = urllib.parse.parse_qs(urllib.parse.urlparse(bootstrap.read_text()).fragment)["token"][0]
        request = urllib.request.Request(self.base + "/api/unlock", data=json.dumps({"token": token}).encode(),
                    headers={"Content-Type": "application/json", "Origin": self.base})
        with urllib.request.urlopen(request, timeout=10) as response:
            self.cookie = response.headers["Set-Cookie"].split(";", 1)[0]
        bootstrap.unlink()

    def request(self, path, data=None, auth=True):
        headers = {"Origin": self.base, "Content-Type": "application/json"}
        if auth:
            headers["Cookie"] = self.cookie
        request = urllib.request.Request(self.base + path,
                    data=None if data is None else json.dumps(data).encode(), headers=headers)
        with urllib.request.urlopen(request, timeout=45) as response:
            return json.load(response)

    def seed(self, agent, native):
        session = str(uuid.uuid4())
        with sqlite3.connect(self.runtime / "authz.db") as db:
            db.execute("INSERT INTO sessions(session_id,agent,cwd,resume_id,launcher,adk_statusline,origin,created_at,updated_at) VALUES(?,?,?,?,?,0,'adopted',?,?)",
                (session, agent, str(self.project), native, "opencode", int(time.time()), int(time.time())))
        # Initialize the real history schema through its read endpoint.
        self.request(f"/api/local/sessions/{session}/history")
        return session

    def stop(self):
        if self.process:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()


def copy(source, destination, session):
    transfer, target = str(uuid.uuid4()), str(uuid.uuid4())
    offset, data = 0, bytearray()
    while True:
        page = source.request(f"/api/local/sessions/{session}/migration?transfer={transfer}&offset={offset}")
        data.extend(base64.urlsafe_b64decode(page["b64"] + "=" * (-len(page["b64"]) % 4)))
        chunk = dict(transfer=transfer, destination=target, cwd=str(destination.project),
                     manifest=page["manifest"], offset=offset, b64=page["b64"])
        result = destination.request("/api/local/migration", chunk)
        assert result["detail"] == ("copied" if page["next"] is None else "receiving")
        if page["next"] is None:
            break
        assert not any(row["id"] == target for row in destination.request("/api/local/sessions")["sessions"])
        offset = page["next"]
    assert hashlib.sha256(data).digest() == base64.urlsafe_b64decode(page["manifest"]["sha256"] + "=")
    assert destination.request("/api/local/migration", chunk)["detail"] == "copied"
    assert any(row["id"] == session for row in source.request("/api/local/sessions")["sessions"])
    return target


def codex_resume(binary, destination, native, cwd, expected_cwd):
    child = subprocess.Popen([binary, "app-server"], env=destination.env, cwd=destination.project,
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    def rpc(ident, method, params):
        child.stdin.write(json.dumps(dict(id=ident, method=method, params=params)) + "\n")
        child.stdin.flush()
        selector = selectors.DefaultSelector()
        selector.register(child.stdout, selectors.EVENT_READ)
        try:
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                if not selector.select(0.5):
                    continue
                line = child.stdout.readline()
                assert line, "native Codex exited before answering"
                reply = json.loads(line)
                if reply.get("id") == ident:
                    assert "result" in reply, f"native Codex rejected {method}"
                    return reply["result"]
            raise AssertionError(f"native Codex timed out on {method}")
        finally:
            selector.close()
    try:
        rpc(1, "initialize", {"clientInfo": {"name": "migration-test", "version": "1"}})
        read = rpc(2, "thread/read", {"threadId": native, "includeTurns": True})
        assert MARKER in json.dumps(read)
        params = {"threadId": native}
        if cwd is not None:
            params["cwd"] = str(cwd)
        resumed = rpc(3, "thread/resume", params)
        assert resumed["thread"]["id"] == native
        assert resumed["thread"]["cwd"] == str(expected_cwd)
    finally:
        child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, default=ROOT / "target/debug/arrakis-conversations")
    parser.add_argument("--codex", default=shutil.which("codex"))
    parser.add_argument("--opencode", type=Path)
    args = parser.parse_args()
    (ROOT / "tmp").mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="session-migration-", dir=ROOT / "tmp") as temp:
        base = Path(temp)
        source = destination = None
        try:
            tool_dir = args.opencode.resolve().parent if args.opencode else None
            source = Host(args.cli.resolve(), base / "source", tool_dir)
            destination = Host(args.cli.resolve(), base / "destination", tool_dir)
            for host, path in [(source, "/api/local/sessions/unknown/migration?transfer=x&offset=0"),
                               (destination, "/api/local/migration")]:
                try:
                    host.request(path, {} if path.endswith("migration") else None, auth=False)
                    raise AssertionError("migration allowed an unauthenticated browser")
                except urllib.error.HTTPError as error:
                    assert error.code == 401
            for agent in ["claude", "codex"]:
                native = str(uuid.uuid4())
                if agent == "claude":
                    folder = source.home / ".claude/projects/project"
                    rows = [{"type":"user","sessionId":native,"uuid":"original","cwd":str(source.project),"message":{"role":"user","content":MARKER}}]
                    filename = native + ".jsonl"
                else:
                    folder = source.home / ".codex/sessions/2026/11/10"
                    rows = [{"type":"session_meta","payload":{"id":native,"timestamp":"2026-11-10T12:00:00Z","cwd":str(source.project),"source":"cli","originator":"codex_cli_rs","cli_version":"0.167.0","model_provider":"openai"}},
                            {"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":MARKER}]}},
                            {"type":"event_msg","payload":{"type":"user_message","message":MARKER,"images":[],"local_images":[],"text_elements":[]}}]
                    filename = f"rollout-2026-11-10T12-00-00-{native}.jsonl"
                if agent == "codex":
                    rows.append({"type": "event_msg", "payload": {
                        "type": "thread_settings_applied", "thread_id": native,
                        "thread_settings": {
                            "cwd": str(source.project), "runtime_workspace_roots": [str(source.project)],
                            "approval_policy": "never", "approvals_reviewer": "user",
                            "permission_profile": {"type": "disabled"}, "disabled_plugin_ids": [],
                            "model": "gpt-5.4", "model_provider_id": "openai", "service_tier": "default",
                            "collaboration_mode": {"mode": "default", "settings": {
                                "model": "gpt-5.4", "reasoning_effort": None, "developer_instructions": None}},
                        }}})
                folder.mkdir(parents=True)
                transcript = folder / filename
                original = "".join(json.dumps({"timestamp":"2026-11-10T12:00:00Z",**row}) + "\n" for row in rows)
                transcript.write_text(original)
                session = source.seed(agent, native)
                target = copy(source, destination, session)
                assert transcript.read_text() == original
                destination.stop()
                destination.start()
                indexed = next(row for row in destination.request("/api/local/sessions")["sessions"] if row["id"] == target)
                assert indexed["reopenable"] and indexed["resume_mode"] == "native"
                history = destination.request(f"/api/local/sessions/{target}/history")
                assert history["items"]
                if agent == "codex" and args.codex:
                    # Each daemon starts with the copied thread unloaded, as on migration.
                    codex_resume(args.codex, destination, native, None, source.project)
                    codex_resume(args.codex, destination, native, destination.project, destination.project)
                print(f"PASS: {agent} native snapshot copy, source retention, restart, and history")
            if args.opencode:
                # The official native importer validates this fixture and supplies the real database schema.
                native = "ses_" + uuid.uuid4().hex[:26]
                template = subprocess.run([str(args.opencode.resolve()), "api", "POST", "/api/session", "--data", json.dumps({"title":"Migration fixture","location":{"directory":str(source.project)}}), "--standalone"],env=source.env,cwd=source.project,capture_output=True,timeout=25)
                assert template.returncode == 0, "native OpenCode template creation failed"
                info = json.loads(template.stdout)["data"]
                info["id"] = native
                fixture = {"info":info,
                           "messages":[{"id":"msg_"+uuid.uuid4().hex[:26],"type":"user","text":MARKER,"time":{"created":1794312000000}}]}
                path = source.home / "opencode-fixture.json"
                path.write_text(json.dumps(fixture))
                result = subprocess.run([str(args.opencode.resolve()),"session","import",str(path),"--directory",str(source.project),"--standalone"],env=source.env,cwd=source.project,capture_output=True,timeout=25)
                assert result.returncode == 0, "native OpenCode fixture import failed"
                session = source.seed("opencode",native)
                target = copy(source,destination,session)
                history = destination.request(f"/api/local/sessions/{target}/history")
                assert history["items"], "native OpenCode history was lost"
                result = subprocess.run([str(args.opencode.resolve()),"session","export",native,"--standalone"],env=destination.env,cwd=destination.project,capture_output=True,timeout=25)
                assert result.returncode == 0 and MARKER in result.stdout.decode(), "native OpenCode did not load migrated context"
                print("PASS: OpenCode native export/import and destination context loading")
        finally:
            for host in [source, destination]:
                if host:
                    host.stop()


if __name__ == "__main__":
    main()
