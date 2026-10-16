#!/usr/bin/env python3
"""已停服且成套备份完成后，离线迁移并原子安装已验收产物。"""
import grp,hashlib,json,os,pathlib,sqlite3,subprocess,sys
assert os.geteuid()==0
os.umask(0o077)
commit,expected_sha,expected_migrator_sha=sys.argv[1:]
root=pathlib.Path('/mnt/docker_nvme/mai-team')
stage=root/'deployments/shared-references-staging'
backup=pathlib.Path((root/'backups/shared-references-latest.txt').read_text().strip())
assert backup.parent == root/'backups'
manifest=json.loads((backup/'backup-manifest.json').read_text())
assert (backup/'integrity.json').is_file()
assert subprocess.check_output(['systemctl','show','mai-server','--property=ActiveState','--value'],text=True).strip() in ('inactive','failed')
assert subprocess.check_output(['systemctl','show','mai-server','--property=MainPID','--value'],text=True).strip()=='0'
sha=hashlib.sha256((stage/'mai-server').read_bytes()).hexdigest()
assert sha == expected_sha, '待安装二进制摘要不一致'
assert hashlib.sha256((stage/'mai-migrate-references').read_bytes()).hexdigest() == expected_migrator_sha
database=root/'data/mai-team.sqlite3'
reports={}
for name,args in [('before',['check']),('migrate',['migrate']),('after',['check'])]:
 command=[str(stage/'mai-migrate-references'),*args,'--database',str(database)]
 if name=='migrate':command+=['--backup',str(backup/'schema37-before-migrate.sqlite3')]
 output=subprocess.check_output(command,text=True)
 report=json.loads(output);reports[name]=report
 (backup/('migration-'+name+'.json')).write_text(json.dumps(report,indent=2))
 assert report['integrity']=='ok'
assert reports['before']['schema_after']=='37'
assert reports['after']['schema_after']=='38'
for field in ['projects','agents','product_events','review_runs']:
 assert reports['before'][field]==reports['after'][field], '迁移损失数据：'+field
policy=pathlib.Path('/etc/mai-server/reference-sources.toml')
assert not policy.exists(), '首次策略文件已存在，拒绝覆盖'
policy.write_text('''# 部署管理员持有；Web UI 不能扩大允许根。
allowed_roots = ["/mnt/docker_nvme/mai-references/linux/7b20d05fd38de8e0be168956f6fd241d750c7d69"]
trusted_owner_uids = [0]
max_bindings_per_project = 16
''')
service_gid=grp.getgrnam('mai-server').gr_gid
os.chown(policy,0,service_gid);os.chmod(policy,0o640)
os.chown(policy.parent,0,service_gid);os.chmod(policy.parent,0o750)
drop=pathlib.Path('/etc/systemd/system/mai-server.service.d/50-reference-sources.conf')
assert not drop.exists(), 'drop-in 已存在，拒绝覆盖'
drop.parent.mkdir(parents=True,exist_ok=True)
drop.write_text('[Service]\nEnvironment=MAI_REFERENCE_POLICY=/etc/mai-server/reference-sources.toml\n')
os.chmod(drop,0o644)
pending=root/'bin/mai-server.new'
subprocess.run(['install','-o','root','-g','root','-m','0755',str(stage/'mai-server'),str(pending)],check=True)
assert hashlib.sha256(pending.read_bytes()).hexdigest() == expected_sha
with pending.open('rb') as f:os.fsync(f.fileno())
os.replace(pending,root/'bin/mai-server')
subprocess.run(['sync','-f',str(root/'bin')],check=True)
record={'commit':commit,'binary_sha256':sha,'migration':reports,'backup':str(backup),'data_time_utc':manifest['data_time_utc'],'linux_commit':'7b20d05fd38de8e0be168956f6fd241d750c7d69'}
(root/'deployments/shared-references-installed.json').write_text(json.dumps(record,indent=2))
subprocess.run(['systemctl','daemon-reload'],check=True)
subprocess.run(['systemctl','start','mai-server.service'],check=True)
print(json.dumps(record))
