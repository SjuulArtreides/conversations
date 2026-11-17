#!/usr/bin/env python3
"""Verify installation/rollback against scratch configuration, never real settings."""
import argparse
import os
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("--target-dir", type=Path, default=root / "target")
args = parser.parse_args()
(root / "tmp").mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(prefix="cmux-installer-", dir=root / "tmp") as scratch:
    folder = Path(scratch)
    config = folder / "ghostty/config"
    config.parent.mkdir()
    original = "# User configuration\nfont-size = 14\n"
    config.write_text(original)
    cli = args.target_dir.resolve() / "debug/arrakis-conversations"
    runtime = folder / "host"
    settings_file = folder / "local-host.json"
    env_base = {**os.environ, "CONVERSATIONS_HOST_SETTINGS_FILE": str(settings_file)}
    command = [str(cli), "host", "--runtime-dir", str(runtime),
        "install-cmux", "--config-file", str(config)]
    def run(*extra, success=True):
        result = subprocess.run([*command, *extra], env=env_base, capture_output=True, timeout=10)
        assert (result.returncode == 0) == success, result.stderr.decode()
    run()
    installed = config.read_text()
    assert installed.startswith(original)
    assert installed.count("command =") == 1
    assert not settings_file.exists(), "install should preserve disabled preference"
    assert any(p.read_text() == original for p in config.parent.glob("*.bak"))
    run()
    assert config.read_text() == installed, "Repeated install must be idempotent"
    run("--enable")
    assert '"auto_wrap": true' in settings_file.read_text()
    # Ghostty still starts the user's shell after a scratch runtime and the
    # installed CLI binary are removed. The startup script lives beside config.
    shell = folder / "original-shell"
    marker = folder / "shell-started"
    shell.write_text("#!/bin/sh\nprintf '%s' \"$*\" > " + str(marker) + "\n")
    shell.chmod(0o700)
    run("--uninstall")
    env = {**env_base, "SHELL": str(shell)}
    subprocess.run([*command, "--enable"], env=env, check=True, capture_output=True)
    launcher = config.parent / "conversations/cmux-shell.sh"
    assert launcher.exists() and not str(launcher).startswith(str(runtime))
    if runtime.exists():
        for item in runtime.iterdir():
            item.unlink()
        runtime.rmdir()
    import re
    script = launcher.read_text()
    launcher.write_text(re.sub(r"^binary=.*$", "binary='" + str(folder / "missing-cli") + "'", script, flags=re.M))
    subprocess.run([str(launcher)], env={**env, "CMUX_WORKSPACE_ID": "missing-runtime"}, check=True, timeout=5)
    assert marker.read_text() == "-l", "missing CLI must fall back to the original shell"
    marker.unlink()
    old_binary = folder / "old-cli"
    old_binary.write_text("#!/bin/sh\nexit 2\n")
    old_binary.chmod(0o700)
    launcher.write_text(re.sub(r"^binary=.*$", "binary='" + str(old_binary) + "'", launcher.read_text(), flags=re.M))
    subprocess.run([str(launcher)], env={**env, "CMUX_WORKSPACE_ID": "old-cli"}, check=True, timeout=5)
    assert marker.read_text() == "-l", "older executable without host must fall back to the original shell"
    marker.unlink()
    run("--uninstall")
    assert config.read_text() == original, "Rollback must preserve existing settings"
    config.write_text("command = /bin/fish\n")
    run(success=False)
    assert config.read_text() == "command = /bin/fish\n"
    config.unlink()
    source = folder / "managed-config"
    source.write_text(original)
    config.symlink_to(source)
    run(success=False)
    assert config.is_symlink() and source.read_text() == original
print("PASS: native installer preserves settings, stable fallback shell, idempotence, rollback, and custom config safety")
