#!/usr/bin/env python3
"""Opt-in native OpenCode2/ADK delivery proof, using only disposable sessions.

Requires a built Conversations CLI/server and the personal delivery plugin installed.
Uses the existing authenticated OpenCode2 server but never changes its config or
permissions. All Conversations fixtures live in an isolated local SQLite server.
The model runs two small tasks; native and local fixture sessions are removed.
Run from a disposable cmux terminal, never from another agent's interactive TUI.
"""
import argparse
import base64
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import shlex
import socket
import sqlite3
import struct
import subprocess
import termios
import threading
import time
import urllib.error
import urllib.request
import urllib.parse
import uuid

ROOT = Path(__file__).resolve().parents[1]


def request(origin, method, path, body=None, authorization=None):
    headers = {"Content-Type": "application/json"}
    if authorization:
        headers["Authorization"] = authorization
    req = urllib.request.Request(origin + path, method=method, headers=headers,
                                 data=json.dumps(body).encode() if body is not None else None)
    for attempt in range(90):
        try:
            with urllib.request.urlopen(req, timeout=15) as response:
                raw = response.read()
                return json.loads(raw) if raw else None
        except urllib.error.URLError as error:
            # EADDRNOTAVAIL is a pre-connect failure, so no request body reached
            # the server. Let unrelated local protocol probes drain TIME_WAIT.
            if getattr(error.reason, "errno", None) != 49 or attempt == 89:
                raise
            time.sleep(1)
    raise AssertionError("unreachable")


