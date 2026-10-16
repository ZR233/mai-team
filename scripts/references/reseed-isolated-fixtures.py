#!/usr/bin/env python3
"""保全发布边界失败现场，验证 T33 后重新建立独立验收夹具；拒绝生产 daemon。"""
import json
import os
import pathlib
import shutil
import sqlite3
import subprocess
import uuid

ROOT = pathlib.Path('/mnt/docker_nvme/mai-references-acceptance/20261015-shared-sources')
SOCKET = '/run/mai-reference-acceptance-docker/docker.sock'
assert os.geteuid() == 0
assert os.stat(SOCKET).st_ino != os.stat('/var/run/docker.sock').st_ino
ENV = {**os.environ, 'DOCKER_HOST': 'unix://' + SOCKET}
OUT = ROOT / 'evidence/published-supersession-red'
OUT.mkdir(parents=True, exist_ok=True)
NAMESPACE = uuid.UUID('9d5c7e6a-c1b2-4f3a-9e77-a1b2c3d4e5f6')
AGENTS = {str(uuid.uuid5(NAMESPACE, 'agent:' + name)) for name in (
    'a-maintainer', 'a-planner', 'a-explorer', 'a-reviewer', 'a-child',
    'a-grandchild', 'b-executor', 'nonproject-executor')}
PROJECTS = {str(uuid.uuid5(NAMESPACE, 'project:' + name)) for name in ('project-a', 'project-b')}
TARGET = str(uuid.uuid5(NAMESPACE, 'agent:a-grandchild'))
BASE = [str(ROOT / 'bin/shared_references_probe'), '--provision', 'all',
        '--agent-image', 'sha256:46efd2ec0c7102db5df390e1aef998c7de4666d9db3f7e5dfbfbcae532d30e5b',
        '--sidecar-image', 'sha256:19cdf22873afa204799c7fe69dbee33fa3be86512245b1b67a889dc334bd62bb']


def docker(*args, check=True):
    return subprocess.run(['docker', *args], env=ENV, check=check,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def run(name, arguments, expected):
    with (OUT / (name + '.json')).open('wb') as output:
        code = subprocess.run(BASE + arguments, env=ENV, stdout=output,
                              stderr=subprocess.STDOUT, timeout=1200).returncode
    print(json.dumps({'step': name, 'exit_code': code}), flush=True)
    assert code == expected, (name, code)


archive = ROOT / 'data-published-supersession-red'
assert not archive.exists(), '已有现场归档，拒绝覆盖'
with sqlite3.connect(ROOT / 'data/mai-team.sqlite3') as database:
    database.row_factory = sqlite3.Row
    binding = dict(database.execute(
        'SELECT * FROM agent_reference_bindings WHERE agent_id=?', (TARGET,)).fetchone())
    assert docker('inspect', binding['container_id'], check=False).returncode != 0
    result_path = OUT / 't33-result.json'
    recorded = result_path.exists()
    image = binding['effective_image_id']
    details = json.loads(docker('image', 'inspect', image).stdout)[0]
    (OUT / 'missing-old-binding.json').write_text(json.dumps(binding, indent=2))
    if not recorded:
        docker('image', 'save', '--output', str(OUT / 'missing-old-snapshot.tar'), image)
    tags = details.get('RepoTags') or []
    assert all(tag.startswith('mai-team-snapshot-') for tag in tags), tags
    if not recorded:
        docker('image', 'rm', *(tags or [image]))
        assert docker('image', 'inspect', image, check=False).returncode != 0
        run('t33-old-and-snapshot-missing', ['recover'], 2)
    latest = database.execute(
        'SELECT phase,error_json,candidate_container_id FROM reference_apply_operations '
        'WHERE agent_id=? ORDER BY created_at DESC,id DESC LIMIT 1', (TARGET,)).fetchone()
    assert latest['phase'] == 'failed' and latest['candidate_container_id'] is None, dict(latest)
    assert json.loads(latest['error_json'])['code'] == 'environment_recovery_unavailable', dict(latest)
    if not recorded:
        result_path.write_text(json.dumps(dict(latest), indent=2))
        docker('image', 'load', '--input', str(OUT / 'missing-old-snapshot.tar'))

# 所有待删除身份必须属于这八个夹具；其他受管或非受管容器完全不动。
ids = docker('ps', '-aq').stdout.decode().split()
containers = json.loads(docker('inspect', *ids).stdout) if ids else []
selected = []
preserved = []
for container in containers:
    labels = container['Config'].get('Labels') or {}
    if labels.get('mai.team.agent') in AGENTS or labels.get('mai.team.project') in PROJECTS:
        assert labels.get('mai.team.managed') == 'true', labels
        selected.append(container)
    else:
        preserved.append({'id': container['Id'], 'labels': labels})
(OUT / 'containers-preserved.json').write_text(json.dumps(preserved, indent=2))
(OUT / 'containers-before-reseed.json').write_text(json.dumps(selected, indent=2))
for container in selected:
    identity = container['Id']
    docker('stop', identity)
    docker('commit', identity, 'mai-acceptance-red-20261015:' + identity[:12])
for container in selected:
    docker('rm', container['Id'])
shutil.move(ROOT / 'data', archive)
run('fresh-seed', ['seed'], 0)
run('fresh-bind', ['--scenario', 'bind', 'apply'], 0)

# 精确在 DB binding 已发布、activation 尚未返回处硬退出，再保存更新修订。
control = ROOT / 'fault-control.json'
control.write_text(json.dumps({'mode': 'crash_activation', 'agent_id': TARGET}))
run('binding-published-crash', ['clear'], 137)
published = json.loads(control.with_suffix('.activation.json').read_text())
control.unlink()
assert published['container_id']
run('new-revision-after-publication-crash', ['--scenario', 'bind', 'apply'], 2)
assert not json.loads(docker('inspect', published['container_id']).stdout)[0]['State']['Running']
# 单次领域 CLI 不带 Runtime 的补偿调度器：旧收据 superseded 后再显式接续当前目标。
run('superseding-current-target-recovery', ['recover'], 0)
with sqlite3.connect(ROOT / 'data/mai-team.sqlite3') as database:
    current = database.execute('SELECT container_id FROM agent_reference_bindings WHERE agent_id=?', (TARGET,)).fetchone()[0]
    assert current != published['container_id']
    assert json.loads(docker('inspect', current).stdout)[0]['State']['Running']
(OUT / 'publication-recovery-result.json').write_text(json.dumps({
    'passed': True, 'published_before_crash': published['container_id'], 'recovered': current}, indent=2))
print('PUBLISHED_SUPERSESSION_RECOVERY_PASS', flush=True)
