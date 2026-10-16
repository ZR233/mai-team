#!/usr/bin/env python3
"""停服后备份当前产品及其恢复所需资源；不删除任何资源。"""
import datetime, hashlib, json, os, pathlib, shutil, sqlite3, stat, subprocess, sys, tarfile
ROOT = pathlib.Path('/mnt/docker_nvme/mai-team')
SOCKET = 'unix:///var/run/docker.sock'
assert os.geteuid() == 0
os.umask(0o077)

def run(args, **kw):
    return subprocess.run(args, check=True, **kw)

def docker(*args):
    return subprocess.check_output(['docker', '--host', SOCKET, *args], text=True)

state = subprocess.check_output(['systemctl','show','mai-server.service','--property=ActiveState','--value'],text=True).strip()
pid = subprocess.check_output(['systemctl','show','mai-server.service','--property=MainPID','--value'],text=True).strip()
check_only = sys.argv[1:] == ['--check']
resources_only = len(sys.argv) == 3 and sys.argv[1] == '--resume-resources'
resume = len(sys.argv) == 3 and sys.argv[1] in ('--resume', '--resume-resources')
assert not sys.argv[1:] or check_only or resume, '仅接受 --check、--resume / --resume-resources <备份目录> 或不带参数执行备份'
if not check_only:
    assert state in ('inactive','failed') and pid == '0', '必须先排空并完全停服'
stamp = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')
backup = ROOT / 'backups' / ('shared-references-' + stamp)
if resume:
    backup = pathlib.Path(sys.argv[2]).resolve()
    assert backup.parent == ROOT/'backups' and backup.name.startswith('shared-references-')
    assert (backup/'data-inventory.json').is_file() and not (backup/'integrity.json').exists()
    stamp = backup.name.removeprefix('shared-references-')
database = ROOT / 'data/mai-team.sqlite3'
if resume:
    # SQLite 的只读 WAL 读取仍可能触碰 SHM 时间戳；续备份检查只打开临时副本。
    check_dir = backup/'resume-counts'
    check_dir.mkdir(exist_ok=True, mode=0o700)
    for suffix in ('', '-wal', '-shm'):
        source = pathlib.Path(str(database)+suffix)
        if source.exists():
            shutil.copy2(source, check_dir/source.name)
    database = check_dir/database.name
with sqlite3.connect('file:' + str(database) + '?mode=ro', uri=True) as db:
    schema = db.execute("select value from settings where key='toasty_schema_version'").fetchone()[0]
    assert schema == '37', '本工具只承担首次 37→38 迁移备份；38 的操作收据须另行纳入备份'
    agent_containers = dict(db.execute('select id,container_id from agents'))
    agents = set(agent_containers)
    projects = set(x[0] for x in db.execute('select id from projects'))
    counts = {t: db.execute('select count(*) from '+t).fetchone()[0] for t in ('projects','agents','product_events','project_review_runs')}
db.close()
ids = docker('ps', '-aq').split()
containers = json.loads(docker('inspect', *ids)) if ids else []
owned = []
for container in containers:
    labels = container['Config'].get('Labels') or {}
    if labels.get('mai.team.managed') == 'true' and (labels.get('mai.team.agent') in agents or (labels.get('mai.team.project') in projects and not labels.get('mai.team.agent'))):
        owned.append(container)
for agent, stored in agent_containers.items():
    primary = [c for c in owned if (c['Config'].get('Labels') or {}).get('mai.team.agent') == agent and (c['Config'].get('Labels') or {}).get('mai.team.sidecar') != 'true']
    assert len(primary) == 1 and stored and primary[0]['Id'].startswith(stored), 'Agent 主容器缺失、重复或与数据库不一致：'+agent
all_volume_names = docker('volume', 'ls', '--format', '{{.Name}}').splitlines()
all_volumes = []
for offset in range(0, len(all_volume_names), 128):
    all_volumes.extend(json.loads(docker('volume', 'inspect', *all_volume_names[offset:offset+128])))
volume_names = {m['Name'] for c in owned for m in c['Mounts'] if m['Type']=='volume'}
for volume in all_volumes:
    labels = volume.get('Labels') or {}
    if labels.get('mai.team.managed') == 'true' and (labels.get('mai.team.agent') in agents or (labels.get('mai.team.kind') == 'project-cache' and labels.get('mai.team.project') in projects)):
        volume_names.add(volume['Name'])
volume_names.update('mai-team-project-'+project+'-cache' for project in projects if 'mai-team-project-'+project+'-cache' in all_volume_names)
volume_metadata = [v for v in all_volumes if v['Name'] in volume_names]
assert len(volume_metadata) == len(volume_names), '备份卷身份不完整'
if check_only:
    print(json.dumps({'check':'ok','schema':schema,'agents':len(agents),'containers':len(owned),'volumes':len(volume_metadata),'counts':counts}))
    sys.exit(0)
