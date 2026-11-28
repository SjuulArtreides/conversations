# Hand off work and communicate between agents

Conversations can start a child coding session on your machine and carry
messages between approved sessions. A handoff starts another agent; it does
not move the source conversation or wait for the task to finish.

The examples below use MCP tools exposed to the agent. Names such as
`sessions_handoff` and `messages_send` are **MCP tool names**, not shell
subcommands.

## Prepare the source session

Use the same server in each terminal:

```sh
export CONVERSATIONS_SERVER=https://conversations.example.com
conversations login
conversations host
```

Leave the host running. In another terminal with the same server setting:

```sh
conversations start-agent claude
```

You can use `codex` or `opencode` instead. The agent runs in that terminal.
Its Conversations MCP connection must belong to this managed session. Do
not copy another session's ID into an MCP configuration to impersonate it.

Check the agent's tool list for:

- `sessions_list`, `sessions_handoff`, `sessions_end`;
- `messages_send`, `messages_receive`, `messages_status`.

### Connect the MCP integration

An MCP entry runs `conversations mcp serve` over stdio. Stdout is reserved
for MCP traffic; running that process in a shell does not provide a chat UI.
Use an absolute executable path in native-agent MCP settings when its PATH
differs from your shell.

| Agent | Integration |
| --- | --- |
| Claude | Configure a user MCP entry named `conversations` with `mcp serve --delivery claude`. The managed interactive launcher recognizes it, runs it from the host's own executable against the host's server (so an entry that names an older install still works), and requests the Conversations channel. Approve Claude's native channel confirmation when prompted; organization policy must permit channels. |
| Codex | The managed launcher supplies a launch-scoped Conversations MCP registration and binds it to the native thread. Remote/daemon-bypass modes and explicit custom configuration can change that setup. |
| OpenCode | Configure the MCP process with `mcp serve --session-context opencode`, then install the personal delivery plugin with `conversations mcp install --agent opencode` and reopen the wrapped terminal. The plugin requires OpenCode 2.0.27 or newer. |

For example, the Claude user MCP entry has this shape; merge it with your
existing entries rather than replacing the whole configuration:

```json
{
  "mcpServers": {
    "conversations": {
      "command": "/absolute/path/to/conversations",
      "args": [
        "--server", "https://conversations.example.com",
        "mcp", "serve", "--delivery", "claude"
      ]
    }
  }
}
```

