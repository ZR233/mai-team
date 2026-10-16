#!/usr/bin/env python3
"""全部目标环境应用完成后恢复本次维护前的自动审查开关。"""
import datetime, json, os, pathlib, urllib.request
assert os.geteuid() == 0
os.umask(0o077)
root = pathlib.Path('/mnt/docker_nvme/mai-team/deployments')
original = json.loads((root / 'shared-references-original-review.json').read_text())
assert original['state'] == 'drained'
assert (root / 'shared-references-installed.json').is_file()
base = 'http://127.0.0.1:8080'
def api(path, body=None):
    req = urllib.request.Request(base + path, data=None if body is None else json.dumps(body).encode(),
                                 method='GET' if body is None else 'PATCH', headers={'Content-Type':'application/json'})
    with urllib.request.urlopen(req, timeout=30) as response:
        assert response.status == 200
        return json.load(response)
status = api('/projects/760ebfac-5002-4574-84e4-acb0cca4d3ac/references/status')
assert status['state'] == 'applied' and status['counts']['failed'] == 0 and status['counts']['waiting'] == 0
assert {p['id'] for p in api('/projects')} == {p['id'] for p in original['projects']}
restored = []
for p in original['projects']:
    api('/projects/' + p['id'], {'auto_review_enabled': p['auto_review_enabled']})
    current = api('/projects/' + p['id'])
    assert current['auto_review_enabled'] == p['auto_review_enabled']
    restored.append({k: current[k] for k in ('id', 'name', 'auto_review_enabled')})
report = {'restored_at': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'projects': restored}
(root / 'shared-references-evidence/reviews-restored.json').write_text(json.dumps(report, indent=2))
print(json.dumps(report, ensure_ascii=False))
