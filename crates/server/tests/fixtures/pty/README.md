# Sanitized terminal captures

Each JSONL record contains terminal geometry and base64-encoded PTY bytes:

```json
{"cols": 100, "rows": 30, "b64": "<base64 raw PTY bytes>", "kind": "output"}
```

The streams originate from non-sensitive count prompts or an interactive
startup with no submitted prompt. Paths and session identifiers have been
replaced with examples for publication. Escape sequences and frame boundaries
are preserved.

- `codex_count_200.jsonl` and `claude_count_200.jsonl` contain a dummy count task.
- `codex_interactive_reflow.jsonl` contains interactive startup and resizes. The
  CLI's `codex_capture_replaces_startup_chrome_in_host_history` unit test uses it.

The historical standalone server and browser replay harnesses are absent from
this source revision. These files do not imply that those tests run in CI.

To capture a new non-sensitive startup stream, use an isolated directory that
the agent already trusts:

```sh
python3 scripts/capture-agent-pty.py path/to/new-fixture.jsonl -- codex
```

Review decoded terminal output before committing any new capture. It can
contain local paths, identifiers, prompts, credentials, or private output.
