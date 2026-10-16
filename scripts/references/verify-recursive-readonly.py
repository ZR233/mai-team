#!/usr/bin/env python3
"""只在独立 Docker 和验收夹具上验证嵌套子挂载的递归只读。"""
import json,os,pathlib,subprocess,uuid
ROOT=pathlib.Path('/mnt/docker_nvme/mai-references-acceptance/20261015-shared-sources')
assert os.geteuid()==0
SOCKET='unix:///run/mai-reference-acceptance-docker/docker.sock'
D=['docker','--host',SOCKET]
source=ROOT/'recursive-capability'/uuid.uuid4().hex
(source/'nested').mkdir(parents=True)
(source/'parent.txt').write_text('parent-original\n')
name='mai-reference-rro-'+source.name[:12]
evidence={'socket':SOCKET,'source':str(source),'container':name,'checks':[]}
mounted=False
cid=None
try:
 subprocess.run(['mount','-t','tmpfs','-o','size=1m','tmpfs',str(source/'nested')],check=True)
 mounted=True
 (source/'nested/child.txt').write_text('child-original\n')
 result=subprocess.run(D+['run','-d','--name',name,'--network','none','--cap-drop','ALL','--security-opt','no-new-privileges:true','--mount',f'type=bind,src={source},dst=/references/probe,readonly,bind-recursive=readonly,bind-propagation=rprivate','sha256:46efd2ec0c7102db5df390e1aef998c7de4666d9db3f7e5dfbfbcae532d30e5b','sh','-c','sleep infinity'],check=True,text=True,capture_output=True)
 cid=result.stdout.strip()
 for label,cmd,want in [
  ('both-readable','cat /references/probe/parent.txt /references/probe/nested/child.txt',0),
  ('parent-denied','echo bad >> /references/probe/parent.txt',1),
  ('nested-denied','echo bad >> /references/probe/nested/child.txt',1),
  ('nested-create-denied','touch /references/probe/nested/created',1),
 ]:
  r=subprocess.run(D+['exec',cid,'sh','-c',cmd],text=True,capture_output=True)
  ok=r.returncode==0 if want==0 else r.returncode!=0 and 'Read-only file system' in r.stderr
  evidence['checks'].append({'label':label,'passed':ok,'exit':r.returncode,'stdout':r.stdout,'stderr':r.stderr})
 evidence['mountinfo']=subprocess.check_output(D+['exec',cid,'sh','-c','cat /proc/self/mountinfo'],text=True).splitlines()
 evidence['mountinfo']=[line for line in evidence['mountinfo'] if '/references/probe' in line]
 evidence['originals_unchanged']=(source/'parent.txt').read_text()=='parent-original\n' and (source/'nested/child.txt').read_text()=='child-original\n'
 assert all(x['passed'] for x in evidence['checks']) and evidence['originals_unchanged']
finally:
 if cid:subprocess.run(D+['rm','-f',cid],stdout=subprocess.DEVNULL,check=True)
 if mounted:subprocess.run(['umount',str(source/'nested')],check=True)
 evidence['container_removed']=cid is not None
 (ROOT/'evidence/recursive-readonly.json').write_text(json.dumps(evidence,indent=2))
print(json.dumps({'pass':all(x['passed'] for x in evidence['checks']),'checks':len(evidence['checks'])}))