configuration_copies = []
if resume:
    recorded = json.loads((backup/'container-inventory.json').read_text())
    assert {c['Id'] for c in owned} == {c['Id'] for c in recorded}, '停服后的资源身份发生改变'
    assert not any(c['State']['Running'] for c in owned), '恢复备份时容器必须保持停止'
    assert json.loads((backup/'counts.json').read_text()) == counts
    owned = recorded
    resources_quiesced_at = datetime.datetime.now(datetime.timezone.utc).isoformat()
    for source in ('/etc/mai-server','/etc/systemd/system/mai-server.service','/etc/systemd/system/mai-server.service.d'):
        if pathlib.Path(source).exists():
            configuration_copies.append((pathlib.Path(source), backup/'etc'/source.lstrip('/')))
else:
    backup.mkdir(parents=True, mode=0o700)
    os.chmod(backup.parent, 0o700)
    (backup/'container-inventory.json').write_text(json.dumps(owned, indent=2))
    (backup/'selected-resources.json').write_text(json.dumps([c['Id'] for c in owned], indent=2))
    (backup/'counts.json').write_text(json.dumps(counts, indent=2))
    (backup/'service-unit.txt').write_text(subprocess.check_output(['systemctl','cat','mai-server.service'],text=True))
    configuration_copies = []
    for source in ('/etc/mai-server','/etc/systemd/system/mai-server.service','/etc/systemd/system/mai-server.service.d'):
        if pathlib.Path(source).exists():
            destination = backup / 'etc' / source.lstrip('/')
            destination.parent.mkdir(parents=True, exist_ok=True)
            run(['cp','-a', source, str(destination)])
            configuration_copies.append((pathlib.Path(source), destination))
    shutil.copy2(ROOT/'bin/mai-server', backup/'mai-server')
    review_settings = ROOT/'deployments/shared-references-original-review.json'
    if review_settings.exists():
        shutil.copy2(review_settings, backup/'original-review-settings.json')
    held_queue = ROOT/'deployments/shared-references-held-queue.json'
    if held_queue.exists():
        shutil.copy2(held_queue, backup/'held-queue.json')
    running = [c['Id'] for c in owned if c['State']['Running']]
    if running:
        run(['docker','--host',SOCKET,'stop','--time','30',*running],stdout=subprocess.DEVNULL)
    resources_quiesced_at = datetime.datetime.now(datetime.timezone.utc).isoformat()
    run(['cp','-a','--reflink=auto',str(ROOT/'data'),str(backup/'data')])
def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as source:
        for chunk in iter(lambda: source.read(1024*1024), b''):
            digest.update(chunk)
    return digest.hexdigest()

def tree_inventory(path, *, content=True):
    digest = hashlib.sha256()
    files = size = 0
    for base, dirs, names in os.walk(path, followlinks=False):
        for name in sorted(dirs + names):
            item = pathlib.Path(base) / name
            st = item.lstat()
            target = os.readlink(item) if item.is_symlink() else None
            record = [str(item.relative_to(path)), st.st_mode, st.st_uid, st.st_gid, 0 if stat.S_ISDIR(st.st_mode) else st.st_size, st.st_mtime_ns, target, sha256_file(item) if content and stat.S_ISREG(st.st_mode) else None]
            digest.update(json.dumps(record,ensure_ascii=False).encode()+b'\n')
            files += 1
            if stat.S_ISREG(st.st_mode): size += st.st_size
        dirs.sort()
    return {'entries':files, 'regular_bytes':size, 'metadata_sha256':digest.hexdigest()}
for source, destination in configuration_copies:
    if source.is_dir():
        assert tree_inventory(source) == tree_inventory(destination), '部署配置备份不一致：'+str(source)
    else:
        assert sha256_file(source) == sha256_file(destination), '服务 unit 备份不一致'
if resources_only:
    # 完整内容摘要已经持久化；保持停服，复核全部元数据及数据库三件套后只续资源阶段。
    assert tree_inventory(ROOT/'data', content=False) == tree_inventory(backup/'data', content=False)
    for suffix in ('', '-wal', '-shm'):
        source = ROOT/'data'/('mai-team.sqlite3'+suffix)
        if source.exists():
            assert sha256_file(source) == sha256_file(backup/'data'/source.name)
    data_inventory = json.loads((backup/'data-inventory.json').read_text())
else:
    data_inventory = tree_inventory(ROOT/'data')
    if resume:
        assert data_inventory == json.loads((backup/'data-inventory.json').read_text()), '停服数据在备份中断后发生改变'
    else:
        assert data_inventory == tree_inventory(backup/'data'), '产品数据备份元数据不一致'
    (backup/'data-inventory.json').write_text(json.dumps(data_inventory,indent=2))
