#!/usr/bin/env python3
"""独立 Docker 上搜索固定 Linux 树；数据页冷缓存仅用 fadvise，不清全机缓存。"""
import argparse
import concurrent.futures
import json
import math
import os
import pathlib
import statistics
import subprocess
import time

SOCKET = 'unix:///run/mai-reference-acceptance-docker/docker.sock'
ROOT = pathlib.Path('/mnt/docker_nvme/mai-references-acceptance/20261015-shared-sources')
SOURCE = pathlib.Path('/mnt/docker_nvme/mai-references/linux/7b20d05fd38de8e0be168956f6fd241d750c7d69/repo')
parser = argparse.ArgumentParser()
parser.add_argument('--output', required=True)
parser.add_argument('--repeats', type=int, default=5)
args = parser.parse_args()
output = pathlib.Path(args.output).resolve()
if os.environ.get('DOCKER_HOST') != SOCKET or ROOT not in output.parents:
    raise SystemExit('仅允许专用 Docker daemon 和本次验收证据目录')
if not 2 <= args.repeats <= 20:
    raise SystemExit('repeats 必须在 2 到 20 之间')

def docker(*argv, **kwargs):
    return subprocess.run(['docker', *argv], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs)

def evict_data_pages():
    count = 0
    for directory, _, files in os.walk(str(SOURCE), followlinks=False):
        for name in files:
            path = pathlib.Path(directory) / name
            if path.is_symlink():
                continue
            with path.open('rb') as stream:
                os.posix_fadvise(stream.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
            count += 1
    return count

def quantile(values, probability):
    values = sorted(values)
    return values[max(0, math.ceil(len(values) * probability) - 1)]

name = 'mai-reference-benchmark-' + str(os.getpid())
image = 'ghcr.io/zr233/mai-team-agent:latest'
docker('image', 'inspect', image)
container = docker('create', '--name', name, '--label', 'mai.reference.acceptance=benchmark',
                   '--network', 'none', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
                   '--memory', '2g', '--cpus', '4', '--pids-limit', '64',
                   '--mount', 'type=bind,src='+str(SOURCE)+',dst=/references/linux,readonly,bind-recursive=readonly,bind-propagation=rprivate',
                   image, 'sleep', 'infinity').stdout.decode().strip()
report = {'source': str(SOURCE), 'commit': SOURCE.parent.name, 'image': image,
          'cold_method': 'POSIX_FADV_DONTNEED on this immutable source only; metadata cache retained',
          'container_limits': {'memory_bytes': 2*1024**3, 'cpus': 4, 'pids': 64}, 'runs': [], 'summary': []}
try:
    docker('start', container)
    inspected = json.loads(docker('inspect', container).stdout)[0]
    report['image_id'] = inspected['Image']
    report['mounts'] = inspected['Mounts']
    pid = inspected['State']['Pid']
    cgroup_line = next(line for line in pathlib.Path('/proc/'+str(pid)+'/cgroup').read_text().splitlines() if line.startswith('0::'))
    cgroup = pathlib.Path('/sys/fs/cgroup') / cgroup_line.split('::', 1)[1].lstrip('/')
    query = ['rg', '-n', r'SYSCALL_DEFINE[0-6]\(openat2', '/references/linux']
    def search(_):
        started = time.perf_counter()
        result = subprocess.run(['docker', 'exec', container, *query], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
        if result.returncode != 0:
            raise RuntimeError('真实 Linux 匹配搜索失败: '+str(result.returncode)+' '+result.stderr.decode())
        return {'latency_ms': (time.perf_counter()-started)*1000, 'output_bytes': len(result.stdout), 'exit_code': result.returncode}
    for concurrency in [1, 4, 8]:
        for cache in ['cold_data', 'hot']:
            if cache == 'hot':
                search(0)
            results = []
            for repeat in range(args.repeats):
                evicted = evict_data_pages() if cache == 'cold_data' else 0
                before_cpu = (cgroup/'cpu.stat').read_text()
                started = time.perf_counter()
                with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as executor:
                    batch = list(executor.map(search, range(concurrency)))
                wall_ms = (time.perf_counter()-started)*1000
                sample = {'concurrency': concurrency, 'cache': cache, 'repeat': repeat, 'evicted_files': evicted,
                          'wall_ms': wall_ms, 'commands': batch, 'cpu_before': before_cpu,
                          'cpu_after': (cgroup/'cpu.stat').read_text(),
                          'memory_current': int((cgroup/'memory.current').read_text()),
                          'memory_peak': int((cgroup/'memory.peak').read_text())}
                report['runs'].append(sample)
                results += [item['latency_ms'] for item in batch]
            report['summary'].append({'concurrency': concurrency, 'cache': cache, 'commands': len(results),
                                      'p50_ms': statistics.median(results), 'p95_ms': quantile(results, .95)})
    no_match = subprocess.run(['docker', 'exec', container, 'rg', '-n', '-F', '__MAI_ABSENT_20261015_01A13E2A__', '/references/linux'], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
    timed = subprocess.run(['docker', 'exec', container, 'timeout', '0.001s', 'rg', '-n', '-F', 'task_struct', '/references/linux'], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
    if no_match.returncode != 1 or no_match.stdout or timed.returncode != 124:
        raise RuntimeError('无匹配或超时退出语义不符合预期')
    report['semantics'] = {'no_match': {'exit_code': no_match.returncode, 'stdout_bytes': len(no_match.stdout)},
                           'timeout': {'exit_code': timed.returncode, 'stdout_bytes': len(timed.stdout)}}
    report['ok'] = True
finally:
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, ensure_ascii=False, indent=2)+'\n')
    docker('rm', '-f', container)
print(json.dumps({'output': str(output), 'summary': report['summary'], 'ok': report.get('ok', False)}, ensure_ascii=False))
