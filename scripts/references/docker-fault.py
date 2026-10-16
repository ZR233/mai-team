#!/usr/bin/env python3
"""仅固定隔离 daemon 可用的 Docker 故障注入，不接触生产 daemon。"""
import json
import os
import pathlib
import subprocess
import sys
import time

ROOT = pathlib.Path('/mnt/docker_nvme/mai-references-acceptance/20261015-shared-sources')
SOCKET = '/run/mai-reference-acceptance-docker/docker.sock'
if os.environ.get('DOCKER_HOST') != 'unix://' + SOCKET:
    sys.exit('故障注入要求独立验收 daemon')
if os.stat(SOCKET).st_ino == os.stat('/var/run/docker.sock').st_ino:
    sys.exit('验收 socket 与生产 socket 相同')
control_file = ROOT / 'fault-control.json'
control = json.loads(control_file.read_text()) if control_file.exists() else {}
args = sys.argv[1:]
mode = control.get('mode')
agent = control.get('agent_id', '')
owned = any('mai.team.agent=' + agent in arg for arg in args) and bool(agent)
commit = args[:1] == ['commit']
sidecar = args[:1] == ['create'] and any('mai.team.sidecar.kind=agent' in arg for arg in args)
if owned and ((commit and mode in ('docker_offline', 'enospc')) or (sidecar and mode == 'mcp_failure')):
    message = {
        'docker_offline': 'Cannot connect to the Docker daemon (isolated injected outage)',
        'enospc': 'write snapshot layer: no space left on device (isolated injected ENOSPC)',
        'mcp_failure': 'required MCP sidecar start failed (isolated injected failure)',
    }[mode]
    evidence = {'time': time.time(), 'mode': mode, 'agent_id': agent, 'command': args, 'message': message}
    with (ROOT / 'evidence/docker-faults.jsonl').open('a') as output:
        output.write(json.dumps(evidence) + '\n')
    print(message, file=sys.stderr)
    sys.exit(125)
os.execv('/usr/bin/docker', ['/usr/bin/docker', *args])
