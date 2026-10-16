#!/usr/bin/env python3
"""核对生产当前执行环境及对应 sidecar 的真实固定只读挂载。"""
import json, os, pathlib, subprocess, urllib.request
assert os.geteuid() == 0
os.umask(0o077)
root = pathlib.Path('/mnt/docker_nvme/mai-team/deployments/shared-references-evidence')
project = '760ebfac-5002-4574-84e4-acb0cca4d3ac'
source = '/mnt/docker_nvme/mai-references/linux/7b20d05fd38de8e0be168956f6fd241d750c7d69/repo'
with urllib.request.urlopen('http://127.0.0.1:8080/projects/' + project + '/references/status') as response:
    status = json.load(response)
assert status['state'] == 'applied' and status['counts']['waiting'] == status['counts']['failed'] == 0
ids = subprocess.check_output(['docker', 'ps', '-aq', '--filter', 'label=mai.team.managed=true'], text=True).split()
containers = json.loads(subprocess.check_output(['docker', 'inspect', *ids], text=True)) if ids else []
agents = {a['agent_id']: a for a in status['agents'] if a.get('container_id')}
records = []
for aid, applied in agents.items():
    selected = [c for c in containers if (c['Config'].get('Labels') or {}).get('mai.team.agent') == aid]
    assert any(c['Id'] == applied['container_id'] for c in selected)
    for c in selected:
        labels = c['Config']['Labels']
        # 不把已退出的旧 generation 当作当前环境。
        if c['Id'] != applied['container_id'] and labels.get('mai.team.sidecar.source') != applied['container_id']:
            continue
        mounts = [m for m in c['Mounts'] if m['Destination'] == '/references/linux']
        assert len(mounts) == 1 and mounts[0]['Type'] == 'bind' and mounts[0]['Source'] == source
        assert mounts[0]['RW'] is False and c['State']['Running']
        assert c['HostConfig']['Privileged'] is False
        raw = subprocess.check_output(['docker', 'exec', c['Id'], 'head', '-n', '5', '/references/linux/Makefile'], text=True)
        assert 'VERSION = 7' in raw and 'PATCHLEVEL = 2' in raw and 'SUBLEVEL = 10' in raw
        # 只尝试打开现有文件的 append FD，不写入字节；异常允许打开也不会改动源码内容。
        probe = subprocess.run(['docker', 'exec', c['Id'], 'sh', '-c', 'exec 3>>/references/linux/Makefile'], text=True, capture_output=True)
        assert probe.returncode != 0 and 'read-only' in probe.stderr.lower(), '当前挂载没有拒绝写权限打开'
        records.append({'agent_id': aid, 'container_id': c['Id'], 'sidecar': labels.get('mai.team.sidecar') == 'true',
                        'generation': applied['generation'], 'mount': mounts[0], 'write_probe_exit': probe.returncode,
                        'write_probe_stderr': probe.stderr.strip(), 'makefile': raw})
assert len(records) >= len(agents)
backup_pointer = pathlib.Path('/mnt/docker_nvme/mai-team/backups/shared-references-latest.txt')
backup = pathlib.Path(backup_pointer.read_text().strip())
old = json.loads((backup / 'backup-manifest.json').read_text())
all_ids = set(subprocess.check_output(['docker', 'ps', '-aq', '--no-trunc'], text=True).split())
assert set(old['preserved_unmanaged_container_ids']) <= all_ids, '原有非受管容器发生变化'
(root / 'linux-mounts.json').write_text(json.dumps(records, indent=2))
print(json.dumps({'agents': len(agents), 'containers_and_sidecars': len(records), 'read_only': True,
                  'unmanaged_preserved': len(old['preserved_unmanaged_container_ids'])}))
