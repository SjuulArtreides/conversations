#!/usr/bin/env python3
"""Restore a saved cmux terminal through the real CLI, PTY and login shell.

The cmux metadata boundary is simulated; no user sessions or settings are touched.
"""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--cli", type=Path, default=root / "target/release/arrakis-conversations")
parser.add_argument("--binding-kind", choices=["codex", "conversations"], default="conversations")
parser.add_argument("--managed", action="store_true", help="Restore using persisted workspace identity with capture=0")
parser.add_argument("--fail-inspection", action="store_true")
parser.add_argument("--missing-native", action="store_true")
parser.add_argument("--host-binding-only", action="store_true")
parser.add_argument("--transient-inspection", action="store_true")
args = parser.parse_args()
(root / "tmp").mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(prefix="restore-test-", dir=root / "tmp") as scratch:
    folder = Path(scratch)
    config = folder / ("Library/Application Support" if sys.platform == "darwin" else ".config") / "arrakis-conversations"
    config.mkdir(parents=True)
    ident, native = str(uuid.uuid4()), str(uuid.uuid4())
    url = "http://127.0.0.1:1"
    (config / "open-sessions.json").write_text(json.dumps({"servers": {url: [{
        "session_id": ident, "agent": "codex", "cwd": str(folder),
        "resume_id": None if args.missing_native else native, "agent_args": ["two words", "", "$(literal)"]
    }, {
        "session_id": str(uuid.uuid4()), "agent": "codex", "cwd": str(folder),
        "resume_id": str(uuid.uuid4())
    }]}}))
    settings = folder / "settings.json"
    settings.write_text('{"cmux":{"auto_wrap":true}}')
    runtime = folder / "host"
    runtime.mkdir(mode=0o700)
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(str(runtime / "host.sock"))
    listener.listen()
    connections = []
    def accept():
        while True:
            try:
                connection, _ = listener.accept()
                connections.append(connection)
            except OSError:
                return
    threading.Thread(target=accept, daemon=True).start()
    fake_cmux = folder / "cmux"
    calls = folder / "cmux-calls.jsonl"
    surface, workspace = str(uuid.uuid4()), str(uuid.uuid4())
    checkpoint = ident if args.binding_kind == "conversations" else native
    fake_cmux.write_text(f'''#!{sys.executable}
import json,sys,os
from pathlib import Path
with Path({str(calls)!r}).open('a') as f: f.write(json.dumps(sys.argv[1:])+'\\n')
if 'tree' in sys.argv:
    print(json.dumps({{'windows':[{{'id':'55555555-5555-4555-8555-555555555555','workspaces':[{{'id':{workspace!r},'panes':[{{'surfaces':[{{'id':{surface!r}}}]}}]}}]}}]}}))
elif 'resume' in sys.argv:
    if os.environ.get('TEST_FAIL_INSPECTION'): sys.exit(1)
    if os.environ.get('TEST_TRANSIENT_INSPECTION') and not Path({str(folder / 'inspection-retried')!r}).exists():
        Path({str(folder / 'inspection-retried')!r}).touch()
        sys.exit(1)
    print(json.dumps({{'resume_binding':None if os.environ.get('TEST_NO_BINDING') else {{'checkpoint_id':os.environ.get('TEST_HOST_CHECKPOINT', {checkpoint!r}),'kind':{args.binding_kind!r}}}}}))
else:
    print(json.dumps({{'ok':True, 'workspaces':[]}}))
''')
    fake_cmux.chmod(0o700)
    bin_dir = folder / "interactive-bin"
    bin_dir.mkdir()
    probe = bin_dir / "codex"
    result = folder / "result.json"
    probe.write_text(f"#!{sys.executable}\nimport json,os,sys\nfrom pathlib import Path\nPath({str(result)!r}).write_text(json.dumps({{'args':sys.argv[1:], 'cwd':os.getcwd(), 'id':os.environ.get('CONVERSATIONS_WRAPPED')}}))\n")
    probe.chmod(0o700)
    (folder / ".zshrc").write_text(f'export PATH="{bin_dir}:$PATH"\n')
    env = {k: v for k, v in os.environ.items() if not k.startswith(("CMUX_", "CONVERSATIONS_", "XDG_", "ZDOTDIR", "GHOSTTY_"))}
    env.update(HOME=str(folder), SHELL="/bin/zsh", PATH="/usr/bin:/bin",
               CMUX_WORKSPACE_ID=workspace, CMUX_SURFACE_ID=surface,
               CMUX_BUNDLED_CLI_PATH=str(fake_cmux), CONVERSATIONS_HOST_SETTINGS_FILE=str(settings))
    if args.managed:
        env.update(CONVERSATIONS_SESSION_ID=ident, CONVERSATIONS_CAPTURE="0")
    master, slave = os.openpty()
    try:
        proc = subprocess.run([str(args.cli.resolve()), "--server", url, "host", "--runtime-dir", str(runtime),
                               "shell", "--", "/bin/sh", "-c", "exit 0"],
                              env=env, stdin=slave, stdout=slave, stderr=subprocess.PIPE, timeout=15)
        assert proc.returncode == 0, proc.stderr.decode()
        if args.missing_native:
            assert not result.exists(), "invalid native identity was launched"
            assert json.loads((config / "open-sessions.json").read_text())["servers"][url][0]["session_id"] == ident
            print("PASS: invalid restore keeps its checkpoint and falls back to a usable shell")
            sys.exit(0)
        assert result.exists(), "saved Codex session was replaced with an ordinary shell"
        assert json.loads(result.read_text()) == {
            "args": ["--no-alt-screen", "resume", native, "two words", "", "$(literal)"],
            "cwd": str(folder), "id": ident,
        }, result.read_text()
        result.unlink()
        # A new tab inherits workspace env, but must not resume its sibling's
        # conversation. Only that original surface's binding owns the identity.
        ordinary = subprocess.run([str(args.cli.resolve()), '--server', url, 'host', '--runtime-dir', str(runtime),
                                   'shell', '--', '/bin/sh', '-c', 'exit 0'],
                                  env={**env, 'TEST_NO_BINDING':'1', 'CONVERSATIONS_SESSION_ID':ident,
                                       'CONVERSATIONS_CAPTURE':'0'},
                                  stdin=slave, stdout=slave, stderr=subprocess.PIPE, timeout=15)
        assert ordinary.returncode == 0, ordinary.stderr.decode()
        assert not result.exists(), 'new tab resumed its sibling conversation'
    finally:
        os.close(master)
        os.close(slave)
        listener.close()
        for connection in connections:
            connection.close()
    # A daemon starting beside an existing restored surface must not open a
    # second workspace for its saved native conversation.
    calls.write_text("")
    log = folder / "daemon.log"
    with log.open('w') as output:
        host = subprocess.Popen([str(args.cli.resolve()), '--server', url, 'host',
                                 '--runtime-dir', str(folder / 'second-host'), 'run', '--backend', 'cmux'],
                                env={**env, **({"TEST_FAIL_INSPECTION":"1"} if args.fail_inspection else {}), **({"TEST_TRANSIENT_INSPECTION":"1"} if args.transient_inspection else {})}, stdout=output, stderr=output)
        try:
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline:
                observed = [json.loads(line) for line in calls.read_text().splitlines()]
                assert not any('workspace.create' in call for call in observed), 'duplicate saved workspace launched'
                if any(('resume' if args.fail_inspection else 'surface.read_text') in call for call in observed):
                    time.sleep(1)
                    break
                time.sleep(.05)
            else:
                raise AssertionError('daemon did not inspect restored surfaces: ' + log.read_text())
            observed = [json.loads(line) for line in calls.read_text().splitlines()]
            assert not any('workspace.create' in call for call in observed), 'duplicate saved workspace launched'
        finally:
            host.terminate()
            host.wait(timeout=5)
    # A persisted host workspace must restart the daemon instead of becoming
    # another ordinary Local: zsh pane after app restart.
    control_env = {**env, "CONVERSATIONS_HOST_CONTROL": "1", "CONVERSATIONS_CAPTURE": "0"}
    control_env.pop("CONVERSATIONS_SESSION_ID", None)
    control_dir = folder / 'control-host'
    if args.host_binding_only:
        control_env.pop("CONVERSATIONS_HOST_CONTROL", None)
        control_env["TEST_HOST_CHECKPOINT"] = "host:" + str(control_dir)
    with log.open('a') as output:
        host = subprocess.Popen([str(args.cli.resolve()), '--server', url, 'host',
                                 '--runtime-dir', str(control_dir), 'shell', '--', '/bin/sh', '-c', 'exit 0'],
                                env=control_env, stdout=output, stderr=output)
        try:
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                if (control_dir / 'host.sock').exists():
                    break
                assert host.poll() is None, 'restored host pane exited instead of running a daemon'
                time.sleep(.05)
            else:
                raise AssertionError('restored host pane did not start its daemon')
            duplicate = subprocess.run([str(args.cli.resolve()), '--server', url, 'host',
                '--runtime-dir', str(control_dir), 'shell', '--', '/bin/sh', '-c', 'exit 0'],
                env={**control_env, 'TEST_NO_BINDING':'1'}, capture_output=True, timeout=10)
            assert duplicate.returncode == 0, duplicate.stderr.decode()
            assert host.poll() is None, 'existing daemon was displaced'

        finally:
            host.terminate()
            host.wait(timeout=5)
print("PASS: restored session identity, native resume target, interactive PATH, cwd and literal argv")
print("PASS: daemon startup restores only existing saved surfaces, leaving orphaned checkpoints dormant")
print("PASS: restored control pane restarts the daemon")
print("PASS: new tabs do not inherit a sibling conversation identity")
