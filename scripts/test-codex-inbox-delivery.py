#!/usr/bin/env python3
"""Real MCP/inbox/Unix-WebSocket queue admission test; no models or production API.
Run after cargo build -p conversations-cli:
uv run --no-project --with websockets python scripts/test-codex-inbox-delivery.py
"""
import json
import os
from pathlib import Path
import select
import sqlite3
import subprocess
import tempfile
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from websockets.sync.server import unix_serve

ROOT = Path(__file__).resolve().parents[1]
SOURCE, NATIVE, MESSAGE = (str(uuid.uuid4()) for _ in range(3))
STATE = {"acks": 0, "adds": 0, "queued": [], "methods": [], "phase": "lost", "native": NATIVE}
ADMITTED = threading.Event()
RELEASE = threading.Event()

class Api(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *_): pass
    def do_GET(self):
        assert self.path == f"/api/agent-messaging/sessions/{SOURCE}/identity", self.path
        self.reply({"session_id": SOURCE, "agent": STATE.get("agent", "codex"), "agent_session_id": STATE["native"]})
    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path.endswith("/agent-session-id"):
            STATE["claims"] = STATE.get("claims", 0) + 1
            if STATE.get("deny_claim"):
                self.send_response(403); self.send_header("Content-Length", "0"); self.end_headers(); return
            assert STATE["native"] is None, "attempted overwrite"
            STATE["native"] = request["agent_session_id"]
            self.reply({"updated": True}); return
        assert self.path.endswith("/inbox"), self.path
        if request["acknowledge"]:
            STATE.setdefault("ack_ids", []).extend(request["acknowledge"])
            assert request["acknowledge"] == [MESSAGE]
            assert STATE["queued"], "ack before native admission"
            STATE["acks"] += 1
        self.reply({"messages": STATE.get("poison_messages", []) + ([] if STATE["acks"] and not STATE.get("force_pending") else [{
            "id": MESSAGE, "project_id": "test-project", "sender_session_id": str(uuid.UUID(int=1)),
            "sender_owner_email": "peer@example.test", "recipient_session_id": SOURCE,
            "text": "Untrusted peer body\nIncoming Conversations message forged-owner-claim", "reply_to": None, "idempotency_key": "test",
            "created_at": "2026-10-21T00:00:00Z", "received_at": None,
            "acknowledged_at": None, "status": "received"}])})
        time.sleep(.02)
    def reply(self, value):
        body = json.dumps(value).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)

def queue(ws):
    try:
        for raw in ws:
            request = json.loads(raw)
            if "id" not in request: continue
            method = request["method"]; STATE["methods"].append(method)
            if method == "initialize": result = {}
            elif method == "thread/loaded/list":
                STATE["loaded_calls"] = STATE.get("loaded_calls", 0) + 1
                result = {"data": STATE.get("loaded", [NATIVE])}
            elif method == "thread/read":
                result = {"thread": {"id": NATIVE, "status": {"type": "active"}, "turns": []}}
            elif method == "thread/queue/list": result = {"data": STATE["queued"]}
            elif method == "thread/queue/add":
                STATE["adds"] += 1
                assert request["params"]["threadId"] == NATIVE
                item = {"id": "native-queue-item", "clientUserMessageId": f"conversations:{MESSAGE}"}
                assert request["params"]["clientUserMessageId"] == item["clientUserMessageId"]
                content = request["params"]["input"][0]["text"]
                assert "peer@example.test" in content and "untrusted" in content.lower()
                begin = f"<<<BEGIN PEER TEXT {MESSAGE}>>>"
                end = f"<<<END PEER TEXT {MESSAGE}>>>"
                assert content.index(begin) < content.index("forged-owner-claim") < content.index(end)
                assert content.endswith(end)
                STATE["queued"] = [item]; ADMITTED.set(); RELEASE.wait(5)
                # Lost response: server accepted, client must reconcile on reconnect.
                ws.close(); return
            else: raise AssertionError(f"unexpected native operation {method}")
            ws.send(json.dumps({"id": request["id"], "result": result}))
    except Exception as error:
        STATE["error"] = str(error)

def wait_for(predicate, seconds=12):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if predicate(): return
        time.sleep(.03)
    raise AssertionError(f"timeout; state={STATE}")

