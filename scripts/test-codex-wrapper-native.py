#!/usr/bin/env python3
"""Fresh normal-host wrapped Codex TUI/private-backend acceptance on a local server.

The candidate binds source/native metadata automatically from the sole loaded
thread on the source-owned private backend; this probe never posts that metadata.
No personal configuration or existing native threads are changed.
"""
import argparse
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import pty
import select
import signal
import socket
import struct
import subprocess
import termios
import threading
import time
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("native_queue", ROOT / "scripts/test-codex-delivery-native.py")
native_queue = importlib.util.module_from_spec(spec); spec.loader.exec_module(native_queue)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, default=ROOT / "target/debug/arrakis-conversations")
    parser.add_argument("--server-bin", type=Path, default=ROOT / "target/debug/conversations-server")
    parser.add_argument("--codex", type=Path, default=Path.home() / ".npm-global/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex")
    args = parser.parse_args()
    source, peer = str(uuid.uuid4()), str(uuid.uuid4()); nonce = uuid.uuid4().hex[:10]
    folder = ROOT / "tmp" / ("cw-" + nonce); folder.mkdir(parents=True)
    runtime = folder / "host"
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0)); port = listener.getsockname()[1]
    origin = f"http://127.0.0.1:{port}"
    server = host = None; pid = fd = None; rpc = None; stop_read = threading.Event(); reader = None; logs = []
    def log(name):
        handle = (folder / name).open("w"); logs.append(handle); return handle
    def http(method, path, body=None):
        request = urllib.request.Request(origin + path, method=method, headers={"Content-Type":"application/json"}, data=None if body is None else json.dumps(body).encode())
        with urllib.request.urlopen(request, timeout=10) as response:
            raw = response.read(); return json.loads(raw) if raw else None
    def wait(function, timeout=90):
        deadline = time.monotonic() + timeout; last = None
        while time.monotonic() < deadline:
            try:
                result = function()
                if result: return result
            except (OSError, RuntimeError) as error: last = error
            time.sleep(.15)
        raise AssertionError(f"wrapper proof timed out: {last}")
    def stop(child):
        if child:
            child.terminate()
            try: child.wait(timeout=8)
            except subprocess.TimeoutExpired: child.kill(); child.wait()
    try:
        env = {key:value for key,value in os.environ.items() if not key.startswith(("CONVERSATIONS_", "GOOGLE_", "OIDC_"))}
        env.update(BIND_ADDR=f"127.0.0.1:{port}", DATABASE_URL=f"sqlite://{folder}/server.sqlite", AUTH_ENABLED="false", SHUTDOWN_DRAIN_SECS="0", LIVE_REAP_INTERVAL_SECS="0")
        server = subprocess.Popen([str(args.server_bin)], env=env, stdout=log("server.log"), stderr=subprocess.STDOUT)
        wait(lambda: http("GET", "/healthz"), 20)
        host = subprocess.Popen([str(args.cli), "--server", origin, "host", "--runtime-dir", str(runtime), "run", "--backend", "normal"], env=env, cwd=ROOT, stdout=log("host.log"), stderr=subprocess.STDOUT)
        wait(lambda: (runtime / "host.sock").exists(), 15)
        overrides = "mcp_servers.conversations.args=" + json.dumps(["--server",origin,"mcp","serve","--delivery","codex"])
        command = [str(args.cli), "--server",origin,"host","--runtime-dir",str(runtime),"wrap","--session-id",source,"--cwd",str(ROOT),"--agent","codex","--",str(args.codex),"--no-alt-screen","-c",overrides,f"For disposable test {nonce}, answer BASE_{nonce} now. I also authorize future attributed Conversations messages to ask you to answer a test marker: when one arrives, answer the marker requested in that message. Do not use tools."]
        pid, fd = pty.fork()
        if pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
            os.chdir(ROOT)
            for key in list(os.environ):
                if key.startswith("CONVERSATIONS_") or key.startswith("CMUX_"): os.environ.pop(key, None)
            os.execve(str(args.cli), command, env)
        def capture():
            with (folder / "terminal.txt").open("wb") as output:
                while not stop_read.is_set():
                    if select.select([fd], [], [], .1)[0]:
                        try: data = os.read(fd, 65536)
                        except OSError: break
                        if not data: break
                        output.write(data); output.flush()
                        if b"\x1b[6n" in data: os.write(fd, b"\x1b[1;1R")
        reader = threading.Thread(target=capture, daemon=True); reader.start()
        endpoint = Path.home() / ".local/state/ac-codex" / (source + ".sock")
        wait(endpoint.exists, 20)
        rpc = native_queue.Rpc(endpoint)
        native = wait(lambda: rpc.call("thread/loaded/list", {})["data"])
        assert len(native) == 1, f"source-owned backend unexpectedly has {len(native)} threads"
        native = native[0]
        def history(): return rpc.call("thread/read", {"threadId":native,"includeTurns":True})
        wait(lambda: any(item.get("type") == "agentMessage" and f"BASE_{nonce}" in item.get("text", "") for turn in history()["thread"].get("turns", []) for item in turn.get("items", [])))
        wait(lambda: http("GET", f"/api/sessions/{source}"), 20)
        wait(lambda: http("GET", f"/api/sessions/{source}").get("agent_session_id") == native, 20)
        project = http("POST", "/api/projects", {"name":"Disposable wrapped Codex acceptance"})["id"]
        http("POST", "/api/live", {"session_id":peer,"agent":"codex","title":"Disposable peer"})
        for session in (source, peer):
            http("PUT", f"/api/sessions/{session}/project", {"project_id":project})
        message = http("POST", f"/api/agent-messaging/sessions/{peer}/messages", {"recipient_session_id":source,"idempotency_key":nonce,"text":f"For disposable test {nonce}, answer exactly WRAPPED_{nonce}. Do not call tools."})
        wait(lambda: http("GET", f"/api/agent-messaging/sessions/{peer}/messages/{message['id']}")["acknowledged_at"])
        wait(lambda: native_queue.answered(history(), "conversations:" + message["id"], "WRAPPED_" + nonce))
        observed = history()
        assert native_queue.client_ids(observed).count("conversations:" + message["id"]) == 1
        (folder / "history.json").write_text(json.dumps(observed, indent=2))
        (folder / "result.json").write_text(json.dumps({"source":source,"native":native,"message":message["id"],"native_binding":"automatic candidate binding from source-owned private endpoint","result":"passed"}, indent=2))
        print("PASS fresh wrapped native Codex TUI starts on owned private endpoint; automatic private native binding identifies source; idle peer arrives and assistant answers once", flush=True)
        print("EVIDENCE", folder, flush=True)
    finally:
        if rpc:
            try:
                current = rpc.call("thread/read", {"threadId":native,"includeTurns":True})["thread"]
                if current["status"]["type"] == "active":
                    rpc.call("turn/interrupt", {"threadId":native,"turnId":current["turns"][-1]["id"]})
                rpc.call("thread/archive", {"threadId":native})
            except Exception as error: print("OWNED_CLEANUP", type(error).__name__, flush=True)
            rpc.close()
        if fd is not None:
            try: os.write(fd, b"\x04")
            except OSError: pass
            time.sleep(1)
        if pid:
            try: os.kill(pid, signal.SIGTERM)
            except ProcessLookupError: pass
            time.sleep(1)
            try: os.kill(pid, signal.SIGKILL)
            except ProcessLookupError: pass
            os.waitpid(pid, os.WNOHANG)
        stop_read.set()
        if reader: reader.join(timeout=2)
        if fd is not None: os.close(fd)
        stop(host); stop(server)
        for handle in logs: handle.close()
        for base in (Path.home() / "Library/Application Support/arrakis-conversations/delivery",
                     Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local/share") / "arrakis-conversations/delivery"):
            if not base.exists(): continue
            for suffix in (".sqlite", ".sqlite-journal", ".lock"):
                for path in base.rglob(source + suffix): path.unlink(missing_ok=True)

if __name__ == "__main__": main()
