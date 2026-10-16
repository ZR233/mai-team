#!/usr/bin/env python3
"""逐字节核对既有会话库；只向发生清理的库补回缺失主键。"""
import concurrent.futures
import datetime
import hashlib
import json
import os
import pathlib
import sqlite3
import subprocess

assert os.geteuid() == 0
os.umask(0o077)
assert subprocess.run(['systemctl', 'is-active', '--quiet', 'mai-server']).returncode != 0
root = pathlib.Path('/mnt/docker_nvme/mai-team')
backup = root / 'backups/shared-references-20261016T032923Z/data'
directory = root / 'deployments/shared-references-history-repair'
assert not (directory / 'sessions.json').exists(), '已有会话修复收据，拒绝重做'


def quoted(value):
    return '"' + value.replace('"', '""') + '"'


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def audit(old):
    relative = old.relative_to(backup)
    current = root / 'data' / relative
    assert current.is_file(), str(relative)
    # 停服备份中的会话库没有待重放 WAL；若不满足，必须另行生成一致性快照。
    old_wal = old.with_name(old.name + '-wal')
    assert not old_wal.exists() or old_wal.stat().st_size == 0, str(relative)
    item = {'path': str(relative), 'backup_sha256': digest(old), 'current_sha256': digest(current)}
    if item['backup_sha256'] == item['current_sha256']:
        return item
    db = sqlite3.connect(current)
    try:
        db.execute('PRAGMA foreign_keys=ON')
        db.execute('ATTACH DATABASE ? AS old', (f'file:{old}?immutable=1',))
        assert db.execute('PRAGMA integrity_check').fetchall() == [('ok',)]
        tables = [r[0] for r in db.execute("SELECT name FROM old.sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")]
        missing = {}
        for table in tables:
            columns = db.execute('PRAGMA old.table_info(' + quoted(table) + ')').fetchall()
            keys = [row[1] for row in columns if row[5]]
            assert keys, table
            condition = ' AND '.join('n.' + quoted(key) + '=o.' + quoted(key) for key in keys)
            count = db.execute('SELECT count(*) FROM old.' + quoted(table) + ' o WHERE NOT EXISTS(SELECT 1 FROM main.' + quoted(table) + ' n WHERE ' + condition + ')').fetchone()[0]
            if count:
                missing[table] = count
        item['missing_rows_before'] = missing
        if missing:
            # 已有三个活跃 Agent 的 checkpoint 是新环境恢复结果，绝不覆盖。
            assert relative.parts[1] not in ('041f1920-2040-44b9-9e1f-b5382ddf331f', '566c6cb7-43c1-4713-b94c-d79dd3beff21', 'dd0bad96-2ef8-465b-af6c-ffacb4a78540')
            target = directory / ('before-session-' + relative.parts[1] + '.sqlite3')
            assert not target.exists()
            before_dump = {table: db.execute('SELECT * FROM ' + quoted(table)).fetchall() for table in tables}
            with sqlite3.connect(target) as snapshot:
                db.backup(snapshot)
            db.execute('BEGIN IMMEDIATE')
            db.execute('PRAGMA defer_foreign_keys=ON')
            for table in missing:
                columns = db.execute('PRAGMA old.table_info(' + quoted(table) + ')').fetchall()
                keys = [row[1] for row in columns if row[5]]
                names = ','.join(quoted(row[1]) for row in columns)
                condition = ' AND '.join('n.' + quoted(key) + '=o.' + quoted(key) for key in keys)
                changed = db.execute('INSERT INTO main.' + quoted(table) + '(' + names + ') SELECT ' + ','.join('o.' + quoted(row[1]) for row in columns) + ' FROM old.' + quoted(table) + ' o WHERE NOT EXISTS(SELECT 1 FROM main.' + quoted(table) + ' n WHERE ' + condition + ')').rowcount
                assert changed == missing[table]
            assert not db.execute('PRAGMA foreign_key_check').fetchall()
            # 验证全部旧行已回来，全部修复前当前行也仍然存在。
            for table in tables:
                assert db.execute('SELECT count(*) FROM (SELECT * FROM old.' + quoted(table) + ' EXCEPT SELECT * FROM main.' + quoted(table) + ')').fetchone()[0] == 0
                after = set(db.execute('SELECT * FROM ' + quoted(table)).fetchall())
                assert set(before_dump[table]) <= after
            db.commit()
        assert db.execute('PRAGMA integrity_check').fetchall() == [('ok',)]
        item['existing_rows_overwritten'] = 0
    finally:
        db.close()
    return item


paths = sorted((backup / 'sessions').rglob('history.sqlite'))
with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
    results = list(pool.map(audit, paths))
report = {'time_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
          'session_databases': len(results), 'byte_identical': sum(r['backup_sha256'] == r['current_sha256'] for r in results),
          'restored_databases': [r for r in results if r.get('missing_rows_before')],
          'changed_without_missing_rows': [r for r in results if r['backup_sha256'] != r['current_sha256'] and not r.get('missing_rows_before')],
          'database_hashes': results}
(directory / 'sessions.json').write_text(json.dumps(report, indent=2))
print(json.dumps({key: value for key, value in report.items() if key != 'database_hashes'}))
