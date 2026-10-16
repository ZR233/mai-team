#!/usr/bin/env python3
"""启动装配测试的有状态 Docker 替身；只模拟被本用例使用的 inspect/创建契约。"""
import copy
import json
import sys
import uuid
from pathlib import Path

arguments = sys.argv[1:]
state_file = Path(__file__).with_suffix('.json')
state = json.loads(state_file.read_text()) if state_file.exists() else {}

def option(name, default=None):
    return arguments[arguments.index(name) + 1] if name in arguments else default

def options(name):
    return [arguments[index + 1] for index, value in enumerate(arguments[:-1]) if value == name]

def mount(specification):
    fields = dict(part.split('=', 1) for part in specification.split(',') if '=' in part)
    kind = fields.get('type', 'volume')
    source = fields.get('src', fields.get('source'))
    target = fields.get('dst', fields.get('target'))
    actual = {'Type': kind, 'Source': source, 'Destination': target, 'RW': 'readonly' not in specification.split(',')}
    configured = {'Type': kind, 'Source': source, 'Target': target, 'ReadOnly': not actual['RW']}
    if kind == 'volume':
        actual['Name'] = source
    return actual, configured

command = arguments[0]
if command == 'ps':
    print('\n'.join(state))
elif command == 'image':
    if arguments[1] == 'inspect':
        print('sha256:fixture')
    elif arguments[1] == 'ls':
        print('')
    else:
        sys.exit(1)
elif command == 'version':
    print('test-version')
elif command == 'create':
    identifier = 'fixture-' + uuid.uuid4().hex
    image = arguments[-3]
    mounts = []
    configured = []
    for specification in options('--mount'):
        actual, configuration = mount(specification)
        mounts.append(actual)
        configured.append(configuration)
    source = option('--volumes-from')
    if source in state:
        mounts += [copy.deepcopy(value) for value in state[source]['Mounts'] if value['Type'] == 'volume']
    labels = dict(value.split('=', 1) for value in options('--label'))
    memory = option('--memory', '0').lower().replace('g', '*1073741824').replace('m', '*1048576')
    memory = int(memory.split('*')[0]) * (int(memory.split('*')[1]) if '*' in memory else 1)
    state[identifier] = {'Id': identifier, 'Name': option('--name'), 'Image': 'sha256:fixture',
        'Config': {'Image': image, 'Labels': labels}, 'State': {'Status': 'running', 'Running': True},
        'Mounts': mounts, 'HostConfig': {'Mounts': configured, 'NetworkMode': option('--network', 'host'),
            'Memory': memory, 'NanoCpus': int(float(option('--cpus', '0')) * 1e9),
            'PidsLimit': int(option('--pids-limit', '0')), 'Privileged': False,
            'CapAdd': [], 'CapDrop': options('--cap-drop'), 'SecurityOpt': options('--security-opt'),
            'VolumesFrom': [source] if source else []}}
    print(identifier)
elif command == 'inspect':
    identifiers = [value for value in arguments[1:] if value in state]
    if '--format' in arguments:
        print('true')
    elif identifiers:
        print(json.dumps([state[value] for value in identifiers]))
    else:
        print('No such container', file=sys.stderr)
        sys.exit(1)
elif command == 'rm':
    for identifier in arguments[1:]:
        state.pop(identifier, None)
elif command == 'stop':
    for identifier in arguments[1:]:
        if identifier in state:
            state[identifier]['State'] = {'Status': 'exited', 'Running': False}
state_file.write_text(json.dumps(state))
