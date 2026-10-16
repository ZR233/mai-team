#!/usr/bin/env python3
"""生产健康后登记固定 Linux 快照，以 CAS 绑定并等待全部环境实际应用。"""
import datetime, json, os, pathlib, time, urllib.request, uuid

assert os.geteuid() == 0
os.umask(0o077)
root = pathlib.Path('/mnt/docker_nvme/mai-team/deployments/shared-references-evidence')
root.mkdir(parents=True, exist_ok=True, mode=0o700)
base = 'http://127.0.0.1:8080'
project = '760ebfac-5002-4574-84e4-acb0cca4d3ac'
commit = '7b20d05fd38de8e0be168956f6fd241d750c7d69'
path = '/mnt/docker_nvme/mai-references/linux/' + commit + '/repo'
digest = 'sha256:d3cbb76d09dcd270b8bfddbedaaeb3b30708b9e33321ac5316d7ed2038df930e'
projection_file = root.parent/'shared-references-bind-projection.json'
projection = json.loads(projection_file.read_text()) if projection_file.exists() else None
if projection:
    assert projection['published_path'] == path and projection['same_device_inode']
    path = projection['host_path']

def api(route, body=None, method=None):
    request = urllib.request.Request(base + route, data=None if body is None else json.dumps(body).encode(),
                                     method=method, headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, timeout=90) as response:
        assert response.status in (200, 202, 204)
        return None if response.status == 204 else json.load(response)

policy = api('/reference-source-policy')
assert policy['allowed_roots'] == [str(pathlib.Path(path).parent)]
assert policy['max_bindings_per_project'] == 16
assert policy['trusted_owner_uids'] == [0]
sources = api('/reference-sources')
assert isinstance(sources, list)
matching = [s for s in sources if s['host_path'] == path]
assert len(matching) <= 1
source = matching[0] if matching else api('/reference-sources', {
    'name': 'Linux 7.2.10', 'host_path': path, 'revision_label': 'Linux 7.2.10 / ' + commit,
    'approved_for_agent_access': True}, 'POST')
deadline = time.monotonic() + 300
while source['state'] == 'validating':
    assert time.monotonic() < deadline, '源验证超时'
    time.sleep(1)
    source = api('/reference-sources/' + source['id'])
assert source['state'] == 'ready', json.dumps(source.get('validation_error'))
assert source['manifest_digest'] == digest
assert source['provenance']['commit_sha'] == commit
(root / 'linux-source.json').write_text(json.dumps(source, indent=2, ensure_ascii=False))
route = '/projects/' + project + '/references'
config = api(route)
if not config['bindings']:
    config = api(route, {'expected_revision': config['revision'], 'bindings': [{
        'id': str(uuid.uuid4()), 'source_id': source['id'], 'alias': 'linux',
        'description': 'Linux 7.2.10 固定只读源码，供内核、驱动及生命周期参考；不能在此编译或修改。',
        'enabled': True}]}, 'PUT')
if projection and len(config['bindings']) == 1 and config['bindings'][0]['source_id'] != source['id']:
    old_binding = config['bindings'][0]
    assert old_binding['source_id'] == projection['replaces_source_id'] and old_binding['alias'] == 'linux' and old_binding['enabled']
    replacement = dict(old_binding, source_id=source['id'])
    config = api(route, {'expected_revision': config['revision'], 'bindings': [replacement]}, 'PUT')
assert len(config['bindings']) == 1
binding = config['bindings'][0]
assert binding['source_id'] == source['id'] and binding['alias'] == 'linux' and binding['enabled']
accepted = api(route + '/apply', {'expected_revision': config['revision'], 'policy': 'when_idle'}, 'POST')
(root / 'linux-apply-accepted.json').write_text(json.dumps(accepted, indent=2))
deadline = time.monotonic() + 600
previous = None
while True:
    status = api(route + '/status')
    counts = status['counts']
    shape = (status['state'], json.dumps(counts, sort_keys=True))
    if shape != previous:
        print(json.dumps({'state': status['state'], 'counts': counts}), flush=True)
        previous = shape
    (root / 'linux-apply-status.json').write_text(json.dumps(status, indent=2))
    assert counts['failed'] == 0, '环境应用失败，保持阻塞并诊断'
    if status['state'] == 'applied' and counts['applied'] > 0 and counts['waiting'] == 0:
        break
    assert time.monotonic() < deadline, '环境应用未收敛'
    time.sleep(2)
print(json.dumps({'project_id': project, 'revision': config['revision'], 'source_id': source['id'],
                  'state': status['state'], 'counts': counts,
                  'completed_at': datetime.datetime.now(datetime.timezone.utc).isoformat()}))

if projection and projection['replaces_source_id'] != source['id']:
    stale = [s for s in api('/reference-sources') if s['id'] == projection['replaces_source_id']]
    if stale:
        assert stale[0]['host_path'] == projection['published_path']
        api('/reference-sources/'+stale[0]['id'], method='DELETE')
