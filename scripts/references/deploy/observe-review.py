#!/usr/bin/env python3
"""只读观察维护期间保存的审查 Job，到达终态后保存恢复证据。"""
import datetime, json, os, pathlib, sqlite3, sys, time, urllib.request

assert os.geteuid() == 0
os.umask(0o077)
root = pathlib.Path('/mnt/docker_nvme/mai-team')
held = json.loads((root/'deployments/shared-references-held-queue.json').read_text())
assert held['state'] == 'restored'
ids = [job['id'] for job in held['jobs']]
assert sys.argv[1:] in ([], ['--follow-replacements'])
follow_replacements = sys.argv[1:] == ['--follow-replacements']
output_name = 'resumed-current-review.json' if follow_replacements else 'held-review-outcome.json'
active = {'queued', 'preparing', 'running', 'retry_waiting', 'submission_pending', 'reconciling'}
timeline = []
deadline = time.monotonic() + 1800
previous = None
while True:
    with urllib.request.urlopen('http://127.0.0.1:8080/health', timeout=20) as response:
        assert json.load(response)['ok']
    db = sqlite3.connect('file:'+str(root/'data/mai-team.sqlite3')+'?mode=ro', uri=True)
    db.row_factory = sqlite3.Row
    try:
        rows = [dict(db.execute('SELECT * FROM project_review_jobs WHERE id=?', (jid,)).fetchone()) for jid in ids]
        if follow_replacements:
            for index, row in enumerate(rows):
                if row['status'] == 'superseded':
                    latest = dict(db.execute('SELECT * FROM project_review_jobs WHERE project_id=? AND pr=? ORDER BY created_at DESC LIMIT 1', (row['project_id'], row['pr'])).fetchone())
                    if latest['id'] != row['id']:
                        ids[index] = latest['id']
                        rows[index] = latest
    finally:
        db.close()
    summary = [{**{k: row[k] for k in ('id', 'project_id', 'pr', 'head_sha', 'status', 'skip_reason', 'attempt_count', 'active_run_id', 'finished_at')}, 'has_submission_receipt': row['submission_receipt_json'] is not None} for row in rows]
    if summary != previous:
        event = {'at': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'jobs': summary}
        timeline.append(event)
        print(json.dumps(event), flush=True)
        previous = summary
    report = {'health': 'ok', 'timeline': timeline, 'terminal': all(row['status'] not in active for row in rows)}
    (root/'deployments/shared-references-evidence'/output_name).write_text(json.dumps(report, indent=2))
    if report['terminal']:
        assert not any(row['status'] == 'failed' for row in rows), '审查已失败，检查真实运行日志'
        break
    assert time.monotonic() < deadline, '保存的审查尚未到达终态，继续检查排队原因'
    time.sleep(5)