def wait_until(check, description, timeout=120):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(.2)
    raise AssertionError(f"Timed out waiting for {description}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--launcher", choices=["raw", "adk"], required=True)
    parser.add_argument("--opencode", type=Path, default=Path.home() / ".opencode/bin/opencode")
    parser.add_argument("--adk", default="adk")
    parser.add_argument("--profile", default="product")
    parser.add_argument("--reply-tools", action="store_true", help="Prove source-bound native MCP reply tools in an owned scratch location (raw only)")
    parser.add_argument("--cwd", type=Path, required=True)
    parser.add_argument("--native-url", default="http://127.0.0.1:4096")
    parser.add_argument("--password-file", type=Path, required=True)
    parser.add_argument("--cli", type=Path, default=ROOT / "target/debug/conversations")
    parser.add_argument("--server", type=Path, default=ROOT / "target/debug/conversations-server")
    args = parser.parse_args()
    assert not args.reply_tools or args.launcher == "raw", "Reply proof uses an isolated raw client location on the same shared OpenCode server"
    assert args.native_url.startswith("http://127.0.0.1:"), "Use the local native server, not a remote host"
    # The credential remains in memory and child environment, never argv or logs.
    password = args.password_file.read_text().strip()
    authorization = "Basic " + base64.b64encode(f"opencode:{password}".encode()).decode()
    native = lambda method, path, body=None: request(args.native_url, method, path, body, authorization)
    info = native("GET", "/api/info")
    assert info["version"].lstrip("v").startswith("2."), info["version"]
    scratch = ROOT / "tmp" / ("opencode-native-" + uuid.uuid4().hex[:10])
    scratch.mkdir(parents=True)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    origin = f"http://127.0.0.1:{port}"
    local = lambda method, path, body=None: request(origin, method, path, body)
    env = {key: value for key, value in os.environ.items() if not key.startswith(("CONVERSATIONS_", "GOOGLE_", "OIDC_"))}
    env.update(BIND_ADDR=f"127.0.0.1:{port}", DATABASE_URL=f"sqlite://{scratch}/inbox.sqlite", AUTH_ENABLED="false", SHUTDOWN_DRAIN_SECS="0", LIVE_REAP_INTERVAL_SECS="0")
    directory = scratch / "workspace" if args.reply_tools else args.cwd.resolve()
    if args.reply_tools: directory.mkdir()
    location_query = "?" + urllib.parse.urlencode({"location[directory]": str(directory)})
    reply_configured = False
    native_id = None
    source = None
    child_pid = None
    master = None
    server = None
    stopped = threading.Event()
    try:
        server_log = (scratch / "server.log").open("wb")
        server = subprocess.Popen([str(args.server.resolve())], env=env, stdout=server_log, stderr=server_log)
        def ready():
            try:
                return local("GET", "/healthz")
            except OSError:
                assert server.poll() is None, "Disposable server exited"
                return False
        wait_until(ready, "disposable server", 15)
        native_id = native("POST", "/api/session", {"title": "Disposable Conversations native delivery proof", "location": {"directory": str(directory)}})["data"]["id"]
        project = local("POST", "/api/projects", {"name": "Disposable native delivery proof"})["id"]
        source, sender = str(uuid.uuid4()), str(uuid.uuid4())
        for sid in [source, sender]:
            local("POST", "/api/live", {"session_id": sid, "title": "Disposable native delivery proof", "agent": "zsh" if args.launcher == "adk" and sid == source else "opencode"})
            local("PUT", f"/api/sessions/{sid}/project", {"project_id": project})
            subprocess.run([str(args.cli.resolve()), "--server", origin, "messaging", "enable", sid], check=True, capture_output=True)
        if args.launcher == "raw":
            local("POST", f"/api/live/{source}/agent-session-id", {"agent_session_id": native_id})
        if args.reply_tools:
            native("PUT", "/api/experimental/mcp/conversations" + location_query, {"config": {
                "type": "local", "command": [str(args.cli.resolve()), "--server", origin,
                    "mcp", "serve", "--session-context", "opencode"], "codemode": False}})
            reply_configured = True
        launch = ([str(args.opencode.resolve()), "--server", args.native_url, "--session", native_id, str(directory)]
                  if args.launcher == "raw" else [args.adk, "code", args.profile, "--", "-s", native_id])
        terminal_env = dict(os.environ, CONVERSATIONS_WRAPPED=source, CONVERSATIONS_SERVER=origin,
                            CONVERSATIONS_CLI=str(args.cli.resolve()), OPENCODE_PASSWORD=password, TERM="xterm-256color", COLUMNS="140", LINES="40")
        wrapped_command = launch
        if args.launcher == "adk":
            # Exercise the actual shell -> adk path. Do not seed its native
            # identity: the integration must learn that from this TUI itself.
            terminal_env.pop("CONVERSATIONS_WRAPPED", None)
            terminal_env.pop("CONVERSATIONS_SESSION_ID", None)
            wrapped_command = [str(args.cli.resolve()), "--server", origin, "host", "--runtime-dir", str(scratch / "runtime"),
                               "wrap", "--session-id", source, "--cwd", str(args.cwd.resolve()), "--agent", "zsh", "--", "/bin/zsh", "-il"]
        child_pid, master = pty.fork()
        if child_pid == 0:
            os.chdir(directory)
            os.execvpe(wrapped_command[0], wrapped_command, terminal_env)
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
        def drain():
            with (scratch / "terminal.log").open("wb") as log:
                while not stopped.is_set():
                    try:
                        if not select.select([master], [], [], .2)[0]:
                            continue
                        data = os.read(master, 65536)
                    except OSError:
                        return
                    if not data:
                        return
                    log.write(data); log.flush()
                    if b"\x1b[6n" in data:
                        os.write(master, b"\x1b[1;1R")
        reader = threading.Thread(target=drain, daemon=True)
        reader.start()
        if args.launcher == "adk":
            time.sleep(3)
            os.write(master, (shlex.join(launch) + "\n").encode())
        time.sleep(8)
        before = native("GET", f"/api/session/{native_id}")["data"]
        if args.launcher == "adk":
            assert args.profile in json.dumps(before.get("metadata", {})), "ADK profile metadata not established"
        policy = {key: before.get(key) for key in ("agent", "permissions")}
        def send(marker):
            return local("POST", f"/api/agent-messaging/sessions/{sender}/messages", {"recipient_session_id": source,
                         "text": f"Reply with exactly {marker}. This is a disposable communication test; do not use tools.",
                         "idempotency_key": marker})
        def accepted(message):
            return local("GET", f"/api/agent-messaging/sessions/{sender}/messages/{message['id']}").get("acknowledged_at")
        def exported():
            return native("GET", f"/api/experimental/session/{native_id}/export")["data"]
        def assistant_completed(marker):
            return any(message.get("type") == "assistant" and message.get("time", {}).get("completed")
                       and any(part.get("type") == "text" and marker in part.get("text", "")
                               for part in message.get("content", [])) for message in exported()["messages"])
        idle_marker = "IDLE_" + uuid.uuid4().hex[:10]
        if args.reply_tools:
            reply_marker = "REPLY_" + uuid.uuid4().hex[:10]
            incoming = local("POST", f"/api/agent-messaging/sessions/{sender}/messages", {
                "recipient_session_id": source, "idempotency_key": reply_marker,
                "text": f"Authorized disposable communication test: use the Conversations MCP sessions_list tool, then messages_send to recipient_session_id {sender}, text exactly {reply_marker}, idempotency_key {reply_marker}. Then answer exactly {idle_marker}. Do not run commands or edit files."})
            approved = []
            def reply_received():
                requests = native("GET", f"/api/session/{native_id}/permission")["data"]
                for permission in requests:
                    assert permission["sessionID"] == native_id
                    assert permission["action"] in {"conversations_sessions_list", "conversations_messages_send"}, permission["action"]
                    native("POST", f"/api/session/{native_id}/permission/{permission['id']}/reply", {"decision":"once"})
                    approved.append(permission["action"])
                messages = local("POST", f"/api/agent-messaging/sessions/{sender}/inbox", {"wait_seconds":0})["messages"]
                return next((message for message in messages if message["text"] == reply_marker), None)
            reply = wait_until(reply_received, "actual model's source-bound Conversations MCP reply", 120)
            assert reply["sender_session_id"] == source and reply["recipient_session_id"] == sender
            wait_until(lambda: assistant_completed(idle_marker), "completed native reply assistant marker")
            snapshot = exported()
            assert any(message.get("type") == "assistant" and any(part.get("type") == "text" and idle_marker in part.get("text", "") for part in message.get("content", [])) for message in snapshot["messages"])
            after = native("GET", f"/api/session/{native_id}")["data"]
            assert policy == {key: after.get(key) for key in policy}
            evidence = {"native_session_id":native_id,"source":source,"peer":sender,"incoming":incoming,"reply":reply,
                        "approved_once":approved,"policy_unchanged":True,"actual_native_mcp_reply":True}
            (scratch / "native-export.json").write_text(json.dumps(snapshot, indent=2)+"\n")
            (scratch / "result.json").write_text(json.dumps(evidence, indent=2)+"\n")
            print(f"PASS actual shared OpenCode native MCP sessions_list and messages_send reply as exact source; {scratch}")
            return
        idle = send(idle_marker)
        wait_until(lambda: accepted(idle), "idle automatic native admission")
        wait_until(lambda: assistant_completed(idle_marker), "completed native idle assistant marker")
        snapshot = exported()
        assert any(m.get("type") == "assistant" and any(p.get("type") == "text" and idle_marker in p.get("text", "") for p in m.get("content", [])) for m in snapshot["messages"]), "Native transcript has no assistant response to idle delivery"
        busy_done = "WORK_DONE_" + uuid.uuid4().hex[:10]
        busy_admission = native("POST", f"/api/session/{native_id}/prompt", {
            "text": f"Write 60 numbered short lines about desert sand. Do not use tools. Continue to line 60, then end with {busy_done}.",
            "delivery": "queue", "resume": True})["data"]
        wait_until(lambda: native_id in native("GET", "/api/session/active")["data"], "real active model execution", 20)
        busy_marker = "BUSY_" + uuid.uuid4().hex[:10]
        busy = send(busy_marker)
        wait_until(lambda: accepted(busy), "busy automatic native queue admission")
        wait_until(lambda: assistant_completed(busy_marker), "completed native queued assistant marker", 180)
        snapshot = exported()
        assert any(m.get("type") == "assistant" and any(p.get("type") == "text" and busy_marker in p.get("text", "") for p in m.get("content", [])) for m in snapshot["messages"]), "Native transcript has no assistant response to queued delivery"
        messages = snapshot["messages"]
        work_index = next(i for i, message in enumerate(messages) if message["id"] == busy_admission["id"])
        peer_index = next(i for i, message in enumerate(messages) if message["id"] == "msg_conversations_" + busy["id"])
        assert peer_index > work_index, "Peer input overtook the active user prompt"
        work_output = messages[work_index + 1:peer_index]
        assert any(message.get("type") == "assistant" and message.get("time", {}).get("completed")
                   and any(part.get("type") == "text" and busy_done in part.get("text", "")
                           for part in message.get("content", [])) for message in work_output), \
            "Peer input interrupted or preceded completion of the busy assistant turn"
        # Reconcile the same durable native ID after an ambiguous transport result.
        for message in [idle, busy]:
            native_message = "msg_conversations_" + message["id"]
            original = native("GET", f"/api/session/{native_id}/message/{native_message}")["data"]
            replay = native("POST", f"/api/session/{native_id}/prompt", {"id": native_message, "text": "retry must preserve first admission", "delivery": "queue", "resume": False})["data"]
            assert replay["id"] == native_message and replay["sessionID"] == native_id
            reconciled = native("GET", f"/api/session/{native_id}/message/{native_message}")["data"]
            assert reconciled == original, "Retry changed the originally admitted native message"
            current = exported()
            assert sum(m["id"] == native_message for m in current["messages"]) == 1, "Duplicate native transcript message"
        after = native("GET", f"/api/session/{native_id}")["data"]
        assert policy == {key: after.get(key) for key in policy}, "Delivery changed agent or permission policy"
        source_metadata = local("GET", f"/api/agent-messaging/sessions/{source}/identity")
        assert source_metadata["agent_session_id"] == native_id, "Source published a different native identity"
        evidence = {"launcher": args.launcher, "native_session_id": native_id, "native_version": info["version"],
                    "message_ids": [idle["id"], busy["id"]],
                    "message_statuses": [local("GET", f"/api/agent-messaging/sessions/{sender}/messages/{m['id']}") for m in [idle,busy]],
                    "source_metadata": source_metadata, "policy_unchanged": True,
                    "wrapped_shell": args.launcher == "adk",
                    "idle_and_busy_automatic_admission": True, "assistant_responses": True,
                    "busy_completed_before_peer_input": True,
                    "native_duplicate_reconciliation": True,
                    "assistant_text": [p["text"] for m in snapshot["messages"] if m.get("type") == "assistant"
                                       for p in m.get("content", []) if p.get("type") == "text" and (idle_marker in p.get("text", "") or busy_marker in p.get("text", ""))]}
        (scratch / "native-export.json").write_text(json.dumps(snapshot, indent=2) + "\n")
        (scratch / "result.json").write_text(json.dumps(evidence, indent=2) + "\n")
        print(f"PASS: native {args.launcher} idle/busy delivery, assistant replies, durable retry, unchanged policy; {scratch}")
    finally:
        stopped.set()
        if child_pid:
            for signum in (signal.SIGTERM, signal.SIGKILL):
                try:
                    os.killpg(child_pid, signum)
                except PermissionError:
                    # Some terminal launchers move the owned wrapper out of its
                    # original group. Fall back to its exact unreaped child PID.
                    try:
                        os.kill(child_pid, signum)
                    except ProcessLookupError:
                        pass
                except ProcessLookupError:
                    pass
                deadline = time.monotonic() + 3
                while time.monotonic() < deadline:
                    if os.waitpid(child_pid, os.WNOHANG)[0]:
                        child_pid = None
                        break
                    time.sleep(.1)
                if child_pid is None:
                    break
        if master is not None:
            os.close(master)
        if reply_configured:
            try:
                native("DELETE", "/api/experimental/mcp/conversations" + location_query)
            except Exception as error:
                print(f"Disposable MCP location cleanup required: {directory}: {type(error).__name__}")
        if native_id:
            try:
                native("POST", f"/api/session/{native_id}/interrupt", {})
            except Exception:
                pass
            try:
                native("DELETE", f"/api/session/{native_id}")
            except Exception as error:
                print(f"Disposable native session cleanup required: {native_id}: {type(error).__name__}")
        if source:
            registry = Path.home() / "Library/Application Support/arrakis-conversations/opencode-context/bindings.sqlite"
            if registry.exists():
                with sqlite3.connect(registry) as db:
                    db.execute("DELETE FROM bindings WHERE source=?", (source,))
        if server:
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill(); server.wait()


if __name__ == "__main__":
    main()
