#!/usr/bin/env python3
"""Exercise real MCP stdio clients against a disposable Conversations server.

Build first: cargo build -p conversations-cli -p conversations-server
Authentication and separate human identities are covered by tests/agent_messaging.rs.
This process test uses auth-off local mode, task-owned signing keys, and inert
wrapper registrations. It never runs coding agents or changes saved credentials.
"""

import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import select
import socket
import sqlite3
import struct
import subprocess
import tempfile
import time
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]


class Mcp:
    def __init__(self, cli, origin, session, log, cookie_file=None, native=None, env=None):
        self.native = native
        self.child = subprocess.Popen(
            [str(cli), "--server", origin, "mcp", "serve"]
            + (["--session", session] if session else [])
            + (["--cookie-file", str(cookie_file)] if cookie_file else []),
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True, bufsize=1, env=env,
        )
        self.sequence = 0
        initialized = self.request("initialize", {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "messaging-process-test", "version": "1"},
        })
        assert "tools" in initialized["result"]["capabilities"], initialized
        self.write({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def write(self, message):
        self.child.stdin.write(json.dumps(message) + "\n")
        self.child.stdin.flush()

    def request(self, method, params):
        self.sequence += 1
        self.write({"jsonrpc": "2.0", "id": self.sequence, "method": method, "params": params})
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if not select.select([self.child.stdout], [], [], max(0, deadline - time.monotonic()))[0]:
                break
            line = self.child.stdout.readline()
            assert line, f"MCP process exited: {self.child.poll()}"
            message = json.loads(line)
            assert message["jsonrpc"] == "2.0", message
            if message.get("id") == self.sequence:
                return message
        raise AssertionError(f"MCP request timed out: {method}")

    def tool(self, name, arguments=None, error=False):
        params = {"name": name, "arguments": arguments or {}}
        if self.native is not None:
            params["_meta"] = {"x-codex-turn-metadata": {"thread_id": self.native}}
        response = self.request("tools/call", params)
        assert "error" not in response, response
        result = response["result"]
        assert bool(result.get("isError", False)) == error, result
        if error:
            return result
        return json.loads(result["content"][0]["text"])

    def close(self):
        self.child.stdin.close()
        try:
            self.child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.child.kill()
            self.child.wait()



class ManagedHost:
    """Real host/relay authority with inert local wrapper registrations.

    No model or terminal application runs. Only the native identity is fixture
    data; keys, pairing, signatures, admission and inbox delivery use production
    code. The owner-only registration sockets stay connected until close().
    """

    def __init__(self, cli, origin, scratch, environment, log):
        self.cli, self.origin = cli, origin
        self.runtime = Path(scratch) / "host"
        self.runtime.mkdir(mode=0o700)
        self.environment = dict(environment, CONVERSATIONS_HOST_DIR=str(self.runtime))
        self.registrations = {}
        self.child = subprocess.Popen([str(cli), "--server", origin, "host", "--backend", "normal"],
            env=self.environment, stdout=log, stderr=log, cwd=scratch)
        try:
            self.wait(lambda: (self.runtime / "host.sock").exists(), "host socket")
            self.host_id = self.credential("identity").strip()
            self.wait_online()
        except Exception:
            self.close()
            raise

    @staticmethod
    def wait(check, description):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            try:
                result = check()
                if result:
                    return result
            except (OSError, ValueError, subprocess.CalledProcessError):
                pass
            time.sleep(.05)
        raise AssertionError(f"timed out waiting for {description}")

    def wait_online(self):
        def online():
            with urllib.request.urlopen(f"{self.origin}/api/host-relay/{self.host_id}/status", timeout=2) as reply:
                return json.load(reply)["online"]
        self.wait(online, "signed host relay")

    def credential(self, *args):
        return subprocess.check_output([str(self.cli), "credential", *args],
            env=self.environment, text=True, stderr=subprocess.PIPE, timeout=10)

    def status(self):
        return json.loads(subprocess.check_output([str(self.cli), "host", "status", "--json"],
            env=self.environment, text=True, stderr=subprocess.PIPE, timeout=5))["sessions"]

    def register(self, session, agent="codex", native=None):
        if session in self.registrations:
            self.registrations.pop(session).close()
            self.wait(lambda: all(s["launch"]["id"] != session for s in self.status()), "old registration removal")
        sock = socket.socket(socket.AF_UNIX)
        sock.connect(str(self.runtime / "host.sock"))
        message = json.dumps({"type": "register", "registration": {
            "launch": {"id": session, "agent": agent, "cwd": str(self.runtime.parent), "command": ["/bin/sleep", "600"]},
            "workspace": None, "surface": None, "local_terminal": True,
            "effect_gate": True}}).encode()
        sock.sendall(struct.pack(">I", len(message)) + message)
        self.registrations[session] = sock
        self.wait(lambda: any(s["launch"]["id"] == session for s in self.status()), "wrapper registration")
        if native is not None:
            with sqlite3.connect(self.runtime / "authz.db") as db:
                db.execute("INSERT OR REPLACE INTO native_bindings(session_id,native,bound_at) VALUES (?,?,?)",
                    (session, native, int(time.time())))

    def approve_messages(self, sessions):
        descriptor = self.credential("descriptor").strip()
        public = self.credential("agent-key").splitlines()[0].split(": ", 1)[1]
        grants = [arg for session in sessions for arg in ("--grant", f"session.write:{session}")]
        approved = self.credential("add", "--kind", "agent", "--public-key", public,
            "--label", "MCP process fixture", *grants)
        for session in sessions:
            self.credential("pair-peer", "--host", descriptor, "--session", session, "--label", "MCP peer")
        return approved.splitlines()[0].split()[-1]

    def close(self):
        for sock in self.registrations.values():
            sock.close()
        self.child.terminate()
        try:
            self.child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.child.kill()
            self.child.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target")
    args = parser.parse_args()
    cli = args.target_dir.resolve() / "debug/conversations"
    server = args.target_dir.resolve() / "debug/conversations-server"
    (ROOT / "tmp").mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="messaging-mcp-", dir=ROOT / "tmp") as folder:
        scratch = Path(folder)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        origin = f"http://127.0.0.1:{port}"
        environment = {k: v for k, v in os.environ.items()
            if k in {"PATH", "TMPDIR", "LANG", "LC_ALL", "TERM", "SSL_CERT_FILE", "NIX_SSL_CERT_FILE"}}
        environment.update(HOME=str(scratch), XDG_CONFIG_HOME=str(scratch / "config"), BIND_ADDR=f"127.0.0.1:{port}", DATABASE_URL=f"sqlite://{scratch}/inbox.sqlite",
                           AUTH_ENABLED="false", SHUTDOWN_DRAIN_SECS="0", LIVE_REAP_INTERVAL_SECS="0")
        clients = []
        child = None
        host = None
        with (scratch / "server.log").open("w+") as log, (scratch / "mcp.log").open("w+") as mcp_log:
            def http(method, path, body=None):
                request = urllib.request.Request(origin + path, method=method,
                    data=None if body is None else json.dumps(body).encode(), headers={"Content-Type": "application/json"})
                with urllib.request.urlopen(request, timeout=5) as response:
                    return json.load(response)

            def start():
                process = subprocess.Popen([str(server)], env=environment, stdout=log, stderr=log)
                for _ in range(100):
                    if process.poll() is not None:
                        raise AssertionError("server exited before readiness")
                    try:
                        http("GET", "/healthz")
                        return process
                    except OSError:
                        time.sleep(.05)
                process.kill()
                process.wait()
                raise AssertionError("server did not become ready")

            try:
                child = start()
                host = ManagedHost(cli, origin, scratch, environment, mcp_log)
                sessions = {name: str(uuid.uuid4()) for name in ["alice", "bob", "eve"]}
                project = http("POST", "/api/projects", {"name": "collaboration"})["id"]
                other = http("POST", "/api/projects", {"name": "unrelated"})["id"]
                natives = {name: str(uuid.uuid4()) for name in sessions}
                for session, group in [("alice", project), ("bob", project), ("eve", other)]:
                    name, session = session, sessions[session]
                    host.register(session, native=natives[name])
                    # The central live-mirror registration routes were removed:
                    # a session's central row is seeded as a stored upload (the
                    # local host remains the messaging/identity authority).
                    http("POST", "/api/sessions", {
                        "uploaded_by": "mcp-fixture",
                        "session": {
                            "id": session, "agent": "codex", "title": f"Task for {session}",
                            "project_path": None, "cwd": None, "git_branch": None,
                            "claude_version": None, "started_at": None, "ended_at": None,
                            "messages": [], "skipped_lines": 0,
                        },
                    })
                    # An unconfigured session must expose the setup/tool surface without
                    # consenting to discovery or messaging during MCP initialization.
                    if name == "alice":
                        unconfigured = Mcp(cli, origin, session, mcp_log, native=natives[name], env=host.environment)
                        try:
                            catalog = unconfigured.request("tools/list", {})["result"]["tools"]
                            assert len(catalog) == 6
                            assert {tool["name"] for tool in catalog if tool.get("annotations", {}).get("readOnlyHint")} == {"sessions_list", "messages_status"}
                            assert unconfigured.tool("sessions_list")["sessions"] == []
                        finally:
                            unconfigured.close()
                    http("PUT", f"/api/sessions/{session}/project", {"project_id": group})
                credential = host.approve_messages([sessions["alice"], sessions["bob"]])
                alice = Mcp(cli, origin, sessions["alice"], mcp_log, native=natives["alice"], env=host.environment)
                clients.append(alice)
                bob = Mcp(cli, origin, sessions["bob"], mcp_log, native=natives["bob"], env=host.environment)
                clients.append(bob)
                listed = alice.request("tools/list", {})
                assert {tool["name"] for tool in listed["result"]["tools"]} == {
                    "sessions_list", "messages_send", "messages_receive", "messages_status", "sessions_handoff", "sessions_end"}
                peers = alice.tool("sessions_list")["sessions"]
                assert peers == [], peers  # Auth-off rows have no person identity for discovery.
                outgoing = {"recipient_session_id": sessions["bob"], "text": "Check the API contract", "idempotency_key": "question-1"}

                first = alice.tool("messages_send", outgoing)
                assert first["status"] == "queued"
                assert alice.tool("messages_send", outgoing)["id"] == first["id"]
                alice.tool("messages_send", dict(outgoing, recipient_session_id=sessions["eve"], idempotency_key="unpaired-peer"), error=True)
                forged = alice.request("tools/call", {
                    "name": "messages_send",
                    "arguments": dict(outgoing, sender_session_id=sessions["bob"]),
                    "_meta": {"x-codex-turn-metadata": {"thread_id": natives["alice"]}},
                })
                assert "error" in forged or forged["result"].get("isError"), forged
                assert "unknown field `sender_session_id`" in json.dumps(forged), forged
                # Inbox durability is local. Restart the opaque relay before receipt;
                # the host must reconnect without losing or duplicating the message.
                child.terminate()
                child.wait(timeout=10)
                child = start()
                host.wait_online()
                incoming = bob.tool("messages_receive")["messages"]
                assert len(incoming) == 1 and incoming[0]["id"] == first["id"]
                assert bob.tool("messages_receive")["messages"][0]["id"] == first["id"]
                assert alice.tool("messages_status", {"message_id": first["id"]})["status"] == "received"
                reply = bob.tool("messages_send", {"recipient_session_id": sessions["alice"], "text": "Contract checked",
                    "idempotency_key": "reply-1", "reply_to": first["id"]})
                assert alice.tool("messages_status", {"message_id": first["id"]})["status"] == "acknowledged"
                assert bob.tool("messages_receive")["messages"] == []
                assert alice.tool("messages_receive", {"acknowledge": [reply["id"]]})["messages"] == []
                # An outstanding bounded receive observes messages sent by another client.
                with concurrent.futures.ThreadPoolExecutor() as executor:
                    waiting = executor.submit(bob.tool, "messages_receive", {"wait_seconds": 3})
                    time.sleep(.2)
                    second = alice.tool("messages_send", dict(outgoing, idempotency_key="question-2"))
                    assert waiting.result(timeout=8)["messages"][0]["id"] == second["id"]
                http("PUT", f"/api/sessions/{sessions['bob']}/project", {"project_id": other})
                assert alice.tool("sessions_list")["sessions"] == []
                # Discovery membership is not authority: local grants still permit a send.
                alice.tool("messages_send", dict(outgoing, idempotency_key="project-moved"))
                host.credential("revoke", "--json", "--", credential)
                alice.tool("messages_send", dict(outgoing, idempotency_key="revoked"), error=True)
                assert bob.tool("messages_receive")["messages"] == []
            except Exception:
                log.flush()
                mcp_log.flush()
                print((scratch / "mcp.log").read_text()[-8000:])
                raise
            finally:
                for client in clients:
                    client.close()
                if host is not None:
                    host.close()
                if child is not None and child.poll() is None:
                    child.terminate()
                    child.wait(timeout=10)
    print("PASS: MCP catalog, ownerless discovery, signed send/retry, local pairing, relay restart, replies, acknowledgment, bounded wait, local revocation")


if __name__ == "__main__":
    main()
