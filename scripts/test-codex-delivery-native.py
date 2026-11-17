#!/usr/bin/env python3
"""Owned native Codex queue acceptance with real model responses, no production API.

Build the candidate CLI first. Run with uv run --no-project --with websockets python.
Uses saved Codex authentication, creates only disposable native threads, and never
changes user configuration or existing threads. Evidence stays under repo tmp/.
"""
import argparse
import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from websockets.sync.client import unix_connect
from websockets.sync.server import unix_serve

ROOT = Path(__file__).resolve().parents[1]

class Rpc:
    def __init__(self, socket):
        self.ws = unix_connect(str(socket), open_timeout=10, legacy=True)
        self.sequence = 0
        self.call("initialize", {"clientInfo":{"name":"conversations-native-queue-test","version":"1"},"capabilities":{"experimentalApi":True}})
        self.ws.send(json.dumps({"method":"initialized"}))
    def call(self, method, params):
        self.sequence += 1
        self.ws.send(json.dumps({"id":self.sequence,"method":method,"params":params}))
        while True:
            reply = json.loads(self.ws.recv(timeout=90))
            if reply.get("id") == self.sequence and "method" not in reply:
                if "error" in reply: raise RuntimeError(f"{method}: {reply['error']}")
                return reply["result"]
            if "id" in reply and "method" in reply:
                self.ws.send(json.dumps({"id":reply["id"],"error":{"code":-32601,"message":"No interactive approval in this disposable probe"}}))
    def close(self): self.ws.close()

def wait(predicate, seconds=90):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = predicate()
        if value: return value
        time.sleep(.1)
    raise AssertionError("native acceptance timed out")

def client_ids(value):
    result = []
    for turn in value["thread"].get("turns", []):
        for item in turn.get("items", []):
            if item.get("type") == "userMessage": result.append(item.get("clientId"))
    return result

