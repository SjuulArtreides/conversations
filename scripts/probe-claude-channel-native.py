#!/usr/bin/env python3
"""Run an isolated interactive Claude channel probe with a local inbox fixture.

Run in a disposable terminal. Read and approve Claude's channel development
confirmation yourself. This script never answers it or changes saved config.
After Claude is ready, POST the reported /probe/enqueue URL from another terminal.
GET /probe/status to see receipt state. Exit Claude normally when finished.
"""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import subprocess
import threading
import uuid

ROOT = Path(__file__).resolve().parents[1]

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cli', type=Path, default=ROOT / 'target/debug/conversations')
    parser.add_argument('--claude', default='claude')
    parser.add_argument('--scratch', type=Path, required=True, help='Owned directory under repo tmp for probe metadata')
    args = parser.parse_args()
    args.scratch.resolve().relative_to((ROOT / 'tmp').resolve())
    args.scratch.mkdir(parents=True, exist_ok=True)
    source, native, message_id = (str(uuid.uuid4()) for _ in range(3))
    state = dict(enqueued=False, acknowledged=False, polls=0, acknowledged_ids=[])
    message = dict(id=message_id, project_id=str(uuid.uuid4()), sender_session_id=str(uuid.uuid4()),
                   sender_owner_email='channel-probe@example.test', recipient_session_id=source,
                   text='Native channel receipt test. Confirm receipt of this message ID using messages_receive acknowledge, then respond CLAUDE_CHANNEL_PROBE_OK. Do not run commands or edit files.',
                   reply_to=None, idempotency_key='native-channel-proof', created_at='2026-10-21T00:00:00Z',
                   received_at=None, acknowledged_at=None, status='received')
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_): pass
        def reply(self, value):
            self.send_response(200); self.send_header('Content-Type', 'application/json'); self.end_headers()
            self.wfile.write(json.dumps(value).encode())
        def do_GET(self): self.reply(state)
        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers.get('Content-Length', 0))) or b'{}')
            if self.path == '/probe/enqueue':
                if state['enqueued'] and state['acknowledged']:
                    message['id'] = str(uuid.uuid4())
                    state['acknowledged'] = False
                state['enqueued'] = True
                self.reply(dict(state, message_id=message['id'])); return
            state['polls'] += 1
            if message['id'] in body.get('acknowledge', []):
                state['acknowledged'] = True
                if message['id'] not in state['acknowledged_ids']: state['acknowledged_ids'].append(message['id'])
            self.reply(dict(messages=[message] if state['enqueued'] and not state['acknowledged'] else []))
    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    origin = f'http://127.0.0.1:{server.server_port}'
    config = dict(mcpServers=dict(conversations=dict(command=str(args.cli.resolve()),
        args=['--server', origin, 'mcp', 'serve', '--session', source, '--delivery', 'claude'])))
    metadata = dict(origin=origin, source=source, native_session=native, message_id=message_id)
    (args.scratch / 'probe.json').write_text(json.dumps(metadata, indent=2)+'\n')
    print(json.dumps(metadata, indent=2), flush=True)
    print(f'After native startup confirmation: curl -X POST {origin}/probe/enqueue', flush=True)
    print(f'Receipt evidence: curl {origin}/probe/status', flush=True)
    try:
        result = subprocess.run([args.claude, '--session-id', native, '--mcp-config', json.dumps(config),
            '--strict-mcp-config', '--dangerously-load-development-channels', 'server:conversations', '--',
            'This is a disposable native channel test. Wait for a channel event without polling. When it arrives, acknowledge its message_id using messages_receive, then follow its simple response request. Do not run shell commands or edit files.'])
        (args.scratch / 'result.json').write_text(json.dumps(state, indent=2)+'\n')
        return result.returncode
    finally: server.shutdown()

if __name__ == '__main__': raise SystemExit(main())
