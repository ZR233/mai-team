#!/usr/bin/env python3
"""仅导出明确允许公开的校验信息，不读取凭据、会话正文或容器环境。"""
import datetime
import hashlib
import json
import os
import pathlib
import sqlite3
import subprocess
import urllib.request

assert os.geteuid() == 0
root = pathlib.Path('/mnt/docker_nvme/mai-team')
deployments = root / 'deployments'
evidence = deployments / 'shared-references-evidence'
repair = deployments / 'shared-references-history-repair'
backup = pathlib.Path((root / 'backups/shared-references-latest.txt').read_text().strip())


def read(path):
    return json.loads(path.read_text())


def api(path):
    with urllib.request.urlopen('http://127.0.0.1:8080' + path, timeout=30) as response:
        return json.load(response)


report = {'exported_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
          'health': api('/health'), 'installed': read(deployments / 'shared-references-installed.json'),
          'projection': read(deployments / 'shared-references-bind-projection.json'),
          'projects': [{key: project[key] for key in ('id', 'name', 'owner', 'repo', 'auto_review_enabled')}
                       for project in api('/projects')],
          'reference_status': api('/projects/760ebfac-5002-4574-84e4-acb0cca4d3ac/references/status'),
          'migration': {name: read(backup / ('migration-' + name + '.json')) for name in ('before', 'migrate', 'after')},
          'backup': read(backup / 'backup-manifest.json'),
          'backup_integrity': read(backup / 'integrity.json'),
          'native_oci_verification': read(backup / 'archive-verification.json'),
          'backup_image_load': read(evidence / 'backup-image-load.json'),
          'history_repair': {name: read(repair / (name + '.json')) for name in ('repair', 'sessions', 'verification')},
          'source': read(evidence / 'linux-source.json'),
          'mounts_after_resume': read(evidence / 'linux-mounts-after-resume.json'),
          'current_reviewer_inheritance': read(evidence / 'current-reviewer-inheritance.json'),
          'reviews_restored': read(evidence / 'reviews-restored.json'),
          'held_review_outcome': read(evidence / 'held-review-outcome.json'),
          'current_review_outcome': read(evidence / 'resumed-current-review.json')}
for name in ('before', 'after'):
    original = read(evidence / (name + '.json'))
    report[name] = {'time_utc': original['time_utc'], 'health': original['health'],
                    'counts': original['counts'], 'schema': original['schema'],
                    'agents': [{**{key: agent[key] for key in ('id', 'project_id', 'container_id', 'active_turn', 'history_sha256', 'history_shape', 'history_items')}, 'mcp_server_count': len(agent['mcp_servers'])}
                               for agent in original['agents']]}
assert report['health']['ok']
pid = subprocess.check_output(['systemctl', 'show', 'mai-server.service', '--value', '-p', 'MainPID'], text=True).strip()
with pathlib.Path('/proc', pid, 'exe').open('rb') as running:
    report['running_binary_sha256'] = hashlib.file_digest(running, 'sha256').hexdigest()
assert report['running_binary_sha256'] == report['installed']['binary_sha256']
assert all(project['auto_review_enabled'] for project in report['projects'])
assert report['reference_status']['state'] == 'applied'
assert report['reference_status']['counts']['failed'] == report['reference_status']['counts']['waiting'] == 0
assert report['current_review_outcome']['terminal'], '等待恢复后的当前 head 审查终态'
assert all(job['status'] != 'failed' for job in report['current_review_outcome']['timeline'][-1]['jobs'])
with sqlite3.connect('file:' + str(root / 'data/mai-team.sqlite3') + '?mode=ro', uri=True) as db:
    report['review_submission_receipts'] = []
    for job in report['current_review_outcome']['timeline'][-1]['jobs']:
        raw = db.execute('SELECT submission_receipt_json FROM project_review_jobs WHERE id=?', (job['id'],)).fetchone()[0]
        if raw:
            receipt = json.loads(raw)
            report['review_submission_receipts'].append({key: receipt[key] for key in ('github_review_id', 'event', 'head_sha', 'html_url', 'submitted_at')})
assert not report['history_repair']['verification']['missing_history_primary_keys']
assert len(report['history_repair']['verification']['restored_session_checks']) == 5
print(json.dumps(report, ensure_ascii=False, indent=2))