images = set(c['Image'] for c in owned)
snapshots = {}
for c in owned:
    tag = 'mai-team-rollback:' + stamp.lower() + '-' + c['Id'][:12]
    if not resume:
        docker('commit',c['Id'],tag)
    image = docker('image','inspect','--format','{{.Id}}',tag).strip()
    assert image.startswith('sha256:') and len(image) == 71, '镜像 ID 必须来自 inspect 的结构化字段'
    images.add(image)
    snapshots[c['Id']] = {'image_id':image,'tag':tag,'name':c['Name']}
    (backup/'snapshots.json').write_text(json.dumps(snapshots,indent=2))
(backup/'images.json').write_text(json.dumps(sorted(images),indent=2))
if not resources_only or not (backup/'images.tar').exists():
    run(['docker','--host',SOCKET,'image','save','-o',str(backup/'images.tar'),*sorted(images)])
with tarfile.open(backup/'images.tar', 'r:') as archive:
    saved = json.load(archive.extractfile('manifest.json'))
    saved_ids = set()
    for image in saved:
        saved_ids.add('sha256:'+hashlib.sha256(archive.extractfile(image['Config']).read()).hexdigest())
        for layer in image['Layers']:
            archive.getmember(layer)
    if 'index.json' in archive.getnames():
        # containerd image store 的镜像 ID 是 OCI descriptor 摘要，逐层核对实际 blob。
        pending_descriptors = json.load(archive.extractfile('index.json'))['manifests']
        verified = set()
        while pending_descriptors:
            descriptor = pending_descriptors.pop()
            digest = descriptor['digest']
            if digest in verified:
                continue
            algorithm, hexadecimal = digest.split(':', 1)
            assert algorithm == 'sha256' and len(hexadecimal) == 64
            member = archive.getmember('blobs/sha256/'+hexadecimal)
            assert member.size == descriptor['size']
            with archive.extractfile(member) as blob:
                assert hashlib.file_digest(blob, 'sha256').hexdigest() == hexadecimal
            verified.add(digest)
            if descriptor['mediaType'].endswith('+json'):
                document = json.load(archive.extractfile(member))
                pending_descriptors.extend(document.get('manifests', []))
                pending_descriptors.extend(document.get('layers', []))
                if isinstance(document.get('config'), dict):
                    pending_descriptors.append(document['config'])
        saved_ids.update(verified)
    assert images <= saved_ids, '镜像归档没有覆盖全部恢复镜像'

volumes = sorted(volume_names)
(backup/'volume-inventory.json').write_text(json.dumps(volume_metadata,indent=2))
volume_integrity = {}
for volume in volume_metadata:
    destination = backup/'volumes'/volume['Name']
    destination.parent.mkdir(parents=True,exist_ok=True)
    if not destination.exists():
        run(['cp','-a','--reflink=auto',volume['Mountpoint'],str(destination)])
    inventory = tree_inventory(pathlib.Path(volume['Mountpoint']))
    assert inventory == tree_inventory(destination), '卷备份内容不一致：'+volume['Name']
    volume_integrity[volume['Name']] = inventory
# 当前资源之外的容器仅记录，部署脚本不主动停止或清理；启动回收由产品的所有权规则决定。
summary = {'backup':str(backup),'data_time_utc':stamp,'resources_quiesced_at':resources_quiesced_at,'completed_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),'counts':counts,'data_inventory':data_inventory,'old_binary_sha256':sha256_file(backup/'mai-server'),'containers':len(owned),'volumes':len(volumes),'image_ids':sorted(images),'preserved_unmanaged_container_ids':[c['Id'] for c in containers if (c['Config'].get('Labels') or {}).get('mai.team.managed')!='true']}
assert summary['old_binary_sha256'] == sha256_file(ROOT/'bin/mai-server'), '旧二进制备份不一致'
integrity = {'volumes':volume_integrity,'images.tar':{'size':(backup/'images.tar').stat().st_size,'sha256':sha256_file(backup/'images.tar')},'configuration':tree_inventory(backup/'etc')}
(backup/'integrity.json').write_text(json.dumps(integrity,indent=2))
(backup/'backup-manifest.json').write_text(json.dumps(summary,indent=2))
run(['sync','-f',str(backup)])
latest = ROOT/'backups/shared-references-latest.txt'
pending = latest.with_suffix('.pending')
pending.write_text(str(backup)+'\n')
run(['sync','-f',str(pending)])
os.replace(pending, latest)
run(['sync','-f',str(latest.parent)])
print(json.dumps(summary))
