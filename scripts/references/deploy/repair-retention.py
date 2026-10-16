#!/usr/bin/env python3
"""停服补回发布期间被既有保留策略删除的历史；只插入缺失记录。"""
import datetime
import hashlib
import json
import os
import pathlib
import re
import shutil
import sqlite3
import subprocess
import sys
import tomllib

assert os.geteuid() == 0
os.umask(0o077)
root = pathlib.Path('/mnt/docker_nvme/mai-team')
backup = root / 'backups/shared-references-20261016T032923Z'
directory = root / 'deployments/shared-references-history-repair'
directory.mkdir(mode=0o700, exist_ok=True)
database = root / 'data/mai-team.sqlite3'
mode = sys.argv[1]
assert mode in ('repair', 'verify')


def quoted(value):
    return '"' + value.replace('"', '""') + '"'


def rows_digest(db, table):
    rows = db.execute('SELECT * FROM ' + quoted(table) + ' ORDER BY rowid').fetchall()
    return hashlib.sha256(repr(rows).encode()).hexdigest()


if mode == 'repair':
    assert subprocess.run(['systemctl', 'is-active', '--quiet', 'mai-server']).returncode != 0
    assert not (directory / 'repair.json').exists(), '已有修复收据，使用 verify 核验'
    assert not (directory / 'before.sqlite3').exists(), '已有修复前像，先核验失败点'
    with sqlite3.connect(database) as source, sqlite3.connect(directory / 'before.sqlite3') as target:
        source.backup(target)
    config = root / 'data/config.toml'
    shutil.copy2(config, directory / 'config-before.toml')
    original = config.read_text()
    old_retention = tomllib.loads(original)['retention']
    section = re.search(r'(?ms)^\[retention\][^\n]*\n.*?(?=^\[|\Z)', original)
    assert section
    edited = section.group()
    names = ('review_history_days', 'product_events_days', 'agent_logs_days', 'tool_traces_days', 'tool_output_days')
    for name in names:
        edited, count = re.subn(r'(?m)^(' + name + r'\s*=\s*)\d+', r'\g<1>36500', edited)
        assert count == 1, name
    updated = original[:section.start()] + edited + original[section.end():]
    expected = tomllib.loads(original)
    expected['retention'].update({name: 36500 for name in names})
    assert tomllib.loads(updated) == expected, '配置只能变更五个保留天数'
    temporary = config.with_name('config.toml.references-retention.tmp')
    stat = config.stat()
    with temporary.open('x') as output:
        output.write(updated)
        output.flush()
        os.fsync(output.fileno())
    os.chown(temporary, stat.st_uid, stat.st_gid)
    os.chmod(temporary, stat.st_mode & 0o777)
    os.replace(temporary, config)
    with sqlite3.connect(database) as db:
        db.execute('PRAGMA foreign_keys=ON')
        db.execute('ATTACH DATABASE ? AS old', (f'file:{backup}/schema37-before-migrate.sqlite3?immutable=1',))
        assert db.execute('PRAGMA integrity_check').fetchall() == [('ok',)]
        assert not db.execute('PRAGMA foreign_key_check').fetchall()
        new_tables = [r[0] for r in db.execute("SELECT name FROM main.sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' EXCEPT SELECT name FROM old.sqlite_master WHERE type='table'")]
        protected = ['projects', 'agents'] + new_tables
        protected_before = {table: rows_digest(db, table) for table in protected}
        sequence_before = db.execute('SELECT max(sequence) FROM product_events').fetchone()[0]
        restored = {}
        db.execute('BEGIN IMMEDIATE')
        db.execute('PRAGMA defer_foreign_keys=ON')
        tables = [r[0] for r in db.execute("SELECT name FROM old.sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")]
        for table in tables:
            columns = db.execute('PRAGMA old.table_info(' + quoted(table) + ')').fetchall()
            keys = [row[1] for row in columns if row[5]]
            assert keys, '历史表没有主键: ' + table
            names_sql = ','.join(quoted(row[1]) for row in columns)
            condition = ' AND '.join('n.' + quoted(key) + '=o.' + quoted(key) for key in keys)
            sql = 'INSERT INTO main.' + quoted(table) + '(' + names_sql + ') SELECT ' + ','.join('o.' + quoted(row[1]) for row in columns) + ' FROM old.' + quoted(table) + ' o WHERE NOT EXISTS(SELECT 1 FROM main.' + quoted(table) + ' n WHERE ' + condition + ')'
            changed = db.execute(sql).rowcount
            if changed:
                restored[table] = changed
        assert {table: rows_digest(db, table) for table in protected} == protected_before
        assert db.execute('SELECT max(sequence) FROM product_events').fetchone()[0] == sequence_before
        assert not db.execute('PRAGMA foreign_key_check').fetchall()
        db.commit()
        assert db.execute('PRAGMA integrity_check').fetchall() == [('ok',)]
    report = {'time_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
              'restored_rows': restored, 'existing_rows_overwritten': 0,
              'protected_tables': protected_before, 'event_sequence_high_watermark': sequence_before,
              'retention_before': old_retention, 'retention_after': expected['retention']}
    (directory / 'repair.json').write_text(json.dumps(report, indent=2))
