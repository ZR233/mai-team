#!/usr/bin/env python3
"""在旧版真实空闲窗口暂停审查，保留原开关，拒绝把取消算作排空。"""
import datetime,json,os,pathlib,sqlite3,time,urllib.request,sys
assert os.geteuid()==0
os.umask(0o077)
root=pathlib.Path('/mnt/docker_nvme/mai-team')
state_path=root/'deployments/shared-references-original-review.json'
state_path.parent.mkdir(parents=True,exist_ok=True,mode=0o700)
base='http://127.0.0.1:8080'
def api(path,body=None):
 data=None if body is None else json.dumps(body).encode()
 request=urllib.request.Request(base+path,data=data,method='GET' if body is None else 'PATCH',headers={'Content-Type':'application/json'})
 with urllib.request.urlopen(request,timeout=20) as response:return json.load(response)
def jobs():
 with sqlite3.connect('file:'+str(root/'data/mai-team.sqlite3')+'?mode=ro',uri=True) as db:
  return list(db.execute('select id,project_id,status from project_review_jobs'))
projects=api('/projects')
if not state_path.exists():
 state={'started_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),'projects':[{k:p[k] for k in ('id','name','auto_review_enabled')} for p in projects],'cancelled_before':[r[0] for r in jobs() if r[2]=='cancelled']}
 state_path.write_text(json.dumps(state,indent=2))
else: state=json.loads(state_path.read_text())
assert {p['id'] for p in projects} == {p['id'] for p in state['projects']}, '项目集合发生变化，需要先刷新维护范围'
nonterminal={'queued','preparing','running','retry_waiting','submission_pending','reconciling'}
deadline=time.monotonic()+60
for original in state['projects']:
 if not original['auto_review_enabled']:continue
 while True:
  current=next(p for p in api('/projects') if p['id']==original['id'])
  if not current['auto_review_enabled']:break
  busy=[r for r in jobs() if r[1]==original['id'] and r[2] in nonterminal]
  if not busy:
   prior_cancelled={r[0] for r in jobs() if r[1]==original['id'] and r[2]=='cancelled'}
   api('/projects/'+original['id'],{'auto_review_enabled':False})
   cancelled={r[0] for r in jobs() if r[1]==original['id'] and r[2]=='cancelled'}-prior_cancelled
   assert not cancelled, '关闭开关时发生新的取消，必须诊断：'+str(sorted(cancelled))
   print('PAUSED '+original['name'],flush=True)
   break
  if time.monotonic()>deadline:
   print(json.dumps({'state':'waiting','project':original['name'],'jobs':[{'id':r[0],'status':r[2]} for r in busy]},ensure_ascii=False),flush=True)
   sys.exit(75)
  time.sleep(.2)
active=[]
for agent in api('/agents'):
 snapshot=api('/threads/'+agent['id'])
 if snapshot.get('activeTurn'):active.append(agent['id'])
assert not active, '普通会话仍有 Turn，暂不进入停服：'+str(active)
state['drained_at']=datetime.datetime.now(datetime.timezone.utc).isoformat()
state['state']='drained'
state_path.write_text(json.dumps(state,indent=2))
print(json.dumps({'state':'drained','state_file':str(state_path),'projects':state['projects']},ensure_ascii=False))
