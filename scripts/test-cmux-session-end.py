#!/usr/bin/env python3
"""End a real cmux-hosted handoff child through the sessions_end MCP tool.

Requires an isolated --cmux-window owned by the caller. Uses a disposable authenticated
server and an explicit cookie file. Starts the installed agent (OpenCode by default) with
a short response-only brief, ends it, and reports whether its cmux workspace remains.
It does not change saved login credentials.
"""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import time
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
CMUX = Path("/Applications/cmux.app/Contents/Resources/bin/cmux")


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target")
    parser.add_argument("--cmux-window", required=True)
    parser.add_argument("--agent", choices=["opencode", "claude", "codex"], default="opencode")
    args = parser.parse_args()
    Mcp = load("messaging_process_test", "scripts/test-agent-messaging-mcp.py").Mcp
    wait = load("cmux_messaging_test", "scripts/test-cmux-agent-messaging.py").wait
    register_source = load("handoff_model_test", "scripts/test-handoff-model.py").register_source
    target = args.target_dir.resolve() / "debug"
    (ROOT / "tmp").mkdir(exist_ok=True)
    scratch = Path(tempfile.mkdtemp(prefix="cmux-session-end-", dir=ROOT / "tmp"))
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    origin = f"http://127.0.0.1:{port}"
    runtime = scratch / "host"
    runtime.mkdir(mode=0o700)
    host_cwd = scratch / "working-directory"
    host_cwd.mkdir()
    processes, workspaces, logs, clients = [], [], [], []
    registrations = []
    cookie = ""
    # Newer cmux authorizes socket clients by capability, so keep the socket settings.
    environment = {k: v for k, v in os.environ.items()
        if not k.startswith(("CONVERSATIONS_", "CMUX_", "GOOGLE_", "OIDC_")) or k.startswith("CMUX_SOCKET")}
    environment.update(BIND_ADDR=f"127.0.0.1:{port}", DATABASE_URL=f"sqlite://{scratch}/test.sqlite",
        AUTH_ENABLED="true", PASSWORD_AUTH_ENABLED="true", COOKIE_SECURE="false", SHUTDOWN_DRAIN_SECS="0",
        LIVE_REAP_INTERVAL_SECS="0", CMUX_BUNDLED_CLI_PATH=str(CMUX))

    def rpc(method, params):
        response = subprocess.run([str(CMUX), "--json", "rpc", method, json.dumps(params)],
            capture_output=True, text=True, timeout=10, check=True)
        value = json.loads(response.stdout)
        return value.get("result", value)

    def spawn(argv, name, cwd=ROOT):
        log = open(scratch / f"{name}.log", "w")
        logs.append(log)
        child = subprocess.Popen(argv, cwd=cwd, env=environment, stdout=log, stderr=log)
        processes.append(child)
        return child

    def http(path, body=None, method=None):
        request = urllib.request.Request(origin + path, data=None if body is None else json.dumps(body).encode(),
            headers={"Content-Type": "application/json", "Cookie": cookie}, method=method)
        with urllib.request.urlopen(request, timeout=5) as response:
            return json.load(response)

    def status():
        result = subprocess.run([str(target / "conversations"), "host", "--runtime-dir", str(runtime),
            "status", "--json"], capture_output=True, text=True, timeout=8, check=True)
        return json.loads(result.stdout)["sessions"]

    def window_workspaces():
        listed = rpc("workspace.list", {"window_id": args.cmux_window})
        return {w["id"].lower() for w in listed["workspaces"]}

    environment.update(CONVERSATIONS_HOST_DIR=str(runtime),
        CONVERSATIONS_HOST_SETTINGS_FILE=str(scratch / "settings.json"))
    try:
        spawn([str(target / "conversations-server")], "server")
        wait(lambda: http("/healthz"), "server readiness")
        request = urllib.request.Request(origin + "/auth/register", data=json.dumps({"email": "cmux-end@example.test",
            "password": secrets.token_urlsafe(24)}).encode(), headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=5) as response:
            cookie = response.headers["Set-Cookie"].split(";", 1)[0]
        cookie_file = scratch / "cookie"
        cookie_file.write_text(cookie)
        cookie_file.chmod(0o600)
        context = wait(lambda: rpc("workspace.create", {"window_id": args.cmux_window, "title": "Session end isolated host",
            "initial_command": "/bin/sleep 240", "workspace_env": {"CONVERSATIONS_CAPTURE": "0"}, "focus": False}),
            "isolated cmux window readiness")
        workspaces.append(context["workspace_id"])
        environment.update(CMUX_WORKSPACE_ID=context["workspace_id"], CMUX_SURFACE_ID=context["surface_id"], CMUX_WINDOW_ID=args.cmux_window)
        spawn([str(target / "conversations"), "--server", origin, "host", "--runtime-dir", str(runtime),
            "--backend", "cmux", "--cookie-file", str(cookie_file)], "host", host_cwd)
        wait(lambda: (runtime / "host.sock").exists(), "host socket")
        source, context_session = str(uuid.uuid4()), str(uuid.uuid4())
        for identity in [source, context_session]:
            registrations.append(register_source(runtime, host_cwd, identity))
            wait(lambda: any(s["launch"]["id"] == identity for s in status()), "local source registration")
        log = open(scratch / "source-mcp.log", "w")
        logs.append(log)
        mcp = Mcp(target / "conversations", origin, source, log, cookie_file, env=environment)
        clients.append(mcp)

        refused = mcp.tool("sessions_end", {"session_id": context_session}, error=True)
        assert "sessions_handoff" in refused["content"][0]["text"], refused
        assert any(s["launch"]["id"] == context_session for s in status()), "refusal changed the session"

        task = {"brief": "Reply with the single word READY. Do not use tools.", "idempotency_key": "end-child", "agent": args.agent}
        child = mcp.tool("sessions_handoff", task)["session_id"]

        def started():
            result = mcp.tool("sessions_handoff", task)
            if result["status"] == "failed":
                raise RuntimeError(result.get("error"))
            return result if result["status"] == "started" else None
        wait(started, "handoff startup", 40)
        running = wait(lambda: next((s for s in status() if s["launch"]["id"] == child), None), "child wrapper registration")
        workspace = running["workspace"]
        workspaces.append(workspace)
        assert workspace.lower() in window_workspaces(), "the leftover-workspace check cannot see the child"
        time.sleep(5)

        ended = mcp.tool("sessions_end", {"session_id": child})
        assert ended["status"] == "ending" and ended["host_notified"] is True, ended
        # The host's own registry is the evidence that the process exited.
        wait(lambda: all(s["launch"]["id"] != child for s in status()), "child wrapper exit", 20)
        again = mcp.tool("sessions_end", {"session_id": child})
        assert again["status"] == "not_running", again
        time.sleep(2)
        leftover = workspace.lower() in window_workspaces()
        result = {"child": child, "workspace": workspace, "workspace_left_open": leftover}
        (scratch / "result.json").write_text(json.dumps(result))
        print("PASS: refused an unrelated local session, ended a cmux handoff child, host saw the wrapper exit, repeat is not_running")
        print("Child workspace left open after End:", leftover)
    finally:
        for client in clients:
            client.close()
        for connection in registrations:
            connection.close()
        try:
            workspaces.extend(s["workspace"] for s in status() if s.get("workspace"))
            time.sleep(.3)
        except (OSError, subprocess.SubprocessError):
            pass
        for workspace in set(workspaces):
            try:
                rpc("workspace.close", {"workspace_id": workspace, "force": True})
            except subprocess.SubprocessError:
                pass
        for child in reversed(processes):
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
        for log in logs:
            log.close()
        print("Test artifacts:", scratch)


if __name__ == "__main__":
    main()
