#!/usr/bin/env python3
"""停服维护只暂存尚未领取的队列项；应用完成后恢复原 Job 身份与调度时间。"""
import datetime, json, os, pathlib, sqlite3, sys, urllib.request

assert os.geteuid() == 0
os.umask(0o077)
mode = sys.argv[1]
assert mode in ('prepare', 'restore')
root = pathlib.Path('/mnt/docker_nvme/mai-team')
deploy = root / 'deployments'
state_file = deploy / 'shared-references-held-queue.json'
original_file = deploy / 'shared-references-original-review.json'
database = root / 'data/mai-team.sqlite3'
base = 'http://127.0.0.1:8080'
active_states = ('queued', 'preparing', 'running', 'retry_waiting', 'submission_pending', 'reconciling')

def api(path, body=None):
    request = urllib.request.Request(base + path, data=None if body is None else json.dumps(body).encode(),
        method='GET' if body is None else 'PATCH', headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)

def rows():
    with sqlite3.connect('file:' + str(database) + '?mode=ro', uri=True) as db:
        db.row_factory = sqlite3.Row
        return [dict(r) for r in db.execute('SELECT * FROM project_review_jobs')]

def idle():
    for agent in api('/agents'):
        assert not api('/threads/' + agent['id']).get('activeTurn'), '有活动 Turn，拒绝维护'

def unclaimed(job):
    return job['attempt_count'] == 0 and job['lease_owner'] is None and job['lease_expires_at'] is None and job['active_run_id'] is None

if mode == 'prepare':
    assert not state_file.exists(), '已暂存队列，按原收据继续，不能覆盖'
    original = json.loads(original_file.read_text())
    idle()
    before = rows()
    pending = [r for r in before if r['status'] in active_states]
    assert pending and all(r['status'] == 'queued' and unclaimed(r) for r in pending), '只允许暂存从未领取的 queued 工作'
    projects = api('/projects')
    assert {p['id'] for p in projects} == {p['id'] for p in original['projects']}
    state = {'state': 'prepared', 'prepared_at': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'jobs': pending}
    state_file.write_text(json.dumps(state, indent=2))
    with state_file.open('rb') as stream:
        os.fsync(stream.fileno())
    already_cancelled = {r['id'] for r in before if r['status'] == 'cancelled'}
    for project in projects:
        if project['auto_review_enabled']:
            api('/projects/' + project['id'], {'auto_review_enabled': False})
    after = rows()
    newly_cancelled = [r for r in after if r['status'] == 'cancelled' and r['id'] not in already_cancelled]
    # 旧版关闭开关把 queued 标记 cancelled；保留前像，不能把它冒充执行完成。
    state['paused_rows'] = newly_cancelled
    state_file.write_text(json.dumps(state, indent=2))
    assert {r['id'] for r in newly_cancelled} == {r['id'] for r in pending}, '暂停期间工作集合发生变化，先核验前像和 paused_rows'
    assert all(unclaimed(r) for r in newly_cancelled), '暂停期间有任务被领取，拒绝继续'
    assert not [r for r in after if r['status'] in active_states]
    idle()
    state['state'] = 'held'
    state_file.write_text(json.dumps(state, indent=2))
    original.update(state='drained', drained_at=datetime.datetime.now(datetime.timezone.utc).isoformat(), held_queue=str(state_file))
    original_file.write_text(json.dumps(original, indent=2))
    print(json.dumps({'state': 'drained', 'queued_work_held': len(pending), 'receipt': str(state_file)}))
else:
    state = json.loads(state_file.read_text())
    assert state['state'] == 'held'
    status = api('/projects/760ebfac-5002-4574-84e4-acb0cca4d3ac/references/status')
    assert status['state'] == 'applied' and status['counts']['failed'] == status['counts']['waiting'] == 0
    assert all(not p['auto_review_enabled'] for p in api('/projects'))
    with sqlite3.connect(database) as db:
        db.row_factory = sqlite3.Row
        db.execute('BEGIN IMMEDIATE')
        for old in state['jobs']:
            current = dict(db.execute('SELECT * FROM project_review_jobs WHERE id=?', (old['id'],)).fetchone())
            assert current['status'] == 'cancelled' and unclaimed(current)
            assert all(current[k] == old[k] for k in ('project_id', 'head_sha', 'pr', 'created_at', 'next_attempt_at'))
            changed = db.execute('UPDATE project_review_jobs SET status=?, finished_at=?, updated_at=? WHERE id=? AND status=?',
                (old['status'], old['finished_at'], datetime.datetime.now(datetime.timezone.utc).isoformat(), old['id'], 'cancelled')).rowcount
            assert changed == 1
    state.update(state='restored', restored_at=datetime.datetime.now(datetime.timezone.utc).isoformat())
    state_file.write_text(json.dumps(state, indent=2))
    print(json.dumps({'state': 'restored', 'jobs': [r['id'] for r in state['jobs']]}))
