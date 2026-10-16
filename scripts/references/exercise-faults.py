#!/usr/bin/env python3
"""固定隔离实例的可重试故障矩阵；Docker CLI 故障由包装器返回，关闭超时真实等待 60 秒。"""
import json
import os
import pathlib
import sqlite3
import subprocess
import time
import uuid

ROOT = pathlib.Path('/mnt/docker_nvme/mai-references-acceptance/20261015-shared-sources')
SOCKET = '/run/mai-reference-acceptance-docker/docker.sock'
assert os.geteuid() == 0
assert os.stat(SOCKET).st_ino != os.stat('/var/run/docker.sock').st_ino
ENV = {**os.environ, 'DOCKER_HOST': 'unix://' + SOCKET}
TARGET = str(uuid.uuid5(uuid.UUID('9d5c7e6a-c1b2-4f3a-9e77-a1b2c3d4e5f6'), 'agent:a-maintainer'))
OUT = ROOT / 'evidence/fault-matrix'
OUT.mkdir(parents=True, exist_ok=True)
CONTROL = ROOT / 'fault-control.json'
BASE = [str(ROOT / 'bin/shared_references_probe'), '--provision', 'all',
        '--docker-binary', str(ROOT / 'bin/docker-fault'),
        '--agent-image', 'sha256:46efd2ec0c7102db5df390e1aef998c7de4666d9db3f7e5dfbfbcae532d30e5b',
        '--sidecar-image', 'sha256:19cdf22873afa204799c7fe69dbee33fa3be86512245b1b67a889dc334bd62bb']
RESULTS = []


def database():
    return sqlite3.connect('file:' + str(ROOT / 'data/mai-team.sqlite3') + '?mode=ro', uri=True)


def execute(name, args):
    start = time.monotonic()
    with (OUT / (name + '.log')).open('wb') as output:
        result = subprocess.run(BASE + args, env=ENV, stdout=output, stderr=subprocess.STDOUT, timeout=1200)
    row = {'step': name, 'exit_code': result.returncode, 'seconds': round(time.monotonic() - start, 2)}
    RESULTS.append(row)
    (OUT / 'results.json').write_text(json.dumps(RESULTS, indent=2))
    print(json.dumps(row), flush=True)
    return result.returncode


def assert_stopped(container):
    result = subprocess.run(['docker', 'inspect', '-f', '{{.State.Running}}', container],
                            env=ENV, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    assert result.returncode != 0 or result.stdout.strip() == b'false', (container, '仍在运行')


for index, mode in enumerate(('docker_offline', 'enospc', 'mcp_failure', 'close_timeout')):
    CONTROL.write_text(json.dumps({'mode': mode, 'agent_id': TARGET}))
    args = ['clear'] if index % 2 == 0 else ['--scenario', 'bind', 'apply']
    execute(mode, args)
    with database() as db:
        row = db.execute('SELECT phase, old_container_id, candidate_container_id, error_json FROM reference_apply_operations WHERE agent_id=? ORDER BY created_at DESC,id DESC LIMIT 1', (TARGET,)).fetchone()
        assert row and row[0] == 'failed', (mode, row)
        for container in row[1:3]:
            if container:
                assert_stopped(container)
        assert row[3] and ('environment_' in row[3] or 'docker' in row[3].lower()), row
        (OUT / (mode + '-receipt.json')).write_text(json.dumps({'phase': row[0], 'old_container_id': row[1], 'candidate_container_id': row[2], 'failure': json.loads(row[3])}, indent=2))
    CONTROL.unlink()
    assert execute(mode + '-operator-retry', ['recover']) == 0
    with database() as db:
        latest = db.execute('SELECT phase FROM reference_apply_operations WHERE agent_id=? ORDER BY created_at DESC,id DESC LIMIT 1', (TARGET,)).fetchone()
        assert latest == ('succeeded',), latest

assert execute('final-bind', ['--scenario', 'bind', 'apply']) == 0
print('FAULT_MATRIX_PASS', flush=True)
