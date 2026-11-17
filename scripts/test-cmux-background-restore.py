#!/usr/bin/env python3
"""Native regression: restore dormant panes without focus, input, or duplicates.

Run inside cmux. Uses a private HOME and host, imports four disposable panes,
and never restarts the app or a user's host. Requires the normal shell adapter.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--cli', type=Path, default=ROOT / 'target/release/conversations')
args = parser.parse_args()
CMUX = os.environ.get('CMUX_BUNDLED_CLI_PATH', '/Applications/cmux.app/Contents/Resources/bin/cmux')


def rpc(method, params):
    result = subprocess.run([CMUX, 'rpc', method, json.dumps(params)], capture_output=True, text=True, timeout=10)
    assert result.returncode == 0, f'{method}: {result.stderr}'
    return json.loads(result.stdout)


def tree():
    return json.loads(subprocess.check_output([CMUX, '--json', '--id-format', 'uuids', 'tree', '--all'], text=True, timeout=10))


def selected():
    return [(w['id'], ws['id']) for w in tree()['windows'] for ws in w['workspaces'] if ws['selected']]


def runtimes():
    return {t['surface_id']: t for t in rpc('debug.terminals', {})['terminals']}


assert os.environ.get('CMUX_WORKSPACE_ID'), 'Run this test inside cmux'
created = []
original = selected()
(ROOT / "tmp").mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(prefix='background-restore-', dir=ROOT / 'tmp') as scratch:
    folder = Path(scratch)
    # Keep the Unix socket pathname below macOS's 104-byte limit.
    runtime = folder / 'h'
    config = folder / ('Library/Application Support' if sys.platform == 'darwin' else '.config') / 'arrakis-conversations'
    config.mkdir(parents=True)
    url = 'http://127.0.0.1:1'
    saved = []
    host = None
    try:
        snapshot_source = Path.home() / 'Library/Application Support/cmux/session-com.cmuxterm.app.json'
        version = json.loads(snapshot_source.read_text())['version']
        workspaces = []
        for index in range(4):
            panel, workspace = str(uuid.uuid4()), str(uuid.uuid4())
            terminal = {'workingDirectory': str(folder), 'isRemoteTerminal': False, 'hasReceivedExplicitInput': False}
            if index in (1, 2):
                ident = str(uuid.uuid4())
                saved.append({'session_id': ident, 'agent': 'codex', 'cwd': str(folder), 'resume_id': str(uuid.uuid4())})
                terminal['resumeBinding'] = {'kind': 'conversations', 'checkpointId': ident,
                    'command': '/usr/bin/true', 'cwd': str(folder), 'source': 'conversations',
                    'autoResume': False, 'approvalPolicy': 'manual', 'updatedAt': time.time()}
            workspaces.append({'workspaceId': workspace, 'stableId': str(uuid.uuid4()),
                'currentDirectory': str(folder), 'customTitle': f'Restore regression {index}',
                'isPinned': False, 'hasUnreadIndicator': False, 'isManuallyUnread': False, 'isMuted': False, 'layoutMode': 'splits', 'processTitle': 'Regression', 'environment': {}, 'focusedPanelId': panel,
                'layout': {'type': 'pane', 'pane': {'panelIds': [panel], 'selectedPanelId': panel}},
                'panels': [{'id': panel, 'stableSurfaceId': str(uuid.uuid4()), 'type': 'terminal',
                            'title': f'Restore regression {index}', 'directory': str(folder),
                            'isPinned': False, 'hasUnreadIndicator': False, 'isManuallyUnread': False, 'listeningPorts': [], 'directoryIsTrustedRemoteReport': False, 'terminal': terminal}],
                'statusEntries': [], 'logEntries': []})
        fixture = folder / 'restore.json'
        fixture.write_text(json.dumps({'version': version, 'createdAt': time.time(), 'windows': [{
            'windowId': str(uuid.uuid4()), 'tabManager': {'selectedWorkspaceIndex': 0, 'workspaces': workspaces},
            'sidebar': {'isVisible': True, 'selection': 'tabs', 'width': 240}, 'frame': {'x': 100, 'y': 100, 'width': 800, 'height': 600}}]}))
        before_windows = {w['id'] for w in tree()['windows']}
        result = subprocess.run([CMUX, 'restore-session', '--from', str(fixture)], capture_output=True, text=True, timeout=30)
        assert result.returncode == 0, result.stderr
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            created.clear()
            for window in tree()['windows']:
                if window['id'] not in before_windows:
                    for ws in window['workspaces']:
                        surface = ws['panes'][0]['surfaces'][0]
                        created.append({'workspace_id': ws['id'], 'surface_id': surface['id'], 'title': ws['title']})
            if len(created) == 4:
                break
            time.sleep(.1)
        assert len(created) == 4, 'restore fixture window missing'
        control = created.pop(0)
        original = selected()
        assert all(not runtimes()[p['surface_id']]['runtime_surface_ready'] for p in created), 'fixture did not restore dormant panes'
        (config / 'open-sessions.json').write_text(json.dumps({'servers': {url: saved}}))
        env = {k: v for k, v in os.environ.items() if not k.startswith(('CONVERSATIONS_', 'XDG_'))}
        env['HOME'] = str(folder)
        env['CMUX_BUNDLED_CLI_PATH'] = CMUX
        before_ids = {ws['id'] for w in tree()['windows'] for ws in w['workspaces']}
        started_at = None
        with (folder / 'host.log').open('w') as output:
            for attempt in range(2):
                host = subprocess.Popen([str(args.cli.resolve()), '--server', url, 'host', '--runtime-dir', str(runtime),
                                         'run', '--backend', 'cmux'], env=env, stdout=output, stderr=output)
                deadline = time.monotonic() + 12
                while time.monotonic() < deadline:
                    assert host.poll() is None, 'isolated host exited'
                    if all(runtimes()[p['surface_id']]['runtime_surface_ready'] for p in created[:2]):
                        break
                    time.sleep(.2)
                else:
                    raise AssertionError('saved panes stayed dormant until selection')
                assert not runtimes()[created[2]['surface_id']]['runtime_surface_ready'], 'unrelated pane was started'
                assert selected() == original, 'restore stole focus'
                after_ids = {ws['id'] for w in tree()['windows'] for ws in w['workspaces']}
                assert after_ids == before_ids, 'restore created duplicate workspaces'
                # A second host startup must not replace an already-running shell.
                screen = [runtimes()[p['surface_id']]['runtime_surface_created_at'] for p in created[:2]]
                if started_at is not None:
                    assert screen == started_at, 'host restart replaced a running terminal'
                started_at = screen
                time.sleep(.5)
                assert [runtimes()[p['surface_id']]['runtime_surface_created_at'] for p in created[:2]] == screen, 'restore replaced a terminal runtime'
                host.terminate()
                host.wait(timeout=5)
                host = None
        print('PASS: two dormant saved panes start without focus changes; unrelated pane stays dormant; no duplicate workspaces on host restart')
    finally:
        if host is not None:
            host.terminate()
            host.wait(timeout=5)
        for pane in reversed(created + ([control] if 'control' in locals() else [])):
            rpc('workspace.close', {'workspace_id': pane['workspace_id'], 'force': True})
