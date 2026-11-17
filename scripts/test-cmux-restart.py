# /// script
# requires-python = ">=3.11"
# dependencies = ["websockets>=14,<16"]
# ///
"""Real server/host/PTY restart contract; only cmux and the agent are simulated.

The agent reports its native identity through cmux's resume metadata, just as
SessionStart does. Restart must resume that identity, keep the Conversations ID,
and save enough state for an app restart. HOME and all test state are isolated.
"""

import argparse
import asyncio
import importlib.util
import json
import os
from pathlib import Path
import secrets
import shlex
import signal
import subprocess
import sys
import time
import uuid

sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parents[1]


def fake():
    folder = Path(os.environ["CMUX_TEST_DIR"])
    args = sys.argv[2:]
    if sys.argv[1] == "--shell-agent":
        with (folder / "shell-launches.jsonl").open("a") as out:
            out.write(json.dumps({"id": os.environ.get("CONVERSATIONS_WRAPPED"),
                                  "cwd": os.getcwd(), "pid": os.getpid()}) + "\n")
        print("SHELL_READY", flush=True)
        time.sleep(120)
    elif sys.argv[1] == "--agent":
        native = (
            args[args.index("resume") + 1] if "resume" in args else str(uuid.uuid4())
        )
        if "resume" in args:
            assert os.environ.get("TEST_CMUX_HOOKS") != "1", (
                "resume changed Codex daemon configuration"
            )
            assert native == (folder / "native").read_text(), (
                "wrong native resume target"
            )
        else:
            assert os.environ.get("TEST_CMUX_HOOKS") == "1", (
                "fresh session omitted native identity hooks"
            )
            (folder / "native").write_text(native)
        (folder / "binding.json").write_text(
            json.dumps({"kind": "codex", "checkpoint_id": native})
        )
        with (folder / "launches.jsonl").open("a") as out:
            out.write(
                json.dumps({"native": native, "args": args, "pid": os.getpid(), "size": list(os.get_terminal_size())}) + "\n"
            )
        print("AGENT_READY", flush=True)
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline and not (folder / "agent-exit").exists():
            time.sleep(0.05)
        sys.exit(143)
    elif sys.argv[1] == "--terminal":
        import select

        master, slave = os.openpty()
        import fcntl, struct, termios
        background = (folder / "launches.jsonl").exists()
        cols, rows = (99, 35) if background else (132, 42)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        child = subprocess.Popen(
            ["/bin/sh", "-c", args[0]],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            start_new_session=True,
        )
        os.close(slave)
        (folder / "terminal-pid").write_text(str(child.pid))
        try:
            while child.poll() is None:
                if select.select([master], [], [], 0.1)[0]:
                    try:
                        data = os.read(master, 65536)
                        with (folder / "terminal.log").open("ab") as out:
                            out.write(data)
                    except OSError:
                        break
        finally:
            if child.poll() is None:
                child.terminate()
            child.wait(timeout=5)
            os.close(master)
    else:
        if (folder / "cmux-offline").exists():
            sys.exit(1)
        surface = "11111111-1111-4111-8111-111111111111"
        workspace = "22222222-2222-4222-8222-222222222222"
        if "tree" in args:
            print(
                json.dumps(
                    {
                        "windows": [
                            {
                                "id": "55555555-5555-4555-8555-555555555555",
                                "workspaces": [
                                    {"id": "33333333-3333-4333-8333-333333333333", "panes": []},
                                    {
                                        "id": workspace,
                                        "panes": [{"surfaces": [{"id": surface}]}],
                                    }
                                ]
                            }
                        ]
                    }
                )
            )
        elif "resume" in args:
            binding = folder / "binding.json"
            if "set" in args:
                if os.environ.get("CMUX_TEST_FAIL_BIND") == "1":
                    sys.exit(1)
                binding.write_text(
                    json.dumps(
                        {
                            "kind": args[args.index("--kind") + 1],
                            "checkpoint_id": args[args.index("--checkpoint") + 1],
                        }
                    )
                )
            print(
                json.dumps(
                    {
                        "resume_binding": json.loads(binding.read_text())
                        if binding.exists()
                        else None
                    }
                )
            )
        elif args[:2] == ["rpc", "debug.terminals"]:
            print(json.dumps({"terminals": [{"surface_id": surface,
                "hosted_view_in_headless_bootstrap_window": False,
                "hosted_view_visible_in_ui": not (folder / "launches.jsonl").exists()}]}))
        elif "list" in args:
            print(
                json.dumps(
                    {"workspaces": [{"id": workspace, "custom_title": "Local: codex"}]}
                )
            )
        elif args[:2] == ["rpc", "workspace.create"]:
            params = json.loads(args[2])
            assert params["window_id"] == "55555555-5555-4555-8555-555555555555"
            (folder / "workspace.json").write_text(json.dumps(params))
            with (folder / "workspace-creates.jsonl").open("a") as out:
                out.write(json.dumps(params) + "\n")
            env = {
                **os.environ,
                **params["workspace_env"],
                "CMUX_SURFACE_ID": surface,
                "CMUX_WORKSPACE_ID": workspace,
            }
            subprocess.Popen(
                [sys.executable, __file__, "--terminal", params["initial_command"]],
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
            print(json.dumps({"workspace_id": workspace, "surface_id": surface}))
        elif args[:2] == ["rpc", "surface.read_text"] and (folder / "dormant").exists():
            # A restored cmux pane starts its saved shell adapter on first read.
            params = json.loads((folder / "workspace.json").read_text())
            env = {**os.environ, **params["workspace_env"],
                   "CMUX_SURFACE_ID": surface, "CMUX_WORKSPACE_ID": workspace}
            command = shlex.join([
                os.environ["CMUX_TEST_CLI"], "--server", os.environ["CMUX_TEST_SERVER"],
                "host", "--runtime-dir", str(folder / "host"), "shell",
            ])
            (folder / "dormant").unlink()
            subprocess.Popen([sys.executable, __file__, "--terminal", command],
                             env=env, stdin=subprocess.DEVNULL,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                             start_new_session=True)
            print("{}")
        else:
            print("{}")


async def test():
    import tempfile

    spec = importlib.util.spec_from_file_location(
        "host_test", ROOT / "scripts/test-local-host.py"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--cli", type=Path, default=ROOT / "target/release/arrakis-conversations"
    )
    parser.add_argument(
        "--server", type=Path, default=ROOT / "target/debug/conversations-server"
    )
    parser.add_argument("--fail-bind", action="store_true")
    parser.add_argument("--agent-exit", action="store_true")
    scenarios = parser.add_mutually_exclusive_group()
    scenarios.add_argument("--explicit-resume-start", action="store_true",
                           help="host start with explicit Codex resume argv must checkpoint its native identity")
    scenarios.add_argument("--host-survives-app-restart", action="store_true",
                           help="a surviving host must resume dormant panes after cmux becomes available again")
    scenarios.add_argument("--shell-reboot", action="store_true",
                           help="automatically wrapped shells keep their identity across two cold host/pane restarts")
    args = parser.parse_args()
    (ROOT / "tmp").mkdir(exist_ok=True)
    folder = Path(tempfile.mkdtemp(prefix="cmux-restart-", dir=ROOT / "tmp"))
    t = module.Test(ROOT / "target", folder)
    t.cli, t.server = args.cli.resolve(), args.server.resolve()
    home = folder / "home"
    home.mkdir()
    bins = folder / "bin"
    bins.mkdir()
    for name, mode in [("cmux", "--cmux"), ("codex", "--agent"), ("zsh", "--shell-agent")]:
        path = bins / name
        path.write_text(
            "#!/bin/sh\n" + ('if [ "$1" = "-ilc" ]; then exec /bin/zsh "$@"; fi\n' if name == "zsh" else '') + "exec "
            + shlex.join([sys.executable, str(Path(__file__).resolve()), mode])
            + ' "$@"\n'
        )
        path.chmod(0o700)
    shims = folder / "shims"
    shims.mkdir()
    shim = shims / "codex"
    shim.write_text(
        "#!/bin/sh\nexport TEST_CMUX_HOOKS=1\nexec "
        + shlex.quote(str(bins / "codex"))
        + ' "$@"\n'
    )
    shim.chmod(0o700)
    (home / ".zshrc").write_text(f'export PATH="{bins}:$PATH"\n')
    settings = folder / "settings.json"
    settings.write_text('{"cmux":{"auto_wrap":true}}')
    t.environment.update(
        HOME=str(home),
        XDG_CONFIG_HOME=str(home / ".config"),
        SHELL="/bin/zsh",
        CMUX_TEST_DIR=str(folder),
        CMUX_TEST_CLI=str(t.cli),
        CMUX_TEST_SERVER=t.url,
        CONVERSATIONS_HOST_SETTINGS_FILE=str(settings),
        CMUX_TEST_FAIL_BIND="1" if args.fail_bind else "0",
        CMUX_AGENT_COMMAND_SHIM_ROOT=str(shims),
        CMUX_WORKSPACE_ID="33333333-3333-4333-8333-333333333333",
        CMUX_SURFACE_ID="44444444-4444-4444-8444-444444444444",
        CMUX_BUNDLED_CLI_PATH=str(bins / "cmux"),
        PATH=str(bins) + ":" + os.environ["PATH"],
        BIND_ADDR=t.url.removeprefix("http://"),
        DATABASE_URL=f"sqlite://{folder}/test.db?mode=rwc",
        AUTH_ENABLED="true",
        PASSWORD_AUTH_ENABLED="true",
        COOKIE_SECURE="false",
        SHUTDOWN_DRAIN_SECS="0",
    )
    ident = None
    try:
        t.spawn([str(t.server)], "server")
        await t.wait(lambda: t.http("/healthz"), "server ready")
        t.http(
            "/auth/register",
            {"email": "restart@example.test", "password": secrets.token_urlsafe(24)},
        )
        cookie = folder / "cookie"
        cookie.write_text(t.cookie)
        cookie.chmod(0o600)
        host = t.spawn(
            [
                str(t.cli),
                "--server",
                t.url,
                "host",
                "--runtime-dir",
                str(t.host_dir),
                "--backend",
                "cmux",
                "--cookie-file",
                str(cookie),
            ],
            "host",
        )
        await t.wait(
            lambda: t.host_online(),
            "host relay connected",
        )
        if args.shell_reboot:
            t.environment["SHELL"] = str(bins / "zsh")
            # Start the same automatic shell adapter that cmux invokes on new
            # panes, without a server launch or pre-existing checkpoint.
            command = shlex.join([str(t.cli), "--server", t.url, "host",
                                  "--runtime-dir", str(t.host_dir), "shell",
                                  "--", str(bins / "zsh")])
            params = {"workspace_env": {"CONVERSATIONS_CAPTURE": "1"}}
            (folder / "workspace.json").write_text(json.dumps(params))
            host_env = t.environment.copy()
            t.environment.update(CMUX_SURFACE_ID="11111111-1111-4111-8111-111111111111",
                                 CMUX_WORKSPACE_ID="22222222-2222-4222-8222-222222222222")
            t.spawn([sys.executable, str(Path(__file__).resolve()), "--terminal", command], "shell-terminal")
            t.environment = host_env
            await t.wait(lambda: t.status(), "automatic shell registered")
            ident = t.status()[0]["launch"]["id"]
            launches = folder / "shell-launches.jsonl"
            config = home / ("Library/Application Support" if sys.platform == "darwin" else ".config") / "arrakis-conversations/open-sessions.json"
            await t.wait(lambda: config.exists() and any(s["session_id"] == ident for s in json.loads(config.read_text())["servers"].get(t.url, [])),
                         "automatic shell missing its reboot checkpoint")
            await t.wait(lambda: (folder / "binding.json").exists(), "shell surface binding")
            for attempt in range(2):
                (folder / "cmux-offline").touch()
                os.kill(int((folder / "terminal-pid").read_text()), signal.SIGTERM)
                await t.wait(lambda: not t.status(), "app quit disconnected shell")
                host.terminate()
                host.wait(timeout=5)
                (folder / "dormant").touch()
                (folder / "cmux-offline").unlink()
                host = t.spawn(host.args, f"reboot-host-{attempt}")
                await t.wait(lambda: launches.exists() and len(launches.read_text().splitlines()) == attempt + 2,
                             "saved shell did not resume")
                await t.wait(lambda: len(t.status()) == 1 and t.status()[0]["launch"]["id"] == ident,
                             "shell identity changed after reboot")
                live = t.http("/api/live")["live_sessions"]
                assert [s["id"] for s in live] == [ident], "reboot created extra live shell tabs"
                assert not (folder / "workspace-creates.jsonl").exists(), "reboot created another workspace"
            records = [json.loads(line) for line in launches.read_text().splitlines()]
            assert {r["id"] for r in records} == {ident}, records
            assert {r["cwd"] for r in records} == {records[0]["cwd"]}, records
            assert len({r["pid"] for r in records}) == 3, records
            t.http(f"/api/live/{ident}/complete", {})
            await t.wait(lambda: not t.status(), "explicit End stopped restored shell")
            await t.wait(lambda: not json.loads(config.read_text())["servers"].get(t.url), "explicit End retired shell checkpoint")
            print("PASS: automatic shell keeps one Conversations ID and tab across two cold restarts; End retires it")
            return
        if args.explicit_resume_start:
            (folder / "native").write_text(str(uuid.uuid4()))
            subprocess.run([str(t.cli), "--server", t.url, "host", "--runtime-dir",
                            str(t.host_dir), "start", "--agent", "codex", "--cwd", str(home),
                            "--", str(bins / "codex"), "--no-alt-screen", "resume",
                            (folder / "native").read_text()],
                           env=t.environment, capture_output=True, check=True, timeout=15)
        else:
            assert t.http("/api/live/start-new", {"agent": "codex"})["delivered"]
        await t.wait(lambda: t.status(), "new agent registered")
        ident = t.status()[0]["launch"]["id"]
        await t.wait(lambda: (folder / "native").exists(), "agent native identity")
        native = (folder / "native").read_text()
        config = (
            home
            / ("Library/Application Support" if sys.platform == "darwin" else ".config")
            / "arrakis-conversations/open-sessions.json"
        )
        if args.explicit_resume_start:
            await t.wait(lambda: (folder / "launches.jsonl").exists(), "explicit resume agent started")
            launch = json.loads((folder / "launches.jsonl").read_text().splitlines()[0])
            assert launch["native"] == native and "resume" in launch["args"], launch
            await t.wait(
                lambda: config.exists() and any(s["session_id"] == ident and s.get("resume_id") == native
                    for s in json.loads(config.read_text())["servers"].get(t.url, [])),
                "explicit host start failed to checkpoint the running agent's native resume identity",
            )
            print("PASS: explicit Codex resume launch persists its native checkpoint")
            return
        await t.wait(
            lambda: t.http(f"/api/sessions/{ident}").get("agent_session_id") == native,
            "native agent identity published to Conversations",
        )
        saved = json.loads(config.read_text())["servers"][t.url]
        assert any(
            s["session_id"] == ident and s["resume_id"] == native for s in saved
        ), "new session not checkpointed"
        if args.host_survives_app_restart:
            # cmux disappears while the daemon survives; wrapper shutdown must
            # preserve the already-proven native checkpoint.
            (folder / "cmux-offline").touch()
            os.kill(int((folder / "terminal-pid").read_text()), signal.SIGTERM)
            await t.wait(lambda: not t.status(), "app quit disconnected its wrapper")
            saved = json.loads(config.read_text())["servers"].get(t.url, [])
            assert any(s["session_id"] == ident and s["resume_id"] == native for s in saved), "app quit erased checkpoint"
            (folder / "dormant").touch()
            (folder / "cmux-offline").unlink()
            inventory = subprocess.run([str(bins / "cmux"), "--json", "tree", "--all"],
                                       env=t.environment, capture_output=True, check=True, text=True)
            assert "11111111-1111-4111-8111-111111111111" in inventory.stdout
            assert host.poll() is None, "host did not survive app quit"
            failure = None
            try:
                await t.wait(
                    lambda: len((folder / "launches.jsonl").read_text().splitlines()) == 2,
                    "surviving host did not resume the saved dormant pane after cmux restarted",
                )
            except AssertionError as error:
                # Prove the same pane, checkpoint and shell adapter work when
                # the host restarts. Keep the original failure after this control.
                failure = error
                host.terminate()
                host.wait(timeout=5)
                host = t.spawn(host.args, "restarted-host")
                await t.wait(
                    lambda: len((folder / "launches.jsonl").read_text().splitlines()) == 2,
                    "harness control: restarting the host must recover the same pane",
                )
            launches = [json.loads(line) for line in (folder / "launches.jsonl").read_text().splitlines()]
            assert launches[1]["native"] == native and "resume" in launches[1]["args"], launches
            await t.wait(lambda: any(s["launch"]["id"] == ident for s in t.status()), "same conversation reconnected")
            assert not (folder / "dormant").exists(), "saved pane was not awakened"
            assert len((folder / "workspace-creates.jsonl").read_text().splitlines()) == 1, "recovery created a duplicate workspace"
            if failure:
                print("CONTROL PASS: restarting only the host resumed the same native conversation")
                raise failure
            print("PASS: surviving host resumes the same dormant conversation without a duplicate workspace")
            return
        t.http(f"/api/live/{ident}/complete", {})
        assert t.http(f"/api/sessions/{ident}/reopen", {})["delivered"]
        await t.wait(
            lambda: len((folder / "launches.jsonl").read_text().splitlines()) == 2,
            "replacement agent started",
        )
        launches = [
            json.loads(line)
            for line in (folder / "launches.jsonl").read_text().splitlines()
        ]
        assert launches[0]["size"] == [132, 42], launches
        assert launches[1]["size"] == [132, 42], f"restart lost last visible terminal size: {launches}"
        assert launches[0]["pid"] != launches[1]["pid"]
        assert "resume" in launches[1]["args"] and launches[1]["native"] == native
        await t.wait(
            lambda: any(s["launch"]["id"] == ident for s in t.status()),
            "same Conversations identity restored",
        )
        if args.agent_exit:
            (folder / "agent-exit").touch()
            await t.wait(
                lambda: not json.loads(config.read_text())["servers"].get(t.url),
                "normal agent exit 143 retires checkpoint",
            )
            print("PASS: ordinary agent exit 143 is not mistaken for app shutdown")
            return
        # App quit signals the wrapper; it must leave a restart checkpoint.
        os.kill(int((folder / "terminal-pid").read_text()), signal.SIGTERM)
        await t.wait(lambda: not t.status(), "app-quit wrapper disconnected")
        await asyncio.sleep(1)
        saved = json.loads(config.read_text())["servers"].get(t.url, [])
        assert any(s["session_id"] == ident for s in saved), (
            "app quit erased the resume checkpoint"
        )
        t.http(f"/api/live/{ident}/complete", {})
        await t.wait(
            lambda: not json.loads(config.read_text())["servers"].get(t.url),
            "explicit End removes checkpoint",
        )
        print(
            "PASS: fresh native identity published and persisted; web restart resumes the same thread; app quit preserves and explicit End removes the checkpoint"
        )
    finally:
        # The fake terminal is launched by cmux, outside t.children.
        # Stop its real PTY wrapper even if a red assertion interrupted the test.
        if (folder / "terminal-pid").exists():
            try:
                os.kill(int((folder / "terminal-pid").read_text()), signal.SIGTERM)
            except ProcessLookupError:
                pass
        if ident:
            try:
                t.http(f"/api/live/{ident}/complete", {})
            except Exception:
                pass
        for process in reversed(t.children):
            if process.poll() is None:
                process.terminate()
        for process in reversed(t.children):
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
        print("Evidence:", folder)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] in ("--agent", "--shell-agent", "--cmux", "--terminal"):
        fake()
    else:
        asyncio.run(test())
