"""Behaviour tests for probe-terminal-files.py using harmless dummy agents.

  python3 scripts/test_probe_terminal_files.py
"""
import argparse
import importlib.util
import os
import shutil
import socket
import stat
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    "probe", os.path.join(os.path.dirname(os.path.abspath(__file__)), "probe-terminal-files.py")
)
probe = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(probe)

SEQ = {"gap_ms": 10, "pastes": ["61"]}
AGENT = '#!/bin/sh\nstty raw -echo\nhead -c1 >/dev/null\necho "[Image #1]"\nsleep 1\n'


class RunProbeTest(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.mkdtemp(prefix="probe-test-")
        self.work = os.path.join(self.root, "work")
        self.home = os.path.join(self.work, "home")
        os.makedirs(self.home)
        self.cwd = os.getcwd()
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(1)

    def tearDown(self):
        os.chdir(self.cwd)
        self.listener.close()
        shutil.rmtree(self.root, ignore_errors=True)

    def agent(self, name, body=AGENT):
        path = os.path.join(self.root, name)
        with open(path, "w") as f:
            f.write(body)
        os.chmod(path, os.stat(path).st_mode | stat.S_IXUSR)
        return path

    def run_probe(self, binary, seq=SEQ):
        args = argparse.Namespace(
            agent="opencode", binary=binary, gap_ms=None, edge=False, startup=0.5
        )
        return probe.run_probe(args, seq, self.work, self.home, self.listener, [])

    def test_relative_binary_with_slash(self):
        self.agent("agent.sh")
        os.chdir(self.root)
        result, _ = self.run_probe("./agent.sh")
        self.assertEqual(result["image_placeholders"], ["[Image #1]"])

    def test_path_command(self):
        self.agent("dummy-agent")
        old = os.environ["PATH"]
        os.environ["PATH"] = self.root + os.pathsep + old
        try:
            result, _ = self.run_probe("dummy-agent")
        finally:
            os.environ["PATH"] = old
        self.assertEqual(result["image_placeholders"], ["[Image #1]"])

    def test_path_command_via_relative_path_entry(self):
        os.mkdir(os.path.join(self.root, "relative-bin"))
        self.agent("relative-bin/rel-agent")
        os.chdir(self.root)
        old = os.environ["PATH"]
        os.environ["PATH"] = "relative-bin" + os.pathsep + old
        try:
            result, _ = self.run_probe("rel-agent")
        finally:
            os.environ["PATH"] = old
        self.assertEqual(result["image_placeholders"], ["[Image #1]"])

    def test_missing_binary_fails_before_fork(self):
        with self.assertRaises(SystemExit) as cm:
            self.run_probe(os.path.join(self.root, "missing"))
        self.assertIn("not found", str(cm.exception))

    def test_exec_failure_is_child_only_and_reported(self):
        bad = self.agent("bad.sh", "#!/nonexistent/interpreter\n")
        with self.assertRaises(SystemExit) as cm:
            self.run_probe(bad)
        self.assertIn("failed to start", str(cm.exception))
        self.assertIn("cannot exec", str(cm.exception))
        # Parent state survives: the child's exception must not run parent cleanup.
        self.assertTrue(os.path.isdir(self.work))
        self.assertGreaterEqual(self.listener.fileno(), 0)

    def test_exec_failure_without_pastes_is_not_success(self):
        bad = self.agent("bad.sh", "#!/nonexistent/interpreter\n")
        with self.assertRaises(SystemExit) as cm:
            self.run_probe(bad, {"gap_ms": 10, "pastes": []})
        self.assertIn("failed to start", str(cm.exception))


if __name__ == "__main__":
    unittest.main()
