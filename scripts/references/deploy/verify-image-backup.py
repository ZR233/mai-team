#!/usr/bin/env python3
"""在全新私有 Docker daemon 中实际加载备份，验证回退不依赖原有 blob。"""
import datetime
import json
import os
import pathlib
import subprocess
import time

assert os.geteuid() == 0
os.umask(0o077)
root = pathlib.Path('/mnt/docker_nvme/mai-team')
backup = pathlib.Path((root / 'backups/shared-references-latest.txt').read_text().strip())
probe = pathlib.Path('/mnt/docker_nvme/mai-references-acceptance/20261015-shared-sources/restore-probe-private-docker')
runtime = pathlib.Path('/run/mai-reference-restore-private')
unit = 'mai-reference-restore-private-docker.service'
containerd_unit = 'mai-reference-restore-private-containerd.service'
report_path = root / 'deployments/shared-references-evidence/backup-image-load.json'
assert not report_path.exists() and not probe.exists() and not runtime.exists()
production_pid = subprocess.check_output(['systemctl', 'show', 'docker.service', '--value', '-p', 'MainPID'], text=True).strip()
assert production_pid.isdigit() and int(production_pid) > 0
space = os.statvfs(root)
assert space.f_bavail * space.f_frsize > 12 * 1024**3, '恢复探针需要预留 12 GiB 空间'
probe.mkdir(mode=0o700)
runtime.mkdir(mode=0o700)
configuration = {'data-root': str(probe / 'data'), 'exec-root': str(runtime / 'exec'),
                 'hosts': ['unix://' + str(runtime / 'docker.sock')], 'pidfile': str(runtime / 'docker.pid'),
                 'containerd': str(runtime / 'containerd.sock'),
                 'containerd-namespace': 'mai-reference-restore-probe',
                 'containerd-plugins-namespace': 'mai-reference-restore-probe-plugins',
                 'bridge': 'none', 'iptables': False, 'ip6tables': False, 'ip-forward': False, 'ip-masq': False}
(probe / 'daemon.json').write_text(json.dumps(configuration))
(probe / 'containerd.toml').write_text(f'''version = 3
root = "{probe / 'containerd-data'}"
state = "{runtime / 'containerd-state'}"
disabled_plugins = ["io.containerd.grpc.v1.cri", "io.containerd.cri.v1.runtime", "io.containerd.cri.v1.images"]
[grpc]
address = "{runtime / 'containerd.sock'}"
''')
command = ['docker', '--host', configuration['hosts'][0]]
started = datetime.datetime.now(datetime.timezone.utc).isoformat()
started_units = []
try:
    subprocess.run(['systemd-run', '--unit=' + containerd_unit, '--collect', '--property=MemoryMax=1G',
                    '--property=CPUQuota=100%', '--property=IOWeight=10', '/usr/bin/containerd',
                    '--config=' + str(probe / 'containerd.toml')], check=True)
    started_units.append(containerd_unit)
    subprocess.run(['systemd-run', '--unit=' + unit, '--collect', '--property=MemoryMax=2G',
                    '--property=CPUQuota=100%', '--property=IOWeight=10', '/usr/bin/dockerd',
                    '--config-file=' + str(probe / 'daemon.json')], check=True)
    started_units.append(unit)
    deadline = time.monotonic() + 60
    while subprocess.run(command + ['info'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode:
        assert time.monotonic() < deadline, '私有恢复 daemon 未就绪'
        time.sleep(1)
    assert subprocess.check_output(command + ['info', '--format', '{{.DockerRootDir}}'], text=True).strip() == configuration['data-root']
    assert not subprocess.check_output(command + ['image', 'ls', '--quiet', '--all'], text=True).strip(), '必须是全新私有镜像库'
    loaded = subprocess.run(command + ['load', '--platform=linux/amd64', '--input', str(backup / 'images.tar')],
                            capture_output=True, text=True, timeout=900)
    (probe / 'load.stdout').write_text(loaded.stdout)
    (probe / 'load.stderr').write_text(loaded.stderr)
    assert loaded.returncode == 0, '实际 Docker load 失败，查看私有 load.stderr'
    expected = json.loads((backup / 'backup-manifest.json').read_text())['image_ids']
    inspected = json.loads(subprocess.check_output(command + ['image', 'inspect', *expected], text=True))
    assert {image['Id'] for image in inspected} == set(expected)
    assert not subprocess.check_output(command + ['ps', '--all', '--quiet'], text=True).strip()
    assert subprocess.check_output(['systemctl', 'show', 'docker.service', '--value', '-p', 'MainPID'], text=True).strip() == production_pid
    report = {'started_at': started, 'completed_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
              'platform': 'linux/amd64', 'initial_images': 0, 'images_loaded_and_inspected': len(inspected),
              'containers_created': 0, 'docker_load_exit': loaded.returncode, 'production_daemon_unchanged': True,
              'fresh_private_data_root': str(probe / 'data'), 'private_containerd_root': str(probe / 'containerd-data'),
              'private_containerd_socket': configuration['containerd']}
finally:
    for started_unit in reversed(started_units):
        subprocess.run(['systemctl', 'stop', started_unit], check=True)
report['isolated_daemon_stopped'] = True
report['private_containerd_stopped'] = True
report_path.write_text(json.dumps(report, indent=2))
print(json.dumps(report))
