#!/usr/bin/env python3
"""Start real handoff children on a chosen model through a real local host.

Uses a disposable authenticated server and an explicit cookie file; never changes saved
login credentials. Cases (repeat --case to pick several):

  opencode  OpenCode child on --model; checks the uploaded transcript's assistant model
  unknown   OpenCode child on a model that does not exist; expects status failed
  codex     Codex child with --codex-model; checks the child's argv (no login needed)
  adk       OpenCode child through the adk launcher; checks the TUI status bar (cmux only).
            Needs --adk-opencode: an OpenCode binary that adk accepts. The case runs the real
            adk with --private (own server, in-memory database) and that binary in a HOME
            overlay, so it never touches the shared host server, its install, or your data.
            Only OpenCode's free models exist there, hence --adk-model.
            With --adk-attached instead, adk attaches to your running OpenCode host server (the
            normal path). It then also checks the session export, a typed follow-up, and an
            unknown model through adk, using --model. Its sessions are deleted afterwards.

--backend normal runs headless children (any terminal, no cmux). --backend cmux needs an
isolated --cmux-window owned by the caller. Extra PATH entries (for example a private
OpenCode or Codex) go first with --path.
"""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import struct
import tempfile
import time
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
CMUX = Path("/Applications/cmux.app/Contents/Resources/bin/cmux")
BRIEF = "Reply with the single word HANDOFF_MODEL_OK. Do not use tools."


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def register_source(runtime, cwd, session):
    """Keep a fixture source registered on the owner-only local host socket."""
    connection = socket.socket(socket.AF_UNIX)
    connection.connect(str(runtime / "host.sock"))
    message = json.dumps({"type": "register", "registration": {
        "launch": {"id": session, "agent": "zsh", "cwd": str(cwd),
            "command": ["/bin/sleep", "600"]},
        "workspace": None, "surface": None, "local_terminal": True,
        "effect_gate": True}}).encode()
    connection.sendall(struct.pack(">I", len(message)) + message)
    return connection


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target")
    parser.add_argument("--backend", choices=["normal", "cmux"], default="normal")
    parser.add_argument("--cmux-window")
    parser.add_argument("--case", action="append", choices=["opencode", "unknown", "codex", "adk"], required=True)
    parser.add_argument("--model", default="anthropic/claude-haiku-4-5")
    parser.add_argument("--variant")
    parser.add_argument("--codex-model", default="gpt-6.1-sol")
    parser.add_argument("--adk-model", default="opencode/deepseek-v4.1-flash-free")
    parser.add_argument("--adk-opencode", type=Path)
    parser.add_argument("--adk-attached", action="store_true")
    parser.add_argument("--path", action="append", default=[])
    args = parser.parse_args()
    if args.backend == "cmux" and not args.cmux_window:
        parser.error("--backend cmux needs an isolated --cmux-window")
    if "adk" in args.case and (args.backend != "cmux" or not (args.adk_opencode or args.adk_attached)):
        parser.error("the adk case reads the TUI status bar, so it needs --backend cmux and --adk-opencode or --adk-attached")
    Mcp = load("messaging_process_test", "scripts/test-agent-messaging-mcp.py").Mcp
    wait = load("cmux_messaging_test", "scripts/test-cmux-agent-messaging.py").wait
    target = args.target_dir.resolve() / "debug"
    (ROOT / "tmp").mkdir(exist_ok=True)
    scratch = Path(tempfile.mkdtemp(prefix="handoff-model-", dir=ROOT / "tmp"))
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    origin = f"http://127.0.0.1:{port}"
    runtime = scratch / "host"
    runtime.mkdir(mode=0o700)
    host_cwd = scratch / "working-directory"
    host_cwd.mkdir()
    processes, workspaces, logs, clients, children, natives = [], [], [], [], [], []
    registrations = []
    cookie = ""
    environment = {k: v for k, v in os.environ.items()
        if not k.startswith(("CONVERSATIONS_", "CMUX_", "GOOGLE_", "OIDC_")) or k.startswith("CMUX_SOCKET")}
    if args.backend == "normal":
        environment = {k: v for k, v in environment.items() if not k.startswith("CMUX_")}
    if "adk" in args.case and not args.adk_attached:
        overlay = scratch / "adk-home"
        (overlay / ".opencode/bin").mkdir(parents=True)
        home = Path.home()
        for entry in home.iterdir():
            if entry.name != ".opencode":
                (overlay / entry.name).symlink_to(entry)
        for entry in (home / ".opencode").iterdir():
            if entry.name != "bin":
                (overlay / ".opencode" / entry.name).symlink_to(entry)
        (overlay / ".opencode/bin/opencode").symlink_to(args.adk_opencode.resolve())
        shim = scratch / "adk-bin"
        shim.mkdir()
        (shim / "adk").write_text(f"""#!/bin/sh
export HOME={overlay}
if [ "$1" = code ]; then shift; exec direnv exec {home}/example adk code --private "$@"; fi
exec direnv exec {home}/example adk "$@"
""")
        (shim / "adk").chmod(0o755)
        args.path[:0] = [str(shim), str(args.adk_opencode.resolve().parent)]
    environment["PATH"] = os.pathsep.join([*args.path, environment["PATH"]])
    environment.update(BIND_ADDR=f"127.0.0.1:{port}", DATABASE_URL=f"sqlite://{scratch}/test.sqlite",
        AUTH_ENABLED="true", PASSWORD_AUTH_ENABLED="true", COOKIE_SECURE="false", SHUTDOWN_DRAIN_SECS="0",
        LIVE_REAP_INTERVAL_SECS="0", CONVERSATIONS_CAPTURE="0")

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

    # Longer than the host's own 120s verdict, which starts after claim and launch.
    def settle(mcp, task, timeout=150):
        def done():
            result = mcp.tool("sessions_handoff", task)
            # Track the child as soon as it exists, so a timeout still ends it.
            if result.get("session_id") and result["session_id"] not in children:
                children.append(result["session_id"])
            return result if result["status"] in ("started", "failed") else None
        return wait(done, f"handoff {task['idempotency_key']} to settle", timeout)

    settings_file = scratch / "settings.json"
    settings_file.write_text(json.dumps({"launch": {"opencode_auto": True}}))
    environment.update(CONVERSATIONS_HOST_DIR=str(runtime),
        CONVERSATIONS_HOST_SETTINGS_FILE=str(settings_file))
    results = {}
    try:
        spawn([str(target / "conversations-server")], "server")
        wait(lambda: http("/healthz"), "server readiness")
        request = urllib.request.Request(origin + "/auth/register", data=json.dumps({"email": "handoff-model@example.test",
            "password": secrets.token_urlsafe(24)}).encode(), headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=5) as response:
            cookie = response.headers["Set-Cookie"].split(";", 1)[0]
        cookie_file = scratch / "cookie"
        cookie_file.write_text(cookie)
        cookie_file.chmod(0o600)
        if args.backend == "cmux":
            context = wait(lambda: rpc("workspace.create", {"window_id": args.cmux_window, "title": "Handoff model host",
                "initial_command": "/bin/sleep 600", "workspace_env": {"CONVERSATIONS_CAPTURE": "0"}, "focus": False}),
                "isolated cmux window readiness")
            workspaces.append(context["workspace_id"])
            environment.update(CMUX_WORKSPACE_ID=context["workspace_id"], CMUX_SURFACE_ID=context["surface_id"],
                CMUX_WINDOW_ID=args.cmux_window, CMUX_BUNDLED_CLI_PATH=str(CMUX))
        spawn([str(target / "arrakis-conversations"), "--server", origin, "host", "--runtime-dir", str(runtime),
            "run", "--backend", args.backend, "--cookie-file", str(cookie_file)], "host", host_cwd)
        wait(lambda: (runtime / "host.sock").exists(), "local host socket")
        source = str(uuid.uuid4())
        registrations.append(register_source(runtime, host_cwd, source))
        wait(lambda: any(s["launch"]["id"] == source for s in status()), "local source registration")
        log = open(scratch / "source-mcp.log", "w")
        logs.append(log)
        mcp = Mcp(target / "arrakis-conversations", origin, source, log, cookie_file, env=environment)
        clients.append(mcp)

        if "opencode" in args.case:
            task = {"brief": BRIEF, "idempotency_key": "opencode-model", "agent": "opencode", "model": args.model}
            if args.variant:
                task["variant"] = args.variant
            settled = settle(mcp, task)
            assert settled["status"] == "started", settled
            wanted = args.model.split("/", 1)[1]
            # Without the fix the TUI would pick its most recent model, so the proof needs another one.
            state = Path(environment.get("XDG_STATE_HOME") or Path.home() / ".local/state") / "opencode/model.json"
            recent = json.loads(state.read_text())["recent"][0] if state.exists() else None
            assert not recent or recent["modelID"] != wanted, f"pick a --model other than the TUI's recent {recent}"

            report = runtime / "opencode" / settled["session_id"] / "report.json"
            native = wait(lambda: json.loads(report.read_text())["nativeID"], "native OpenCode session report", 60)
            natives.append(native)

            def answered():
                exported = subprocess.run(["opencode", "session", "export", native], cwd=host_cwd, env=environment,
                    capture_output=True, text=True, timeout=30)
                session = json.loads(exported.stdout)
                models = [m["model"]["id"] for m in session["messages"] if m.get("type") == "assistant" and m.get("model")]
                return (session["info"]["model"]["id"], models) if models else None
            session_model, models = wait(answered, "assistant reply in OpenCode", 120)
            assert session_model == wanted and all(model == wanted for model in models), (session_model, models)
            results["opencode"] = {"status": settled["status"], "native": native, "session_model": session_model,
                "assistant_models": models}
            print(f"PASS opencode: started, session and assistant on {session_model} ({native})")

        if "unknown" in args.case:
            task = {"brief": BRIEF, "idempotency_key": "unknown-model", "agent": "opencode",
                    "model": "anthropic/claude-does-not-exist"}
            settled = settle(mcp, task)
            assert settled["status"] == "failed", settled
            assert "not available" in settled["error"] and "opencode models" in settled["error"], settled
            results["unknown"] = {"status": settled["status"], "error": settled["error"]}
            print(f"PASS unknown: failed with {settled['error']!r}")

        if "codex" in args.case:
            task = {"brief": BRIEF, "idempotency_key": "codex-model", "agent": "codex",
                    "model": args.codex_model, "variant": "high"}
            settled = settle(mcp, task)
            assert settled["status"] == "started", settled

            def codex_argv():
                listing = subprocess.run(["ps", "-axww", "-o", "command="], capture_output=True, text=True).stdout
                return next((line for line in listing.splitlines()
                    if "codex" in line and args.codex_model in line and "arrakis-conversations" not in line), None)
            argv = wait(codex_argv, "codex process with --model", 30)
            assert f"--model {args.codex_model}" in argv and 'model_reasoning_effort="high"' in argv, argv
            results["codex"] = {"status": settled["status"], "argv": argv}
            print(f"PASS codex: started with {argv}")

        if "adk" in args.case:
            settings = json.loads(settings_file.read_text())
            settings.setdefault("launch", {})["default_launcher"] = "adk"
            settings_file.write_text(json.dumps(settings))
            adk_model = args.model if args.adk_attached else args.adk_model

            def screen_of(running):
                text = json.dumps(rpc("surface.read_text", {"workspace_id": running["workspace"],
                    "surface_id": running["surface"], "lines": 80}))
                (scratch / "adk-screen.json").write_text(text)
                return text

            def adk_handoff(key, model):
                task = {"brief": BRIEF, "idempotency_key": key, "agent": "opencode", "model": model, "variant": "high"}
                settled = settle(mcp, task, 180)
                # adk can pause on a 1Password approval before its terminal registers.
                running = wait(lambda: next((s for s in status() if s["launch"]["id"] == settled["session_id"]), None),
                    "adk child registration", 90)
                workspaces.append(running["workspace"])
                return settled, running

            settled, running = adk_handoff("adk-model", adk_model)
            if settled["status"] != "started":
                screen_of(running)
            assert settled["status"] == "started", settled
            screen = wait(lambda: (lambda t: t if "HANDOFF_MODEL_OK" in t else None)(screen_of(running)),
                "adk child answer", 120)
            label = adk_model.split("/", 1)[1].split("-free")[0].lower()
            assert label.split("-")[0] in screen.lower(), "status bar does not name the chosen model"
            results["adk"] = {"status": settled["status"], "screen": str(scratch / "adk-screen.json")}
            print("PASS adk: started, answered, and the status bar names the chosen model")

            if args.adk_attached:
                wanted = adk_model.split("/", 1)[1]
                report = runtime / "opencode" / settled["session_id"] / "report.json"
                native = wait(lambda: json.loads(report.read_text())["nativeID"], "adk native session report", 60)
                natives.append(native)

                def models():
                    exported = subprocess.run(["opencode", "session", "export", native], env=environment,
                        capture_output=True, text=True, timeout=30)
                    (scratch / "adk-export.json").write_text(exported.stdout + exported.stderr)
                    session = json.loads(exported.stdout)
                    return (session["info"].get("model") or {}).get("id"), [m["model"]["id"] for m in session["messages"]
                        if m.get("type") == "assistant" and m.get("model")]
                session_model, replies = wait(lambda: (lambda r: r if r[1] else None)(models()),
                    "adk assistant reply in OpenCode", 60)
                assert session_model == wanted and replies == [wanted] * len(replies), (session_model, replies)
                print(f"PASS adk export: session and assistant on {session_model} ({native})")

                follow = "Reply with the single word FOLLOW_UP_OK. Do not use tools."
                subprocess.run([str(CMUX), "send", "--workspace", running["workspace"], follow], check=True,
                    capture_output=True, timeout=10)
                time.sleep(1)
                subprocess.run([str(CMUX), "send-key", "--workspace", running["workspace"], "enter"], check=True,
                    capture_output=True, timeout=10)
                session_model, replies = wait(lambda: (lambda r: r if len(r[1]) >= 2 else None)(models()),
                    "follow-up reply", 120)
                assert session_model == wanted and replies == [wanted] * len(replies), (session_model, replies)
                results["adk"].update(native=native, session_model=session_model, replies=replies)
                print(f"PASS adk follow-up: typed prompt stayed on {wanted} ({len(replies)} replies)")

                settled, running = adk_handoff("adk-unknown", "anthropic/claude-does-not-exist")
                # adk created a session before the plugin refused the model; clean it up too.
                report = runtime / "opencode" / settled["session_id"] / "report.json"
                try:
                    natives.append(wait(lambda: json.loads(report.read_text())["nativeID"], "adk session report", 30))
                except AssertionError:
                    print("WARNING: could not find the session adk created for the unknown model; delete it by hand")
                assert settled["status"] == "failed", settled
                assert "not available" in settled["error"] and "opencode models" in settled["error"], settled
                results["adk_unknown"] = {"status": settled["status"], "error": settled["error"]}
                print(f"PASS adk unknown: failed with {settled['error']!r}")

        (scratch / "result.json").write_text(json.dumps(results, indent=2))
    finally:
        for child in children:
            try:
                mcp.tool("sessions_end", {"session_id": child})
            except (OSError, ValueError, AssertionError):
                pass
        for client in clients:
            client.close()
        for connection in registrations:
            connection.close()
        for native in natives:
            # The child lives in the user's real OpenCode database; leave nothing behind.
            subprocess.run(["opencode", "session", "delete", native], cwd=host_cwd, env=environment,
                capture_output=True, timeout=30)
        if args.backend == "cmux":
            try:
                workspaces.extend(s["workspace"] for s in status() if s.get("workspace"))
            except (OSError, subprocess.SubprocessError):
                pass
            for workspace in set(workspaces):
                try:
                    rpc("workspace.close", {"workspace_id": workspace, "force": True})
                except subprocess.SubprocessError:
                    pass
        time.sleep(1)
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
