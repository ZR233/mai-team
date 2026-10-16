#!/usr/bin/env python3
"""保存生产发布前后事实；不发送聊天、不手动触发审查。"""
import datetime, hashlib, json, os, pathlib, re, sqlite3, subprocess, sys, urllib.request

assert os.geteuid() == 0
os.umask(0o077)
mode = sys.argv[1]
assert mode in ('before', 'after')
root = pathlib.Path('/mnt/docker_nvme/mai-team')
dest = root / 'deployments/shared-references-evidence'
dest.mkdir(parents=True, exist_ok=True, mode=0o700)
base = 'http://127.0.0.1:8080'

def get(path):
    with urllib.request.urlopen(base + path, timeout=90) as response:
        assert response.status == 200
        return json.load(response)

def docker(*args):
    return json.loads(subprocess.check_output(['docker', *args], text=True))

report = {'time_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'health': get('/health')}
report['projects'] = get('/projects')
agents = get('/agents')
report['agents'] = []
for agent in agents:
    aid = agent['id']
    thread = get('/threads/' + aid)
    turns = get('/threads/' + aid + '/turns')
    # 完整历史响应留在私有目录；公开报告只保留哈希、形状及条目数。
    raw = json.dumps(turns, sort_keys=True).encode()
    (dest / (mode + '-' + aid + '-turns.json')).write_bytes(raw)
    runtime = thread.get('runtime') or {}
    item = {'id': aid, 'project_id': agent.get('project_id'), 'container_id': agent.get('container_id'),
            'active_turn': bool(thread.get('activeTurn')), 'mcp_servers': runtime.get('activeMcpServers', []),
            'history_sha256': hashlib.sha256(raw).hexdigest(),
            'history_shape': list(turns) if isinstance(turns, dict) else 'array',
            'history_items': len(turns) if isinstance(turns, list) else None}
    assert not item['active_turn'], '发布验收期间出现活动 Turn'
    if item['container_id']:
        physical = docker('inspect', item['container_id'])[0]
        item['docker'] = {'id': physical['Id'], 'image': physical['Image'],
                          'running': physical['State']['Running'], 'mounts': physical['Mounts']}
    report['agents'].append(item)
with sqlite3.connect('file:' + str(root / 'data/mai-team.sqlite3') + '?mode=ro', uri=True) as db:
    report['counts'] = {table: db.execute('select count(*) from ' + table).fetchone()[0]
                        for table in ('projects', 'agents', 'product_events', 'project_review_runs')}
    report['schema'] = db.execute('pragma user_version').fetchone()[0]
with urllib.request.urlopen(base + '/', timeout=15) as response:
    html = response.read().decode()
    assert response.status == 200
asset = re.search(r'(?:src|href)="(/assets/[^\"]+\.js)"', html)
assert asset, 'Web UI 未引用构建的 JS 资源'
with urllib.request.urlopen(base + asset.group(1), timeout=15) as response:
    assert response.status == 200
    report['web_asset_sha256'] = hashlib.sha256(response.read()).hexdigest()
with urllib.request.urlopen(base + '/events/product', timeout=4) as response:
    assert response.status == 200
    assert response.headers.get_content_type() == 'text/event-stream'
    report['sse'] = {'status': response.status, 'content_type': response.headers.get_content_type()}
if mode == 'after':
    before = json.loads((dest / 'before.json').read_text())
    assert {p['id'] for p in before['projects']} == {p['id'] for p in report['projects']}
    old = {a['id']: a for a in before['agents']}
    assert set(old) == {a['id'] for a in report['agents']}
    for a in report['agents']:
        prev = old[a['id']]
        assert a['history_sha256'] == prev['history_sha256'], '历史响应发生变化：' + a['id']
        old_volumes = {m['Destination']: m['Name'] for m in prev.get('docker', {}).get('mounts', []) if m['Type'] == 'volume'}
        new_volumes = {m['Destination']: m['Name'] for m in a.get('docker', {}).get('mounts', []) if m['Type'] == 'volume'}
        assert old_volumes == new_volumes, '卷身份发生变化：' + a['id']
        assert a.get('docker', {}).get('running'), '执行容器未就绪：' + a['id']
        if prev['mcp_servers']:
            assert set(prev['mcp_servers']) <= set(a['mcp_servers']), 'MCP 服务减少：' + a['id']
report_path = dest / (mode + '.json')
report_path.write_text(json.dumps(report, indent=2, ensure_ascii=False))
print(json.dumps({'mode': mode, 'agents': len(agents), 'counts': report['counts'],
                  'mcp': [len(a['mcp_servers']) for a in report['agents']], 'report': str(report_path)}))