def answered(history, key, marker):
    for turn in history["thread"].get("turns", []):
        items = turn.get("items", [])
        if any(item.get("type") == "userMessage" and item.get("clientId") == key for item in items):
            return any(item.get("type") == "agentMessage" and marker in item.get("text", "") for item in items)
    return False

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", type=Path)
    parser.add_argument("--cli", type=Path, default=ROOT / "target/debug/conversations")
    parser.add_argument("--shared", action="store_true", help="create only a new owned thread on the existing shared daemon")
    args = parser.parse_args()
    codex = args.codex or Path.home() / ".npm-global/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex"
    if not codex.is_file(): codex = Path(shutil.which("codex") or "codex")
    folder = ROOT / "tmp" / ("cn-" + uuid.uuid4().hex[:6]); folder.mkdir(parents=True)
    source = str(uuid.uuid4()); nonce = uuid.uuid4().hex[:10]
    socket = Path.home() / ".codex/app-server-control/app-server-control.sock" if args.shared else folder / "s.sock"
    backend = None; pump = None; native = None; rpc = None; api = None; proxy = None
    pump_socket = folder / "p.sock"
    state = {"message":None, "ack":False, "ack_attempts":0, "reject_ack_once":False, "errors":[], "accepted":{}, "adds":{}}
    logs = []
    def log(name):
        output = (folder / name).open("w+"); logs.append(output); return output
    def backend_start():
        child = subprocess.Popen([str(codex), "app-server", "--listen", "unix://" + str(socket)],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=log("backend.log"), cwd=ROOT)
        wait(lambda: socket.exists() or child.poll() is not None, 20)
        assert child.poll() is None, "native backend failed to start"
        return child
    def read(): return rpc.call("thread/read", {"threadId":native,"includeTurns":True})
    def done():
        try: return rpc.call("thread/read", {"threadId":native,"includeTurns":False})["thread"]["status"]["type"] == "idle"
        except RuntimeError as error:
            if "rollout" in str(error) and "is empty" in str(error): return False
            raise
    def queue_list(connection): return connection.call("thread/queue/list", {"threadId":native,"limit":100})["data"]
    class Api(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        def log_message(self, *_): pass
        def reply(self, value, status=200):
            body = json.dumps(value).encode(); self.send_response(status)
            self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            try: self.wfile.write(body)
            except BrokenPipeError: pass
        def do_GET(self):
            assert self.path == f"/api/agent-messaging/sessions/{source}/identity", self.path
            self.reply({"session_id":source,"agent":"codex","agent_session_id":native})
        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if request.get("acknowledge"):
                try:
                    assert request["acknowledge"] == [state["message"]["id"]]
                    key = "conversations:" + state["message"]["id"]
                    assert state["accepted"].get(key), "HTTP ack preceded successful native queue response"
                    state["ack_attempts"] += 1
                    if state["reject_ack_once"]:
                        state["reject_ack_once"] = False
                        return self.reply({"error":"disposable lost acknowledgment response"}, 503)
                    state["ack"] = True
                except Exception as error:
                    state["errors"].append(str(error)); return self.reply({"error":str(error)}, 500)
            if not request.get("acknowledge") and (state["ack"] or state["message"] is None):
                time.sleep(.25)
            self.reply({"messages": [] if state["ack"] or state["message"] is None else [state["message"]]})
    def proxy_connection(inbound):
        outbound = unix_connect(str(socket), legacy=True)
        requests = {}
        def responses():
            try:
                for raw in outbound:
                    value = json.loads(raw)
                    key = requests.get(value.get("id")) if "method" not in value else None
                    if key and "result" in value:
                        assert value["result"]["queuedSubmission"]["clientUserMessageId"] == key
                        state["accepted"][key] = True
                    inbound.send(raw)
            except Exception: pass
            finally: inbound.close()
        reader = threading.Thread(target=responses, daemon=True); reader.start()
        try:
            for raw in inbound:
                value = json.loads(raw)
                if value.get("method") == "thread/queue/add":
                    key = value["params"]["clientUserMessageId"]
                    requests[value["id"]] = key
                    state["adds"][key] = state["adds"].get(key, 0) + 1
                outbound.send(raw)
        except Exception: pass
        finally: outbound.close(); reader.join(timeout=2)
    def start_pump():
        env = dict(os.environ, CONVERSATIONS_CODEX_SOCKET=str(pump_socket))
        child = subprocess.Popen([str(args.cli), "--server", f"http://127.0.0.1:{api.server_port}", "mcp", "serve", "--session", source, "--delivery", "codex"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log("pump-" + str(len(logs)) + ".log"), text=True, env=env)
        child.stdin.write(json.dumps({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"native-pump-test","version":"1"}}}) + "\n"); child.stdin.flush()
        assert select.select([child.stdout], [], [], 10)[0]
        assert "result" in json.loads(child.stdout.readline())
        child.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n'); child.stdin.flush()
        return child
    def stop(child):
        if child is None: return
        if child.stdin and not child.stdin.closed: child.stdin.close()
        try: child.wait(timeout=4)
        except subprocess.TimeoutExpired: child.terminate()
        try: child.wait(timeout=4)
        except subprocess.TimeoutExpired: child.kill(); child.wait()
    def message(label):
        marker = label + "_" + nonce
        value = {"id":str(uuid.uuid4()),"project_id":"native-test-project","sender_session_id":str(uuid.uuid4()),"sender_owner_email":"native-peer@example.test","recipient_session_id":source,
            "text":f"For this disposable test, answer exactly {marker}. Do not call any tools.","reply_to":None,"idempotency_key":label,"created_at":"2026-10-21T00:00:00Z","received_at":None,"acknowledged_at":None,"status":"received"}
        state.update(message=value, ack=False)
        return value, marker
    try:
        if not args.shared: backend = backend_start()
        rpc = Rpc(socket)
        native = rpc.call("thread/start", {"cwd":str(ROOT),"approvalPolicy":"never","sandbox":"read-only","ephemeral":False})["thread"]["id"]
        print("OWNED_NATIVE", native, "endpoint", socket, flush=True)
        rpc.call("turn/start", {"threadId":native,"input":[{"type":"text","text":f"For test {nonce}, answer exactly BASE_{nonce}. Do not use tools.","text_elements":[]}]})
        wait(done)
        proxy = unix_serve(proxy_connection, str(pump_socket))
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        api = ThreadingHTTPServer(("127.0.0.1", 0), Api)
        threading.Thread(target=api.serve_forever, daemon=True).start()
        pump = start_pump()
        idle, marker = message("IDLE")
        wait(lambda: state["ack"]); wait(lambda: answered(read(), "conversations:" + idle["id"], marker)); wait(done)
        history = read(); assert "conversations:" + idle["id"] in client_ids(history)
        print("PASS idle wake and actual native model answer", flush=True)
        # Produce a bounded, visibly active model turn, then queue a peer follow-up.
        busy = rpc.call("turn/start", {"threadId":native,"input":[{"type":"text","text":f"For disposable test {nonce}, write a numbered list from 1 to 180, each with the word waiting. Do not call tools. Finish with BUSY_DONE_{nonce}.","text_elements":[]}]})["turn"]["id"]
        wait(lambda: not done(), 15)
        followup, marker = message("AFTER_BUSY")
        wait(lambda: state["ack"])
        during = read(); ids = client_ids(during)
        # Native queue acceptance may precede the queued user-message turn.
        queued = queue_list(rpc)
        assert "conversations:" + followup["id"] in ids or any(q["clientUserMessageId"] == "conversations:" + followup["id"] for q in queued)
        wait(lambda: "conversations:" + followup["id"] in client_ids(read()))
        wait(lambda: answered(read(), "conversations:" + followup["id"], marker)); wait(done); history = read()
        turns = history["thread"]["turns"]
        busy_index = next(i for i,t in enumerate(turns) if t["id"] == busy)
        follow_index = next(i for i,t in enumerate(turns) if any(item.get("clientId") == "conversations:" + followup["id"] for item in t.get("items", [])))
        assert follow_index > busy_index and turns[busy_index]["status"] == "completed"
        assert answered(history, "conversations:" + followup["id"], marker)
        print("PASS busy follow-up begins after current turn and native model answers", flush=True)
        # Retain an accepted message after a failed HTTP ack; restart only our pump.
        state["reject_ack_once"] = True
        recovery, marker = message("RECOVERY")
        wait(lambda: state["ack_attempts"] >= 3)
        stop(pump); pump = start_pump()
        wait(lambda: state["ack"]); wait(lambda: answered(read(), "conversations:" + recovery["id"], marker)); wait(done)
        history = read(); assert client_ids(history).count("conversations:" + recovery["id"]) == 1
        assert answered(history, "conversations:" + recovery["id"], marker)
        assert not state["errors"], state["errors"]
        assert all(count == 1 for count in state["adds"].values()), state["adds"]
        (folder / "result.json").write_text(json.dumps({"native":native,"source":source,"messages":[idle["id"],followup["id"],recovery["id"]],"endpoint":"shared" if args.shared else "private","result":"passed"}, indent=2))
        (folder / "history.json").write_text(json.dumps(history, indent=2))
        print("PASS pump restart recovers accepted message without duplicate native turn", flush=True)
        print("EVIDENCE", folder, flush=True)
    finally:
        stop(pump)
        if rpc and native:
            try:
                if not done():
                    current = read()["thread"]["turns"][-1]
                    rpc.call("turn/interrupt", {"threadId":native,"turnId":current["id"]})
                rpc.call("thread/archive", {"threadId":native})
            except Exception as error: print("OWNED_THREAD_CLEANUP", type(error).__name__, flush=True)
        if rpc: rpc.close()
        if backend:
            backend.terminate()
            try: backend.wait(timeout=5)
            except subprocess.TimeoutExpired: backend.kill(); backend.wait()
        if api: api.shutdown()
        if proxy: proxy.shutdown()
        for output in logs: output.close()
        for base in (Path.home() / "Library/Application Support/arrakis-conversations/delivery",
                     Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local/share") / "arrakis-conversations/delivery"):
            if not base.exists(): continue
            for suffix in (".sqlite", ".sqlite-journal", ".lock"):
                for path in base.rglob(source + suffix): path.unlink(missing_ok=True)

if __name__ == "__main__": main()
