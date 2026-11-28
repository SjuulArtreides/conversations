"""Check PR commit subjects and the workspace version in the proposed merge."""

import re
import subprocess
import sys
import tomllib

HEADER = re.compile(r"([a-z][a-z0-9-]*)(\([^()\s]+\))?(!)?: (\S.*)")
BREAKING = re.compile(r"^BREAKING[ -]CHANGE: \S", re.MULTILINE)


def git(*args):
    return subprocess.check_output(["git", *args], text=True)


def read_toml(ref, path):
    return tomllib.loads(git("show", f"{ref}:{path}"))


def check(base, head, merged):
    errors = []
    major, minor, patch = map(
        int, read_toml(base, "Cargo.toml")["workspace"]["package"]["version"].split(".")
    )
    minimum = (major, minor, patch)
    for commit in git("rev-list", "--no-merges", f"{base}..{head}").splitlines():
        subject, body = git("show", "-s", "--format=%s%x00%b", commit).split("\0", 1)
        header = HEADER.fullmatch(subject)
        if not header:
            errors.append(f"{commit[:7]}: use a Conventional Commit subject, e.g. 'fix: describe the change'.")
            continue
        kind, _, breaking, _ = header.groups()
        if breaking or BREAKING.search(body):
            bump = (major + 1, 0, 0) if major else (0, minor + 1, 0)
        elif kind == "feat":
            bump = (major, minor + 1, 0)
        elif kind in {"fix", "perf"}:
            bump = (major, minor, patch + 1)
        else:
            continue
        minimum = max(minimum, bump)

    workspace = read_toml(merged, "Cargo.toml")["workspace"]["package"]
    version = workspace["version"]
    required = ".".join(map(str, minimum))
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
        errors.append(f"Workspace version {version} must use major.minor.patch.")
    elif tuple(map(int, version.split("."))) < minimum:
        errors.append(
            f"Workspace version {version} must be at least {required}. "
            "Bump Cargo.toml locally and refresh Cargo.lock with Cargo."
        )

    locked = read_toml(merged, "Cargo.lock")["package"]
    for crate in ("conversations-cli", "conversations-core", "conversations-server"):
        if not any(
            entry["name"] == crate and entry["version"] == version and "source" not in entry
            for entry in locked
        ):
            errors.append(f"Cargo.lock must record {crate} at workspace version {version}.")

    return errors


if __name__ == "__main__":
    problems = check(*sys.argv[1:])
    for problem in problems:
        print(problem, file=sys.stderr)
    sys.exit(bool(problems))
