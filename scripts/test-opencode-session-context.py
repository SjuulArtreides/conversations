#!/usr/bin/env python3
"""Process-level shared-MCP identity routing, without models or user configuration."""
import json
import os
from pathlib import Path
import select
import sqlite3
import subprocess
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = Path(__file__).resolve().parents[1]


def main():
    cli = ROOT / "target/debug/conversations"
    sources = [str(uuid.uuid4()), str(uuid.uuid4())]
    natives = ["ses_context_" + uuid.uuid4().hex for _ in sources]
    metadata = {source: {"id": source, "agent": "opencode", "agent_session_id": native} for source, native in zip(sources, natives)}
    operations, calls = [], []
    denied = set()
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        def log_message(self, *_): pass
        def reply(self, value, status=200):
            body = json.dumps(value).encode(); self.send_response(status)
            self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
        def do_GET(self):
            calls.append(self.path)
            assert not self.path.startswith("/api/sessions/"), "identity lookup must not download transcripts"
            source = self.path.split("/")[4]
            if self.path.endswith("/identity"):
                if source in denied:
                    return self.reply({"error": "only the session owner can inspect its messaging identity"}, 403)
                return self.reply(metadata[source])
            operations.append((source, self.path))
            if source in denied: return self.reply({"error": "only the session owner can use its messaging identity"}, 403)
            self.reply({"sessions": [{"session_id": source}], "source": source})
        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            calls.append(self.path)
            source = self.path.split("/")[4]
            if self.path.endswith("/inbox"):
                return self.reply({"messages": []})
            operations.append((source, self.path))
            if source in denied: return self.reply({"error": "only the session owner can use its messaging identity"}, 403)
            self.reply(dict(body, sender_session_id=source))
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    origin = f"http://127.0.0.1:{server.server_port}"
    child = None
    bridges = []
    sequence = 0
    def exchange(method, params):
        nonlocal sequence
        sequence += 1
        child.stdin.write(json.dumps({"jsonrpc":"2.0", "id":sequence, "method":method, "params":params}) + "\n"); child.stdin.flush()
        assert select.select([child.stdout], [], [], 8)[0], "MCP timed out"
        line = child.stdout.readline()
        assert line, child.stderr.read()
        return json.loads(line)
    def tool(name, native=None, arguments=None):
        params = {"name": name, "arguments": arguments or {}}
        if native is not None: params["_meta"] = {"ai.opencode/sessionID": native}
        return exchange("tools/call", params)
    def error(response):
        assert "error" in response or response.get("result", {}).get("isError"), response
    try:
        env = dict(os.environ, CONVERSATIONS_WRAPPED=str(uuid.uuid4()))
        env["CONVERSATIONS_SESSION_ID"] = str(uuid.uuid4())
        child = subprocess.Popen([str(cli), "--server", origin, "mcp", "serve", "--session-context", "opencode"], env=env,
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        assert "result" in exchange("initialize", {"protocolVersion":"2025-11-25", "capabilities":{}, "clientInfo":{"name":"context-test", "version":"1"}})
        child.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n'); child.stdin.flush()
        for invalid in [None, "", "not-a-native", "ses_bad\n"]:
            before = len(calls); error(tool("sessions_list", invalid)); assert len(calls) == before
        # Registry population must come from a real source-bound bridge, never a tool argument.
        for source, native in zip(sources, natives):
            bridge = subprocess.Popen([str(cli), "--server", origin, "mcp", "bridge", "--session", source, "--native-session", native],
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            bridges.append(bridge)
            import time
            time.sleep(.3)
            assert bridge.poll() is None, bridge.stderr.read()
        for source, native in zip(sources, natives):
            response = tool("sessions_list", native)
            assert not response["result"].get("isError"), response
            assert response["result"]["structuredContent"]["source"] == source, response
            sent = tool("messages_send", native, {"recipient_session_id": sources[1], "text":"peer reply", "idempotency_key":str(uuid.uuid4())})
            assert sent["result"]["structuredContent"]["sender_session_id"] == source, sent
        assert "messages" in tool("messages_receive", natives[0])["result"]["structuredContent"]
        status = tool("messages_status", natives[1], {"message_id":str(uuid.uuid4())})
        assert status["result"]["structuredContent"]["source"] == sources[1], status
        handoff = tool("sessions_handoff", natives[0], {"brief":"task", "idempotency_key":"handoff-default"})
        assert handoff["result"]["structuredContent"]["agent"] == "opencode", handoff
        before = len(operations)
        error(tool("messages_send", natives[0], {"source_session_id":sources[1], "recipient_session_id":sources[1], "text":"forged", "idempotency_key":"forged"}))
        assert len(operations) == before
        metadata[sources[0]]["agent_session_id"] = "ses_changed"
        error(tool("sessions_list", natives[0])); assert len(operations) == before
        metadata[sources[0]]["agent_session_id"] = natives[0]
        denied.add(sources[0]); before = len(operations)
        error(tool("sessions_list", natives[0]))
        assert calls[-1] == f"/api/agent-messaging/sessions/{sources[0]}/identity"
        assert len(operations) == before, "revoked ownership reached a tool operation"
        assert not any(path.startswith("/api/sessions/") for path in calls)
        print("PASS shared MCP contexts route independently; missing/malformed context, model source spoof, changed native, and revoked ownership reject")
    finally:
        for process in [child, *bridges]:
            if process is not None:
                process.stdin.close()
                try: process.wait(timeout=3)
                except subprocess.TimeoutExpired: process.kill(); process.wait()
        server.shutdown()
        data = Path.home() / "Library/Application Support/arrakis-conversations"
        registry = data / "opencode-context/bindings.sqlite"
        if registry.exists():
            with sqlite3.connect(registry) as db:
                db.executemany("DELETE FROM bindings WHERE source=?", [(source,) for source in sources])
        for source in sources:
            if (data / "delivery").exists():
                for suffix in (".sqlite", ".sqlite-journal", ".lock"):
                    for path in (data / "delivery").rglob(source + suffix): path.unlink(missing_ok=True)


if __name__ == "__main__": main()