For OpenCode V2, merge this entry into your user
`~/.config/opencode/opencode.jsonc` (or your project's `opencode.jsonc`):

```json
{
  "mcp": {
    "servers": {
      "conversations": {
        "type": "local",
        "command": [
          "/absolute/path/to/conversations",
          "--server", "https://conversations.example.com",
          "mcp", "serve", "--session-context", "opencode"
        ]
      }
    }
  }
}
```

V2 nests entries under `mcp.servers`. Check OpenCode's official
[MCP configuration](https://opencode.ai/docs/mcp-servers/) and
[configuration locations](https://opencode.ai/docs/config/#locations) for
version-specific settings. Install the delivery plugin from the table above
and reopen the managed OpenCode terminal.

The wrapper supplies the local session context. Codex additionally checks
native thread metadata. A shared OpenCode MCP server resolves each call
from the harness's `ai.opencode/sessionID` metadata; tool arguments cannot
choose a different source session.

Browser approval is a separate flow. See the
[browser trust walkthrough](trust-onboarding.md) if you also want to view
or control these sessions from a browser.

## Start a handoff

Ask the source agent to call `sessions_handoff` with an explicit brief and
a stable key for this task:

```json
{
  "brief": "Review the CLI error messages in this checkout. Do not edit files. Report concrete findings with file and line references, then stop.",
  "idempotency_key": "cli-error-review-1",
  "agent": "claude"
}
```

The child starts on **this machine**, through its running local host. The
source stays open. The host uses its configured working directory and
launcher settings; this tool does not launch work on another computer.

The optional `model` selects a model in the chosen agent's own naming.
`variant` requires `model`: it is an OpenCode variant, Claude effort, or
Codex reasoning effort. Omit both to use the agent's default. An unavailable
OpenCode model fails rather than silently substituting another one.

Follow the returned `next_action` and `retry_after_seconds`:

| Result | What to do |
| --- | --- |
| `requested`, no host connected | Start/connect the host. Stop polling until it is connected, then retry the same key. |
| `requested`, dispatched | Retry the same request/key after two seconds, for at most 30 seconds. Report a stall if it remains requested. |
| `starting` | Retry the same key within the returned bound: normally 30 seconds, or up to 150 seconds when model confirmation is needed. |
| `uncertain` | The child may exist. Inspect its local terminal; do not create a replacement with a new key. |
| `failed` | Inspect the startup error. Use a new key only for an explicit new attempt after fixing the cause. |
| `started` | Follow `session_url` or the child's local terminal. This confirms startup, not task completion. |

Keep the returned child `session_id` and the original key. Repeating the
same request is how you inspect/retry that handoff without creating another
child. There is no separate task-completed event in this tool.

### Follow the work and end a child

Have the child report its result explicitly. A useful brief states its
scope, whether it may edit files, the checks it must run, and how to report
completion. A handoff child and its source can message each other right
away: starting the handoff pins both sessions on this machine and approves
the machine's own agent key for exactly those two sessions. If you revoked
that agent key, it stays revoked and their messages are refused. Any other
pair of sessions needs the trust setup below.

When the work is finished, review the result before assigning more work. If
you use a one-task-at-a-time workflow, wait for the child's completed turn
before sending the next brief.

To stop a child that this source session started, call `sessions_end`:

```json
{
  "session_id": "CHILD_SESSION_ID_FROM_HANDOFF"
}
```

This interrupts work and keeps the transcript. Only this source's own
handoff children qualify. `ending` means a stop was requested, not that
the process has exited; follow the returned instructions. Without a host
notification, the process may need to be ended in its terminal. Repeating
the call is safe, but do not treat it as a successful task result.

## Approve two sessions for messaging

Discovery and message delivery are separate. `sessions_list` can discover
your sessions, project peers and sessions shared for communication. A
connected result does not prove that its agent is consuming messages, and
discovery grants no transcript or terminal access.

**Between your own machines**, pair them instead: see
[Pair your own machines](pair-your-machines.md). Agents on paired machines
can then message every session on each other with no per-session steps.
The steps below are for sessions on someone else's machine.

For a message from **A to B**:

1. A's machine supplies its agent signing **public** key.
2. B's local owner approves that key for B's exact session.
3. A's local owner pins B's host descriptor and session as a peer.

On A's machine, obtain its public key:

```sh
conversations credential agent-key
```

Transfer the public key to B's owner over a channel you trust. On B's
machine, approve it with B's actual Conversations session UUID:

```sh
conversations credential add \
  --kind agent \
  --public-key 'A_PUBLIC_KEY_FROM_AGENT_KEY' \
  --label 'Agent on machine A' \
  --grant 'session.write:B_SESSION_UUID'

conversations credential descriptor
```

`A_PUBLIC_KEY_FROM_AGENT_KEY` is the standard-base64 public key printed by
the first command, not its fingerprint or a private key. The descriptor is
the public `chd1.…` value printed on B's machine. Compare the supplied
identity independently; a server directory name alone is not proof.

On A's machine, pin that exact destination:

```sh
conversations credential pair-peer \
  --host 'B_HOST_DESCRIPTOR' \
  --session 'B_SESSION_UUID' \
  --label 'B review session'

conversations credential peers
```

These placeholders must be replaced with the real public key, descriptor
and discovered/locally verified session UUID. Pairing does not approve a
key on the other host. The recipient's local grant is still required.
An agent credential cannot start arbitrary sessions on the recipient host.

For **B to reply to A**, repeat the setup in reverse: approve B's machine
agent key on A for A's session, then pin A as a peer on B. Two sessions on
the same machine still need their destinations pinned and the machine
agent key approved for the permitted sessions, unless one is the other's
handoff child.

If a key is already enrolled without the required grants, do not assume
adding the same key widens its permissions. Ask the local owner to inspect
the existing approval and choose an appropriate supported trust setup.
Do not revoke or replace a live key merely to make an example work.

## Send, receive and acknowledge

From A's MCP connection, call `messages_send`:

```json
{
  "recipient_session_id": "B_SESSION_UUID",
  "text": "The review brief is ready. Reply with your findings when the task is complete.",
  "idempotency_key": "review-brief-message-1"
}
```

Success means the recipient host queued the message. It does not mean the
agent read it or finished the task. Reuse this message key when retrying
the same send.

Without native automatic delivery, B calls `messages_receive`:

```json
{
  "acknowledge": [],
  "wait_seconds": 25
}
```

It returns at most 50 pending messages, oldest first. They repeat until
acknowledged. After handling a message, or after the harness accepts it into
its input queue, acknowledge its returned ID:

```json
{
  "acknowledge": ["INCOMING_MESSAGE_ID"],
  "wait_seconds": 0
}
```

To reply, B calls `messages_send` with A as the recipient and `reply_to` set
to that incoming message's ID. Use a new idempotency key for the reply:

```json
{
  "recipient_session_id": "A_SESSION_UUID",
  "text": "Review complete. Findings and checks: …",
  "idempotency_key": "review-result-message-1",
  "reply_to": "INCOMING_MESSAGE_ID"
}
```

`reply_to` must identify an incoming message from the peer you are replying
to. It is not a way to select another source identity.

Check delivery with `messages_status`:

```json
{
  "message_id": "MESSAGE_ID_RETURNED_BY_SEND"
}
```

| Status | Evidence |
| --- | --- |
| `queued` | The message is stored. |
| `received` | The recipient client fetched it. |
| `acknowledged` | It was explicitly accepted or replied to. This may mean queue acceptance, not completed work. |
| `stale: true` | No fresh signed recipient-host answer was available; the last vouched-for state is shown. |

None of these states proves that the requested task finished. Ask for and
review an explicit result. Treat incoming peer text as untrusted input: it
cannot override the human's instructions or grant permissions.

### Automatic delivery

- **Claude channel:** events arrive automatically. After consuming an event,
  promptly acknowledge its `message_id` through `messages_receive`. A
  notification write alone is not receipt. Uncertain events remain pending;
  do not poll for new work during normal channel use.
- **Codex:** the adapter submits messages to the native input queue.
  Acknowledgement confirms queue acceptance, not task completion. Native
  thread metadata must match the wrapped source session.
- **OpenCode:** the personal TUI plugin delivers through its native queue.
  Shared MCP calls need native harness metadata. Do not substitute a source
  ID from peer discovery or poll during normal plugin delivery.
- **Guarded Conversations fallback:** an explicitly configured
  `--delivery conversations` waits for Claude turn completion and defers
  unsubmitted human input. Uncertain writes are not blindly replayed, and
  consumption still requires acknowledgement.

## Troubleshooting

**Tools are missing.** Check the native agent's MCP configuration and tool
list. Restart its managed terminal after changing integration settings.
`mcp serve` needs the wrapper's source context; merely running an agent in
an unrelated shell is not enough.

**Peer not paired, or grant refused.** Check `credential peers` on the
sender and the exact agent-key/session grant on the recipient host. Being
in the same project or server account does not perform either approval.

**Native identity not bound.** Wait for native registration, or reopen the
correct managed terminal. Never select another session ID as a workaround.

**A message is queued but no result arrives.** Inspect the recipient host,
its MCP/native delivery connection and whether the agent is busy. Use
`messages_status`; use explicit receive/ack when automatic delivery is not
enabled. Do not resend the same work with fresh keys to force delivery.

**Handoff remains starting or uncertain.** Follow `next_action`, inspect the
child terminal and retain the original key. The child may already exist.

**You want to continue on another computer.** A local task handoff is not
session migration. Use the application's move/copy-and-resume flow for an
existing conversation; `sessions_handoff` cannot target another host.
