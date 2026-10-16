#!/usr/bin/env python3
"""数据根覆盖发布目录时，在其外建立固定快照的只读私有挂载投影。"""
import datetime, grp, json, os, pathlib, subprocess, time, urllib.error, urllib.request

assert os.geteuid() == 0
os.umask(0o077)
root = pathlib.Path('/mnt/docker_nvme/mai-team')
commit = '7b20d05fd38de8e0be168956f6fd241d750c7d69'
published = pathlib.Path('/mnt/docker_nvme/mai-references/linux')/commit
projected = pathlib.Path('/srv/mai-reference-mounts/linux')/commit
evidence = root/'deployments/shared-references-evidence'
record_path = root/'deployments/shared-references-bind-projection.json'
assert not record_path.exists(), '投影已配置；按现有收据继续，不能覆盖'

def api(path):
    with urllib.request.urlopen('http://127.0.0.1:8080'+path, timeout=90) as response:
        return json.load(response)

assert all(not p['auto_review_enabled'] for p in api('/projects'))
deadline = time.monotonic()+60
while True:
    active = []
    for agent in api('/agents'):
        try:
            thread = api('/threads/'+agent['id'])
        except urllib.error.HTTPError as error:
            assert error.code == 409 and agent['project_id'] == '760ebfac-5002-4574-84e4-acb0cca4d3ac'
            status = api('/projects/'+agent['project_id']+'/references/status')
            assert status['state'] == 'failed'
            running = subprocess.check_output(['docker','inspect','--format','{{.State.Running}}',agent['container_id']], text=True).strip()
            assert running == 'false', '阻塞项目的旧容器应保持停止'
            continue
        if thread.get('activeTurn'):
            active.append(agent['id'])
    if not active:
        break
    assert time.monotonic()<deadline, '仍有当前 Turn，保持原状态等待排空'
    time.sleep(2)
old_source = json.loads((evidence/'linux-source.json').read_text())
assert old_source['host_path'] == str(published/'repo')
(evidence/'linux-source-before-projection.json').write_text(json.dumps(old_source, indent=2))
(evidence/'linux-apply-failed-daemon-root.json').write_text(json.dumps(api('/projects/760ebfac-5002-4574-84e4-acb0cca4d3ac/references/status'), indent=2))
assert pathlib.Path(json.loads(subprocess.check_output(['docker','info','--format','{{json .DockerRootDir}}'], text=True))).resolve() == pathlib.Path('/mnt/docker_nvme')
for path in [projected.parent.parent, projected.parent, projected]:
    created = not path.exists()
    path.mkdir(exist_ok=True, mode=0o755)
    st=path.lstat()
    assert path.is_dir() and not path.is_symlink() and st.st_uid == 0 and not st.st_mode & 0o022
    if created:
        os.chmod(path, 0o755)
    assert path.stat().st_mode & 0o111 == 0o111
if subprocess.run(['findmnt','-rn','--mountpoint',str(projected)], stdout=subprocess.DEVNULL).returncode == 0:
    assert (projected.stat().st_dev, projected.stat().st_ino) == (published.stat().st_dev, published.stat().st_ino)
else:
    assert not any(projected.iterdir()), '拒绝覆盖投影位置已有数据'
unit = subprocess.check_output(['systemd-escape','--path','--suffix=mount',str(projected)], text=True).strip()
unit_path = pathlib.Path('/etc/systemd/system')/unit
unit_content = f'''[Unit]
Description=Mai 共享参考源码固定快照只读投影
Before=mai-server.service
RequiresMountsFor={published}

[Mount]
What={published}
Where={projected}
Type=none
Options=bind,ro,rprivate

[Install]
WantedBy=local-fs.target
'''
if unit_path.exists():
    assert unit_path.read_text() == unit_content, '拒绝覆盖其他挂载单元'
else:
    unit_path.write_text(unit_content)
os.chmod(unit_path, 0o644)
subprocess.run(['systemctl','daemon-reload'], check=True)
subprocess.run(['systemctl','enable','--now',unit], check=True)
assert (published/'repo').stat().st_ino == (projected/'repo').stat().st_ino
assert (published/'repo').stat().st_dev == (projected/'repo').stat().st_dev
mount = subprocess.check_output(['findmnt','-rn','-o','OPTIONS,PROPAGATION','--mountpoint',str(projected)], text=True).strip()
assert 'ro' in mount.split()[0].split(',') and mount.split()[1] == 'private', mount
inventory = json.loads((pathlib.Path((root/'backups/shared-references-latest.txt').read_text().strip())/'container-inventory.json').read_text())
image = next(c['Image'] for c in inventory if c['Name']=='/mai-team-566c6cb7-43c1-4713-b94c-d79dd3beff21')
# 仅临时探针绕过 0444 的 DAC，区分 EACCES 与内核 EROFS；执行环境权限不变。
probe = subprocess.run(['docker','run','--rm','--name','mai-reference-projection-probe-20261016','--network','none','--cap-drop','ALL','--cap-add','DAC_OVERRIDE','--security-opt','no-new-privileges',
    '--mount',f'type=bind,source={projected}/repo,target=/references/linux,readonly,bind-recursive=readonly,bind-propagation=rprivate',
    '--entrypoint','/bin/sh',image,'-c','head -n 5 /references/linux/Makefile; if (exec 3>>/references/linux/Makefile) 2>/tmp/probe-error; then exit 17; fi; grep -i "read-only" /tmp/probe-error'], capture_output=True, text=True)
assert probe.returncode == 0, probe.stderr
assert 'VERSION = 7' in probe.stdout and 'SUBLEVEL = 10' in probe.stdout
record = {'created_at':datetime.datetime.now(datetime.timezone.utc).isoformat(), 'published_path':str(published/'repo'), 'host_path':str(projected/'repo'),
          'mountpoint':str(projected), 'unit':unit, 'unit_path':str(unit_path), 'replaces_source_id':old_source['id'],
          'same_device_inode':True, 'mount':mount, 'probe_stdout':probe.stdout, 'probe_exit':probe.returncode, 'probe_capabilities':['DAC_OVERRIDE']}
record_path.write_text(json.dumps(record, ensure_ascii=False, indent=2))
subprocess.run(['systemctl','stop','mai-server.service'], check=True)
policy = pathlib.Path('/etc/mai-server/reference-sources.toml')
(root/'deployments/shared-references-policy-before-projection.toml').write_bytes(policy.read_bytes())
policy.write_text(f'allowed_roots = ["{projected}"]\ntrusted_owner_uids = [0]\nmax_bindings_per_project = 16\n')
os.chown(policy,0,grp.getgrnam('mai-server').gr_gid);os.chmod(policy,0o640)
drop = pathlib.Path('/etc/systemd/system/mai-server.service.d/50-reference-sources.conf')
drop.write_text(f'[Unit]\nRequiresMountsFor={projected}\n\n[Service]\nEnvironment=MAI_REFERENCE_POLICY=/etc/mai-server/reference-sources.toml\n')
os.chmod(drop,0o644)
subprocess.run(['systemctl','daemon-reload'],check=True)
subprocess.run(['systemctl','start','mai-server.service'],check=True)
print(json.dumps(record,ensure_ascii=False))