else:
    report = {'time_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'missing_old_primary_keys': {}}
    with sqlite3.connect('file:' + str(database) + '?mode=ro', uri=True) as db:
        db.execute('ATTACH DATABASE ? AS old', (f'file:{backup}/schema37-before-migrate.sqlite3?immutable=1',))
        for (table,) in db.execute("SELECT name FROM old.sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'").fetchall():
            keys = [row[1] for row in db.execute('PRAGMA old.table_info(' + quoted(table) + ')') if row[5]]
            assert keys
            condition = ' AND '.join('n.' + quoted(key) + '=o.' + quoted(key) for key in keys)
            count = db.execute('SELECT count(*) FROM old.' + quoted(table) + ' o WHERE NOT EXISTS(SELECT 1 FROM main.' + quoted(table) + ' n WHERE ' + condition + ')').fetchone()[0]
            if count:
                report['missing_old_primary_keys'][table] = count
        report['counts'] = {table: db.execute('SELECT count(*) FROM ' + table).fetchone()[0] for table in ('projects', 'agents', 'product_events', 'project_review_runs')}
        report['integrity'] = db.execute('PRAGMA integrity_check').fetchall()
        report['foreign_key_errors'] = db.execute('PRAGMA foreign_key_check').fetchall()
        # CI watch 是调度指针，启动轮询可消费并入队或判定不再适用；历史行不能缺失。
        report['consumed_ci_watches'] = [dict(zip([column[0] for column in cursor.description], row))
            for cursor in [db.execute('SELECT o.* FROM old.project_review_ci_watches o WHERE NOT EXISTS(SELECT 1 FROM main.project_review_ci_watches n WHERE n.id=o.id)')]
            for row in cursor.fetchall()]
    report['missing_history_primary_keys'] = {table: count for table, count in report['missing_old_primary_keys'].items() if table != 'project_review_ci_watches'}
    assert not report['missing_history_primary_keys'], report['missing_history_primary_keys']
    if report['consumed_ci_watches']:
        repair = json.loads((directory / 'repair.json').read_text())
        assert len(report['consumed_ci_watches']) <= repair['restored_rows']['project_review_ci_watches']
    assert report['integrity'] == [('ok',)] and not report['foreign_key_errors']
    sessions = json.loads((directory / 'sessions.json').read_text())
    report['restored_session_checks'] = []
    for session in sessions['restored_databases']:
        relative = pathlib.Path(session['path'])
        db = sqlite3.connect('file:' + str(root / 'data' / relative) + '?mode=ro', uri=True)
        try:
            db.execute('ATTACH DATABASE ? AS old', (f'file:{backup}/data/{relative}?immutable=1',))
            for (table,) in db.execute("SELECT name FROM old.sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'").fetchall():
                assert db.execute('SELECT count(*) FROM (SELECT * FROM old.' + quoted(table) + ' EXCEPT SELECT * FROM main.' + quoted(table) + ')').fetchone()[0] == 0, str(relative) + ':' + table
            assert db.execute('PRAGMA integrity_check').fetchall() == [('ok',)]
            assert not db.execute('PRAGMA foreign_key_check').fetchall()
            report['restored_session_checks'].append({'path': str(relative), 'all_backup_rows_present': True, 'integrity': 'ok'})
        finally:
            db.close()
    (directory / 'verification.json').write_text(json.dumps(report, indent=2))
print(json.dumps(report))