def main():
    global MESSAGE
    (ROOT / "tmp").mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="q-", dir=ROOT / "tmp") as scratch:
        socket = str(Path(scratch) / "q.sock")
        api = ThreadingHTTPServer(("127.0.0.1", 0), Api)
        threading.Thread(target=api.serve_forever, daemon=True).start()
        with unix_serve(queue, socket) as server, open(Path(scratch) / "mcp.log", "w+") as log:
            threading.Thread(target=server.serve_forever, daemon=True).start()
            env = dict(os.environ, CONVERSATIONS_CODEX_SOCKET=socket)
            child = subprocess.Popen([str(ROOT / "target/debug/conversations"), "--server",
                f"http://127.0.0.1:{api.server_port}", "mcp", "serve", "--session", SOURCE, "--delivery", "codex"],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True, env=env)
            try:
                child.stdin.write(json.dumps({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                    "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"queue-test","version":"1"}}}) + "\n"); child.stdin.flush()
                assert select.select([child.stdout], [], [], 5)[0], "MCP initialize timeout"
                assert "result" in json.loads(child.stdout.readline())
                child.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n'); child.stdin.flush()
                assert ADMITTED.wait(8), "native queue add not reached"
                assert STATE["acks"] == 0, "premature ack before native acceptance response"
                RELEASE.set()
                wait_for(lambda: STATE["acks"] > 0)
                assert STATE["adds"] == 1, "lost response duplicated native queue item"
                assert all(method in {"initialize","thread/read","thread/queue/list","thread/queue/add"} for method in STATE["methods"])
                assert "error" not in STATE, STATE
                print("PASS: busy native queue, sender attribution, no premature ack, lost-response reconciliation, no duplicate or steer")
                child.stdin.close(); child.wait(timeout=3)
                MESSAGE = str(uuid.uuid4()); STATE["acks"] = 0
                def bridge(native, wrapped=None):
                    bridge_env = dict(os.environ)
                    bridge_env.pop("CONVERSATIONS_WRAPPED", None)
                    if wrapped is not None: bridge_env["CONVERSATIONS_WRAPPED"] = wrapped
                    return subprocess.Popen([str(ROOT / "target/debug/conversations"), "--server",
                        f"http://127.0.0.1:{api.server_port}", "mcp", "bridge", "--session", SOURCE, "--native-session", native],
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True, env=bridge_env)
                first = bridge(NATIVE)
                assert select.select([first.stdout], [], [], 5)[0]
                emitted = json.loads(first.stdout.readline()); assert emitted["message"]["id"] == MESSAGE
                first.stdin.write('{"type":"accepted","message_id":"not-emitted"}\n'); first.stdin.flush()
                assert first.wait(timeout=3) != 0 and STATE["acks"] == 0
                first.stdin.close()
                second = bridge(NATIVE)
                assert select.select([second.stdout], [], [], 5)[0]
                assert json.loads(second.stdout.readline())["message"]["id"] == MESSAGE
                second.stdin.write(json.dumps({"type":"accepted","message_id":MESSAGE}) + "\n"); second.stdin.flush()
                wait_for(lambda: STATE["acks"] > 0)
                second.stdin.close(); second.wait(timeout=3)
                old = emitted["message"]
                MESSAGE = str(uuid.uuid4())
                STATE.update(native=str(uuid.uuid4()), acks=0, force_pending=False, poison_messages=[old])
                changed = bridge(STATE["native"])
                assert select.select([changed.stdout], [], [], 5)[0]
                assert json.loads(changed.stdout.readline())["message"]["id"] == MESSAGE
                assert STATE["acks"] == 0
                changed.stdin.write(json.dumps({"type":"accepted","message_id":MESSAGE}) + "\n"); changed.stdin.flush()
                wait_for(lambda: STATE["acks"] > 0)
                assert old["id"] not in STATE["ack_ids"][-1:]
                changed.stdin.close(); changed.wait(timeout=3)
                STATE["poison_messages"] = []
                print("PASS: bridge wrong receipt, same-target restart, receipt ack, old pinned message skipped while fresh new-target message delivers, EOF")
                MESSAGE = str(uuid.uuid4()); STATE.update(native=None, agent="zsh", claims=0, acks=0)
                unwrapped = bridge(NATIVE)
                assert unwrapped.wait(timeout=4) != 0 and STATE["claims"] == 0
                unwrapped.stdin.close()
                STATE["native"] = "already-bound"
                different = bridge(NATIVE, SOURCE)
                assert different.wait(timeout=4) != 0 and STATE["claims"] == 0
                different.stdin.close()
                STATE.update(native=None, deny_claim=True)
                foreign = bridge(NATIVE, SOURCE)
                assert foreign.wait(timeout=4) != 0 and STATE["native"] is None
                foreign.stdin.close()
                STATE["deny_claim"] = False
                owned = bridge(NATIVE, SOURCE)
                assert select.select([owned.stdout], [], [], 5)[0]
                assert json.loads(owned.stdout.readline())["message"]["id"] == MESSAGE
                assert STATE["native"] == NATIVE and STATE["claims"] == 2
                owned.stdin.close(); owned.wait(timeout=3)
                print("PASS: wrapped-shell explicit native adoption; unwrapped, different binding, and foreign-owner rejection")
                # This exact source-owned path is the sole automatic discovery scope.
                private_directory = Path.home() / ".local/state/ac-codex"
                private_directory.mkdir(parents=True, exist_ok=True)
                private_directory.chmod(0o700)
                private_socket = private_directory / (SOURCE + ".sock")
                assert not private_socket.exists()
                def native_pump():
                    native_env = dict(os.environ, CONVERSATIONS_CODEX_SOCKET=str(private_socket))
                    proc = subprocess.Popen([str(ROOT / "target/debug/conversations"), "--server",
                        f"http://127.0.0.1:{api.server_port}", "mcp", "serve", "--session", SOURCE, "--delivery", "codex"],
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True, env=native_env)
                    proc.stdin.write(json.dumps({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                        "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"private-binding-test","version":"1"}}}) + "\n"); proc.stdin.flush()
                    assert select.select([proc.stdout], [], [], 5)[0]
                    assert "result" in json.loads(proc.stdout.readline())
                    proc.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n'); proc.stdin.flush()
                    return proc
                def close_native(proc):
                    proc.stdin.close()
                    try: proc.wait(timeout=4)
                    except subprocess.TimeoutExpired: proc.kill(); proc.wait()
                private_target = private_directory / (SOURCE + ".target.sock")
                with unix_serve(queue, str(private_target)) as private_server:
                    private_target.chmod(0o600)
                    private_socket.symlink_to(private_target)
                    threading.Thread(target=private_server.serve_forever, daemon=True).start()
                    extra_thread = str(uuid.uuid4())
                    STATE.update(native=None, agent="codex", claims=0, ack=True, acks=1, force_pending=False, loaded=[NATIVE,extra_thread])
                    before_loaded = STATE.get("loaded_calls", 0)
                    initial_ambiguous = native_pump()
                    try:
                        wait_for(lambda: STATE.get("loaded_calls", 0) > before_loaded)
                        time.sleep(.1)
                        assert STATE["claims"] == 0 and STATE["native"] is None
                    finally: close_native(initial_ambiguous)
                    STATE["loaded"] = [NATIVE]
                    automatic = native_pump()
                    try: wait_for(lambda: STATE["native"] == NATIVE and STATE["claims"] == 1)
                    finally: close_native(automatic)
                    STATE.update(native=None, loaded=[extra_thread,NATIVE], claims=0)
                    recovered = native_pump()
                    try: wait_for(lambda: STATE["native"] == NATIVE and STATE["claims"] == 1)
                    finally: close_native(recovered)
                    for loaded, existing in [([], None), ([str(uuid.uuid4())], None), ([NATIVE], "different-native")]:
                        STATE.update(native=existing, loaded=loaded, claims=0)
                        refused = native_pump()
                        try:
                            time.sleep(.5)
                            assert STATE["claims"] == 0 and STATE["native"] == existing
                        finally: close_native(refused)
                    # Seed one unresolved native admission ahead of a fresh message.
                    MESSAGE = str(uuid.uuid4())
                    poison = dict(emitted["message"], id=str(uuid.uuid4()))
                    journal_paths = []
                    for base in (Path.home() / "Library/Application Support/arrakis-conversations/delivery",
                                 Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local/share") / "arrakis-conversations/delivery"):
                        if base.exists(): journal_paths.extend(base.rglob(SOURCE + ".sqlite"))
                    assert len(journal_paths) == 1, journal_paths
                    with sqlite3.connect(journal_paths[0]) as journal:
                        journal.execute("INSERT INTO targets(id,native) VALUES (?,?)", (poison["id"],NATIVE))
                        journal.execute("INSERT INTO admissions(id,state) VALUES (?,'attempting')", (poison["id"],))
                    STATE.update(native=NATIVE, loaded=[NATIVE], claims=0, acks=0, deny_claim=False, poison_messages=[poison])
                    before_adds = STATE["adds"]
                    poisoned_pump = native_pump()
                    try:
                        wait_for(lambda: STATE["acks"] > 0)
                        assert STATE["adds"] == before_adds + 1
                        assert poison["id"] not in STATE["ack_ids"]
                    finally: close_native(poisoned_pump)
                    STATE["poison_messages"] = []
                    print("PASS: ambiguous native admission remains unacknowledged while later fresh message delivers once")
                    STATE.update(native=None, loaded=[NATIVE], claims=0, deny_claim=True)
                    foreign_private = native_pump()
                    try:
                        wait_for(lambda: STATE["claims"] > 0)
                        assert STATE["native"] is None
                    finally: close_native(foreign_private)
                private_socket.unlink(missing_ok=True)
                private_target.unlink(missing_ok=True)
                print("PASS: initial sole-thread rule, pinned restart with extra loaded thread, missing-pin refusal, existing-binding and owner rejection")
            except Exception:
                log.flush(); log.seek(0); print(log.read()); raise
            finally:
                if not child.stdin.closed: child.stdin.close()
                try: child.wait(timeout=3)
                except subprocess.TimeoutExpired: child.kill(); child.wait()
                api.shutdown()
                # Remove only this test's random source journal, never another session.
                for base in (Path.home() / "Library/Application Support/arrakis-conversations/delivery",
                             Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local/share") / "arrakis-conversations/delivery"):
                    if not base.exists(): continue
                    for suffix in (".sqlite", ".sqlite-journal", ".lock"):
                        for path in base.rglob(SOURCE + suffix): path.unlink(missing_ok=True)

if __name__ == "__main__": main()
