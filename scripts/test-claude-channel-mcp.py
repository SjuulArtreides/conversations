#!/usr/bin/env python3
"""Test channel transport against a real local signed inbox, without Claude credentials."""
import argparse
from contextlib import closing
import importlib.util
import json
import os
import tempfile
from pathlib import Path
import select
import subprocess
import sqlite3
import uuid

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("identity_fixture", Path(__file__).with_name("test-codex-mcp-identity.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cli', type=Path, default=Path(os.environ.get('CARGO_TARGET_DIR', ROOT / 'target')) / 'debug/conversations')
    args = parser.parse_args()
    (ROOT / 'tmp').mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='claude-channel-', dir=ROOT / 'tmp') as folder:
        run_fixture(args, Path(folder))


def run_fixture(args, scratch):
    with (scratch / 'fixture.log').open('w+') as log:
        with closing(fixture.LocalFixture(args.cli, scratch, log)) as test:
            run_connected(args, scratch, test, log)


def run_connected(args, scratch, test, log):
    environment = dict(test.host.environment, CONVERSATIONS_CLAUDE_DELIVERY_STATE_DIR=str(scratch / 'state'))
    unsupported = subprocess.run([str(args.cli), 'mcp', 'install', '--agent', 'claude'],
        capture_output=True, text=True, env=environment)
    assert unsupported.returncode != 0 and 'only opencode' in unsupported.stderr, unsupported.stderr
    source, sender = (str(uuid.uuid4()) for _ in range(2))
    test.host.register(source, agent='claude')
    test.host.register(sender, agent='claude')
    test.host.approve_messages([source])
    outgoing = fixture.fixture.Mcp(args.cli, test.origin, sender, log, env=environment)
    try:
        message = outgoing.tool('messages_send', dict(recipient_session_id=source,
            text='A peer message', idempotency_key='channel-proof'))
    finally:
        outgoing.close()
    def acknowledged():
        with sqlite3.connect(test.host.runtime / 'authz.db') as db:
            return db.execute('SELECT state FROM agent_inbox WHERE message_id=?', (message['id'],)).fetchone()[0] == 'acknowledged'
    origin = test.origin
    def launch():
        child = subprocess.Popen([str(args.cli), '--server', origin, 'mcp', 'serve', '--session', source, '--delivery', 'claude'],
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1, env=environment)
        def send(value): child.stdin.write(json.dumps(dict(jsonrpc='2.0', **value))+'\n'); child.stdin.flush()
        def read(timeout=10):
            assert select.select([child.stdout], [], [], timeout)[0], 'channel output timed out'
            line = child.stdout.readline()
            assert line, child.stderr.read()
            return json.loads(line)
        send(dict(id=1, method='initialize', params=dict(protocolVersion='2025-11-25', capabilities={}, clientInfo=dict(name='channel-test', version='1'))))
        info = read()['result']
        assert info['capabilities']['experimental'] == {'claude/channel': {}}, info
        send(dict(method='notifications/initialized'))
        return child, send, read
    child = None
    try:
        child, send, read = launch()
        pushed = read()
        assert pushed['method'] == 'notifications/claude/channel', pushed
        assert pushed['params']['meta']['message_id'] == message['id'], pushed
        assert not acknowledged(), 'transport delivery prematurely acknowledged message'
        assert any(path.is_file() for path in (scratch / 'state').rglob('*')), 'fixture ledger escaped its private state directory'
        child.stdin.close(); child.wait(timeout=5); child = None
        child, send, read = launch()
        assert not select.select([child.stdout], [], [], 2.2)[0], 'uncertain notification repeated after restart'
        assert not acknowledged(), 'restart prematurely acknowledged message'
        # Receipt means a model explicitly reports the ID, not a successful transport write.
        send(dict(id=2, method='tools/call', params=dict(name='messages_receive', arguments=dict(acknowledge=[message['id']]))))
        assert read()['id'] == 2
        assert acknowledged()
        child.stdin.close(); child.wait(timeout=5); child = None
        print('PASS: attributed channel notification; no transport ACK; explicit receipt ACK')
    finally:
        if child is not None: child.kill(); child.wait()


if __name__ == '__main__': main()
