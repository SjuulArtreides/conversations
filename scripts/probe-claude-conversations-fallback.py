#!/usr/bin/env python3
"""Run a disposable local server, host, and real interactive Claude fallback session.

No saved agent config or login is modified. Run inside an owned terminal, inspect
and approve normal native MCP permissions yourself. The first idle message can
be sent using probe.json without entering a ready prompt. For draft/busy proofs,
type a draft or start a harmless long reply, send a peer message, and inspect the
actual inbox/transcript. Exit Claude normally to stop the owned host and server.
"""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import time
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cli', type=Path, default=ROOT / 'target/debug/arrakis-conversations')
    parser.add_argument('--server', type=Path, default=ROOT / 'target/debug/conversations-server')
    parser.add_argument('--claude', default='claude')
    parser.add_argument('--scratch', type=Path, required=True)
    args = parser.parse_args()
    args.scratch = args.scratch.resolve()
    args.scratch.relative_to((ROOT / 'tmp').resolve())
    args.scratch.mkdir(parents=True, exist_ok=True)
    runtime = args.scratch / 'host'
    runtime.mkdir(mode=0o700, exist_ok=True)
    source, peer = str(uuid.uuid4()), str(uuid.uuid4())
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0)); port = sock.getsockname()[1]
    origin = f'http://127.0.0.1:{port}'
    environment = {k: v for k, v in os.environ.items() if not k.startswith(('CONVERSATIONS_', 'GOOGLE_', 'OIDC_', 'CMUX_'))}
    environment.update(BIND_ADDR=f'127.0.0.1:{port}', DATABASE_URL=f'sqlite://{args.scratch}/server.sqlite',
                       AUTH_ENABLED='false', SHUTDOWN_DRAIN_SECS='0', LIVE_REAP_INTERVAL_SECS='0')
    def http(method, path, body=None):
        request = urllib.request.Request(origin+path, method=method, data=None if body is None else json.dumps(body).encode(), headers={'Content-Type':'application/json'})
        with urllib.request.urlopen(request, timeout=5) as response: return json.load(response)
    server = host = None
    with (args.scratch / 'server.log').open('w') as server_log, (args.scratch / 'host.log').open('w') as host_log:
        try:
            server = subprocess.Popen([str(args.server.resolve())], env=environment, stdout=server_log, stderr=server_log)
            for _ in range(100):
                try: http('GET','/healthz'); break
                except OSError: time.sleep(.1)
            else: raise RuntimeError('local server failed readiness')
            project = http('POST','/api/projects',{'name':'Disposable guarded Claude fallback proof'})['id']
            for session in (source, peer):
                http('POST','/api/live',{'session_id':session,'title': 'Fallback native proof' if session == source else 'Local proof peer','agent':'claude'})
                http('PUT',f'/api/sessions/{session}/project',{'project_id':project})
            host = subprocess.Popen([str(args.cli.resolve()), '--server', origin, 'host', '--runtime-dir', str(runtime), 'run'],
                                    env=environment, stdout=host_log, stderr=host_log)
            for _ in range(100):
                if (runtime/'host.sock').exists(): break
                time.sleep(.1)
            else: raise RuntimeError('local host failed readiness')
            metadata = dict(origin=origin, source=source, peer=peer, native_session=source, project=project, runtime=str(runtime))
            (args.scratch/'probe.json').write_text(json.dumps(metadata, indent=2)+'\n')
            print(json.dumps(metadata, indent=2), flush=True)
            config = dict(mcpServers=dict(conversations=dict(command=str(args.cli.resolve()), args=['--server',origin,'mcp','serve','--delivery','conversations'])))
            command = [str(args.cli.resolve()), '--server',origin,'host','--runtime-dir',str(runtime),'wrap','--session-id',source,
                       '--agent','claude','--cwd',str(args.scratch),'--',args.claude,'--session-id',source,
                       '--mcp-config',json.dumps(config),'--strict-mcp-config','--',
                       'This is a disposable peer-delivery test. Reply FALLBACK_BASE_READY and wait. Do not poll messages_receive for new work. When a peer message arrives automatically, use messages_receive to acknowledge its message ID, then follow its simple response instruction. Never run commands or edit files.']
            return subprocess.run(command, env=environment).returncode
        finally:
            for child in (host, server):
                if child is not None:
                    child.terminate()
                    try: child.wait(timeout=8)
                    except subprocess.TimeoutExpired: child.kill(); child.wait()

if __name__ == '__main__': raise SystemExit(main())
