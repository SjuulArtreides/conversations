#!/usr/bin/env python3
"""Verify shared order against an explicitly supplied isolated localhost cmux host.

Only sessions created by this run are moved or ended. Native permutations keep
all other workspace slots unchanged. --keep leaves probe sessions for browser QA.
"""
import argparse
import json
from pathlib import Path
import subprocess
import time
import urllib.error
import urllib.request

CMUX = "/Applications/cmux.app/Contents/Resources/bin/cmux"
ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--server", required=True)
    parser.add_argument("--cookie-file", required=True, type=Path)
    parser.add_argument("--runtime-dir", required=True, type=Path)
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    if not args.server.startswith("http://127.0.0.1:"):
        parser.error("requires an isolated 127.0.0.1 server")
    cookie = args.cookie_file.read_text().strip()

    def http(path, data=None, method=None):
        request = urllib.request.Request(args.server + path,
            data=None if data is None else json.dumps(data).encode(),
            headers={"Content-Type": "application/json", "Cookie": cookie}, method=method)
        with urllib.request.urlopen(request, timeout=5) as response:
            return json.load(response)

    def native(*argv):
        result = subprocess.run([CMUX, "--json", "--id-format", "uuids", *argv],
            check=True, capture_output=True, text=True, timeout=8)
        return json.loads(result.stdout)

    def status():
        result = subprocess.run([str(ROOT / "target/debug/conversations"), "host",
            "--runtime-dir", str(args.runtime_dir), "status", "--json"],
            check=True, capture_output=True, text=True, timeout=5)
        return json.loads(result.stdout)["sessions"]

    def wait(predicate, label):
        deadline = time.monotonic() + 25
        while time.monotonic() < deadline:
            value = predicate()
            if value:
                return value
            time.sleep(.2)
        raise AssertionError(f"timed out: {label}")

    initial = {session["launch"]["id"] for session in status()}
    created = []
    try:
        for _ in range(3):
            assert http("/api/live/start-new", {"agent": "zsh"})["delivered"]
            session = wait(lambda: next((s for s in status() if s["launch"]["id"] not in initial and s["launch"]["id"] not in created), None), "new wrapper")
            created.append(session["launch"]["id"])
        mapping = {s["workspace"].lower(): s["launch"]["id"] for s in status() if s["launch"]["id"] in created}
        # Registration can insert new native workspaces beside the selected
        # workspace. Let append synchronization settle before testing a gesture.
        all_mapping = {s["workspace"].lower(): s["launch"]["id"] for s in status()}
        def registration_settled():
            actual = [all_mapping[s["id"].lower()] for w in native("tree", "--all")["windows"]
                      for s in w["workspaces"] if s["id"].lower() in all_mapping]
            return [s for s in http("/api/live/order")["session_ids"] if s in all_mapping.values()] == actual
        wait(registration_settled, "registration append order settles")
        time.sleep(2)
        window = next(w for w in native("tree", "--all")["windows"]
                      if all(any(s["id"].lower() == workspace for s in w["workspaces"]) for workspace in mapping))
        slots = [s["id"].lower() for s in window["workspaces"]]
        native_initial = [mapping[s] for s in slots if s in mapping]
        order = wait(lambda: (value if (value := http("/api/live/order"))["revision"] else None), "initial persisted native order")
        desired = list(reversed(native_initial))
        order = http("/api/live/order", {"revision": order["revision"], "session_ids": desired}, "PUT")
        def current_slots():
            return [s["id"].lower() for w in native("tree", "--all")["windows"] if w["id"] == window["id"] for s in w["workspaces"]]
        wait(lambda: [mapping[s] for s in current_slots() if s in mapping] == desired, "web reorder reaches native cmux")
        for index, ident in enumerate(slots):
            if ident not in mapping:
                assert current_slots()[index] == ident, "unmanaged workspace moved"
        # Native move back, with the full original permutation preserving outsiders.
        native("reorder-workspaces", "--window", window["id"], "--order", ",".join(slots))
        wait(lambda: [s for s in http("/api/live/order")["session_ids"] if s in created] == native_initial,
             "native reorder reaches shared web order")
        try:
            http("/api/live/order", {"revision": order["revision"], "session_ids": desired}, "PUT")
            raise AssertionError("stale revision accepted")
        except urllib.error.HTTPError as error:
            assert error.code == 409
        revision = http("/api/live/order")["revision"]
        time.sleep(5)
        assert http("/api/live/order")["revision"] == revision, "polling caused feedback writes"
        # A completely constrained pair swap performs no native mutation, but
        # must still normalize the shared order once, without a feedback loop.
        pinned_workspace = next(w for w, ident in mapping.items() if ident == native_initial[0])
        try:
            native("workspace-action", "--workspace", pinned_workspace, "--action", "pin")
            def native_managed():
                return [mapping[s] for s in current_slots() if s in mapping]
            wait(lambda: [s for s in http("/api/live/order")["session_ids"] if s in created] == native_managed(), "pin move observed")
            time.sleep(3)
            expected = native_managed()
            current = http("/api/live/order")
            accepted = http("/api/live/order", {"revision":current["revision"], "session_ids":[expected[1], expected[0]]}, "PUT")
            wait(lambda: [s for s in http("/api/live/order")["session_ids"] if s in created] == expected, "constrained pair canonicalized")
            revision = http("/api/live/order")["revision"]
            assert revision == accepted["revision"] + 1
            time.sleep(5)
            assert http("/api/live/order")["revision"] == revision, "pin projection oscillated"
        finally:
            native("workspace-action", "--workspace", pinned_workspace, "--action", "unpin")
        print("PASS: web → cmux → web, stale CAS rejection, unrelated slots, stable polling, constrained pinned pair")
        print(json.dumps({"server": args.server, "sessions": created, "revision": revision}))
    finally:
        if not args.keep:
            for ident in created:
                http(f"/api/live/{ident}/complete", {})


if __name__ == "__main__":
    main()
