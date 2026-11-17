#!/usr/bin/env python3
"""A reused Codex MCP process must never act as another local native thread."""
import importlib.util
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import tempfile
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("messaging_fixture", Path(__file__).with_name("test-agent-messaging-mcp.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class LocalFixture:
    """Disposable central server and real local authority, shared by MCP smokes."""
    def __init__(self, cli, scratch, log):
        self.cli, self.scratch = cli, scratch
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        self.origin = f"http://127.0.0.1:{port}"
        self.environment = {k: v for k, v in os.environ.items()
            if not k.startswith(("CONVERSATIONS_", "CMUX_", "GOOGLE_", "OIDC_"))}
        stub = scratch / "claude-stub"
        stub.write_text("#!/bin/sh\n/bin/sleep 2\n")
        stub.chmod(0o700)
        self.environment.update(HOME=str(scratch), XDG_CONFIG_HOME=str(scratch / "config"),
            CMUX_CUSTOM_CLAUDE_PATH=str(stub), BIND_ADDR=f"127.0.0.1:{port}",
            DATABASE_URL=f"sqlite://{scratch}/central.sqlite", AUTH_ENABLED="false",
            SHUTDOWN_DRAIN_SECS="0", LIVE_REAP_INTERVAL_SECS="0")
        self.server = subprocess.Popen([str(cli.with_name("conversations-server"))],
            env=self.environment, stdout=log, stderr=log)
        self.host = None
        try:
            fixture.ManagedHost.wait(lambda: self.http("GET", "/healthz"), "central server")
            self.host = fixture.ManagedHost(cli, self.origin, scratch, self.environment, log)
        except BaseException:
            self.close()
            raise

    def http(self, method, path, body=None):
        request = urllib.request.Request(self.origin + path,
            data=None if body is None else json.dumps(body).encode(),
            headers={"Content-Type": "application/json"}, method=method)
        with urllib.request.urlopen(request, timeout=5) as response:
            return json.load(response)

    def state(self):
        with sqlite3.connect(self.host.runtime / "authz.db") as db:
            return tuple(tuple(db.execute(f"SELECT * FROM {table}"))
                for table in ("agent_inbox", "agent_sends", "agent_handoffs"))

    def close(self):
        if self.host:
            self.host.close()
        if self.server.poll() is None:
            self.server.terminate()
            self.server.wait(timeout=10)


def main():
    cli = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")) / "debug/arrakis-conversations"
    (ROOT / "tmp").mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="codex-mcp-", dir=ROOT / "tmp") as folder:
        scratch = Path(folder)
        with (scratch / "fixture.log").open("w+") as log:
            test = LocalFixture(cli, scratch, log)
            clients = []
            try:
                source, native, other, claude = (str(uuid.uuid4()) for _ in range(4))
                test.host.register(source, native=native)
                test.host.register(other, agent="claude")
                test.host.register(claude, agent="claude")
                test.host.approve_messages([source, other])
                for session, agent in [(source, "codex"), (other, "claude"), (claude, "claude")]:
                    # The central live-mirror registration routes were removed:
                    # a session's central row is seeded as a stored upload (the
                    # local host remains the identity authority).
                    test.http("POST", "/api/sessions", {
                        "uploaded_by": "mcp-identity-fixture",
                        "session": {
                            "id": session, "agent": agent, "title": "Identity fixture",
                            "project_path": None, "cwd": None, "git_branch": None,
                            "claude_version": None, "started_at": None, "ended_at": None,
                            "messages": [], "skipped_lines": 0,
                        },
                    })
                # There is no central agent-session-id any more: only the local
                # host's binding maps a session to its native thread.
                client = fixture.Mcp(cli, test.origin, source, log, env=test.host.environment)
                clients.append(client)
                def call(name, arguments, context):
                    params = {"name": name, "arguments": arguments}
                    if context is not None:
                        params["_meta"] = {"x-codex-turn-metadata": context}
                    return client.request("tools/call", params)["result"]
                outgoing = {"recipient_session_id": other, "text": "hello", "idempotency_key": "send"}
                tools = [("sessions_list", {}), ("messages_send", outgoing),
                    ("messages_receive", {"acknowledge": []}),
                    ("messages_status", {"message_id": "pending"}),
                    ("sessions_end", {"session_id": other}),
                    ("sessions_handoff", {"brief": "review", "idempotency_key": "handoff", "agent": "claude", "model": "sonnet"})]
                before = test.state()
                for name, arguments in tools:
                    result = call(name, arguments, {"thread_id": other, "parent_thread_id": native})
                    assert result.get("isError"), f"{name} impersonated source: {result}"
                for bad in [None, {}, {"thread_id": ""}, {"thread_id": 123}, "invalid-json", {"thread_id": other, "session_id": native}]:
                    assert call("sessions_handoff", tools[-1][1], bad).get("isError"), bad
                assert test.state() == before, "rejected identities changed local messages or handoffs"
                sent = call("messages_send", outgoing, {"thread_id": native})
                assert not sent.get("isError"), sent
                message_id = json.loads(sent["content"][0]["text"])["id"]
                tools[3] = ("messages_status", {"message_id": message_id})
                for name, arguments in tools:
                    result = call(name, arguments, {"thread_id": native})
                    if name == "sessions_end":
                        assert result.get("isError"), "an unrelated session is not a handoff child"
                        continue
                    assert not result.get("isError"), result
                    if name == "sessions_handoff":
                        handoff = json.loads(result["content"][0]["text"])
                        assert handoff["model"] == "sonnet", handoff
                ended = call("sessions_end", {"session_id": handoff["session_id"]}, {"thread_id": native})
                assert not ended.get("isError"), ended
                ending = json.loads(ended["content"][0]["text"])
                assert ending["status"] in ("ending", "starting", "not_running"), ending
                assert isinstance(ending["host_notified"], bool), ending
                before = test.state()
                with sqlite3.connect(test.host.runtime / "authz.db") as db:
                    db.execute("UPDATE native_bindings SET native=? WHERE session_id=?", (other, source))
                assert call("messages_send", outgoing, {"thread_id": native}).get("isError")
                with sqlite3.connect(test.host.runtime / "authz.db") as db:
                    db.execute("DELETE FROM native_bindings WHERE session_id=?", (source,))
                assert call("messages_send", outgoing, {"thread_id": native}).get("isError")
                assert test.state() == before, "stale binding reached a local effect"
                fixed = fixture.Mcp(cli, test.origin, claude, log, env=test.host.environment)
                clients.append(fixed)
                wrong = fixed.request("tools/call", {"name": "sessions_list", "arguments": {},
                    "_meta": {"x-codex-turn-metadata": {"thread_id": native}}})["result"]
                assert wrong.get("isError"), wrong
                fixed.tool("sessions_list")
                print("PASS: all six tools reject foreign/missing/malformed Codex identity; local matching calls, model handoff, child-only end, stale binding and Claude identity checked")
            finally:
                for client in clients:
                    client.close()
                test.close()



if __name__ == "__main__":
    main()
