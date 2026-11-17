#!/usr/bin/env python3
"""Test messaging and handoff from MCP clients inside real cmux-hosted terminals.

Requires an isolated --cmux-window owned by the caller. Uses a disposable authenticated
server and explicit cookie files. --native-agent additionally runs the installed Codex
with a harmless response-only prompt. It does not change saved login credentials.
"""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import secrets
import shlex
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
CMUX = Path("/Applications/cmux.app/Contents/Resources/bin/cmux")


def load_mcp():
    spec = importlib.util.spec_from_file_location("messaging_process_test", ROOT / "scripts/test-agent-messaging-mcp.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.Mcp


def wait(predicate, label, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            result = predicate()
            if result:
                return result
        except (OSError, ValueError, subprocess.SubprocessError):
            pass
        time.sleep(.1)
    raise AssertionError(f"timed out: {label}")


def fixture(config_file, role):
    config = json.loads(Path(config_file).read_text())
    session = config[role]
    with open(Path(config_file).parent / f"{role}-mcp.log", "w") as log:
        mcp = load_mcp()(Path(config["cli"]), config["origin"], None, log, config["cookie_file"])
        try:
            if role == "peer":
                incoming = mcp.tool("messages_receive", {"wait_seconds": 25})["messages"]
                assert len(incoming) == 1, incoming
                mcp.tool("messages_send", {"recipient_session_id": config["source"],
                    "text": "cmux peer received the message", "reply_to": incoming[0]["id"], "idempotency_key": "peer-reply"})
                result = {"received_id": incoming[0]["id"]}
            else:
                peers = mcp.tool("sessions_list")["sessions"]
                assert [p["session_id"] for p in peers] == [config["peer"]], peers
                sent = mcp.tool("messages_send", {"recipient_session_id": config["peer"],
                    "text": "cmux source requests a contract check", "idempotency_key": "source-question"})
                incoming = mcp.tool("messages_receive", {"wait_seconds": 25})["messages"]
                assert incoming[0]["reply_to"] == sent["id"], incoming
                assert mcp.tool("messages_status", {"message_id": sent["id"]})["status"] == "acknowledged"
                task = {"brief": "Reply with the word formed by joining CMUX, HANDOFF, and OK with underscores. Do not use tools.",
                        "idempotency_key": "cmux-handoff", "agent": "codex"}
                started = mcp.tool("sessions_handoff", task)
                child = started["session_id"]
                def ready():
                    result = mcp.tool("sessions_handoff", task)
                    assert result["session_id"] == child
                    if result["status"] == "failed":
                        raise RuntimeError(result.get("error"))
                    return result if result["status"] == "started" else None
                started = wait(ready, "handoff startup acknowledgment", 40)
                result = {"sent_id": sent["id"], "handoff": started}
            (Path(config_file).parent / f"{role}-result.json").write_text(json.dumps(result))
            print(f"CMUX_{role.upper()}_MCP_PASS", flush=True)
            time.sleep(120)
        finally:
            mcp.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target")
    parser.add_argument("--cmux-window")
    parser.add_argument("--native-agent", action="store_true")
    parser.add_argument("--fixture", choices=["source", "peer"])
    parser.add_argument("--config")
    args = parser.parse_args()
    if args.fixture:
        fixture(args.config, args.fixture)
        return
    if not args.cmux_window:
        parser.error("--cmux-window must identify an isolated window owned by this test")
    if not args.native_agent:
        parser.error("this acceptance test currently requires --native-agent")
    target = args.target_dir.resolve() / "debug"
    (ROOT / "tmp").mkdir(exist_ok=True)
    scratch = Path(tempfile.mkdtemp(prefix="cmux-messaging-", dir=ROOT / "tmp"))
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    origin = f"http://127.0.0.1:{port}"
    runtime = scratch / "host"
    runtime.mkdir(mode=0o700)
    host_cwd = scratch / "working-directory"
    host_cwd.mkdir()
    processes = []
    workspaces = []
    logs = []
    cookie = ""
    environment = {k: v for k, v in os.environ.items() if not k.startswith(("CONVERSATIONS_", "CMUX_", "GOOGLE_", "OIDC_"))}
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
        result = subprocess.run([str(target / "arrakis-conversations"), "host", "--runtime-dir", str(runtime),
            "status", "--json"], capture_output=True, text=True, timeout=8, check=True)
        return json.loads(result.stdout)["sessions"]

    try:
        spawn([str(target / "conversations-server")], "server")
        wait(lambda: http("/healthz"), "server readiness")
        request = urllib.request.Request(origin + "/auth/register", data=json.dumps({"email":"cmux-messaging@example.test",
            "password":secrets.token_urlsafe(24)}).encode(), headers={"Content-Type":"application/json"})
        with urllib.request.urlopen(request, timeout=5) as response:
            cookie = response.headers["Set-Cookie"].split(';', 1)[0]
        cookie_file = scratch / "cookie"
        cookie_file.write_text(cookie)
        cookie_file.chmod(0o600)
        context = wait(lambda: rpc("workspace.create", {"window_id":args.cmux_window, "title":"Messaging isolated host",
            "initial_command":"/bin/sleep 240", "workspace_env":{"CONVERSATIONS_CAPTURE":"0"}, "focus":False}), "isolated cmux window readiness")
        workspaces.append(context["workspace_id"])
        environment.update(CMUX_WORKSPACE_ID=context["workspace_id"], CMUX_SURFACE_ID=context["surface_id"], CMUX_WINDOW_ID=args.cmux_window)
        host = spawn([str(target / "arrakis-conversations"), "--server", origin, "host", "--runtime-dir", str(runtime),
            "--backend", "cmux", "--cookie-file", str(cookie_file)], "host", host_cwd)
        wait(lambda: (runtime / "host.sock").exists(), "host socket")
        host_id = subprocess.check_output(
            [str(target / "arrakis-conversations"), "credential", "--runtime-dir", str(runtime),
             "identity"],
            cwd=ROOT, env=environment, timeout=3, text=True,
        ).strip()
        wait(lambda: http(f"/api/host-relay/{host_id}/status")["online"], "host relay connection")
        project = http("/api/projects", {"name":"cmux collaboration"})["id"]
        source, peer = str(uuid.uuid4()), str(uuid.uuid4())
        config = {"cli":str(target / "arrakis-conversations"), "origin":origin, "cookie_file":str(cookie_file), "source":source, "peer":peer}
        config_file = scratch / "config.json"
        config_file.write_text(json.dumps(config))
        for identity in [source, peer]:
            http("/api/live", {"session_id":identity, "agent":"zsh", "title":"MCP cmux test"})
            http(f"/api/sessions/{identity}/project", {"project_id":project}, "PUT")
        for role, identity in [("peer",peer),("source",source)]:
            argv = [str(target / "arrakis-conversations"), "host", "--runtime-dir", str(runtime), "wrap",
                "--session-id",identity,"--cwd",str(scratch),"--agent","zsh","--",sys.executable,str(Path(__file__).resolve()),
                "--fixture",role,"--config",str(config_file)]
            created = rpc("workspace.create", {"window_id":args.cmux_window,"title":f"Messaging {role}",
                "initial_command":shlex.join(["/usr/bin/env", "CONVERSATIONS_CAPTURE=0", *argv]),
                "workspace_env":{"CONVERSATIONS_CAPTURE":"0"},"focus":False})
            workspaces.append(created["workspace_id"])
        result = wait(lambda: json.loads((scratch / "source-result.json").read_text()), "cmux MCP exchange and handoff", 75)
        peer_result = json.loads((scratch / "peer-result.json").read_text())
        assert peer_result["received_id"] == result["sent_id"]
        child = result["handoff"]["session_id"]
        running = wait(lambda: next((s for s in status() if s["launch"]["id"] == child), None), "handoff wrapper registration")
        workspaces.append(running["workspace"])
        registered = http(f"/api/sessions/{child}")
        assert registered["project_id"] == project
        assert len([s for s in status() if s["launch"]["id"] == child]) == 1
        def native_answer():
            text = rpc("surface.read_text", {"workspace_id":running["workspace"],"surface_id":running["surface"],"lines":100})
            encoded = json.dumps(text)
            (scratch / "native-terminal.json").write_text(encoded)
            return "CMUX_HANDOFF_OK" in encoded
        wait(native_answer, "native Codex response to handoff brief", 90)
        # Fail a second startup before spawning by removing this test host's empty working directory.
        rpc("workspace.close", {"workspace_id":running["workspace"], "force":True})
        host_cwd.rename(scratch / "retired-working-directory")
        with (scratch / "failure-mcp.log").open("w") as log:
            mcp = load_mcp()(target / "arrakis-conversations", origin, source, log, cookie_file)
            try:
                failing = {"brief":"This startup must fail because its directory is absent", "idempotency_key":"expected-startup-failure", "agent":"codex"}
                def failed():
                    response = mcp.tool("sessions_handoff", failing)
                    return response if response["status"] == "failed" else None
                failure = wait(failed, "explicit startup failure report")
                assert "directory" in failure["error"] or "cwd" in failure["error"], failure
                assert mcp.tool("sessions_handoff", failing)["session_id"] == failure["session_id"]
                (scratch / "failure-result.json").write_text(json.dumps(failure))
            finally:
                mcp.close()
        print("PASS: MCP exchange inside cmux, same-project handoff, native Codex initial prompt, startup acknowledgment, retry deduplication, and explicit startup failure")
        print("Evidence:", scratch)
    finally:
        # Collect only this isolated host's child workspaces, never global/current workspaces.
        try:
            workspaces.extend(s["workspace"] for s in status() if s.get("workspace"))
        except (OSError, subprocess.SubprocessError):
            pass
        try:
            for session in status():
                http(f"/api/live/{session['launch']['id']}/complete", {})
            time.sleep(.3)
        except (OSError, subprocess.SubprocessError):
            pass
        for workspace in set(workspaces):
            try:
                rpc("workspace.close", {"workspace_id":workspace, "force":True})
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
