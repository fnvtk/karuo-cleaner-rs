"""Opt-in live model + recoverable cleanup check, using only self-created files.
Runs the installed binary with isolated HOME/state; never purges Trash.
"""
import argparse,hashlib,json,os,pathlib,shutil,subprocess,tempfile,time
parser=argparse.ArgumentParser();parser.add_argument('--live-ai',action='store_true');args=parser.parse_args()
if not args.live_ai:raise SystemExit('Use --live-ai explicitly; this calls the configured model and tests self-created files only.')
EXE=pathlib.Path('/Applications/清空AI.app/Contents/MacOS/KaruoCleaner')
base=pathlib.Path(tempfile.mkdtemp(prefix='.karuo-recovery-check-',dir=pathlib.Path.home())).resolve();os.chmod(base,0o700)
isolated=base/'home';support=isolated/'Library/Application Support/KaruoCleaner';support.mkdir(parents=True)
config=pathlib.Path.home()/'Library/Application Support/KaruoCleaner/ai-config.json';shutil.copyfile(config,support/'ai-config.json');os.chmod(support/'ai-config.json',0o600)
env=dict(os.environ,HOME=str(isolated));fixture=isolated/'fixture';deps=fixture/'project/target/debug/deps';deps.mkdir(parents=True)
artifact=deps/'sample.rlib';credential=deps/'credentials.rlib';personal=fixture/'personal.txt'
for f in [artifact,credential,personal]:
 with f.open('wb') as stream:stream.write(b'own test fixture\n');stream.truncate(1_100_000)
digest=lambda p:hashlib.sha256(p.read_bytes()).hexdigest();expected={str(f):digest(f) for f in [artifact,credential,personal]}
log=(base/'app.log').open('w');app=subprocess.Popen([str(EXE)],env=env,stdout=log,stderr=log)
report={};finished=False

def api(cmd,**kw):
 result=subprocess.run([str(EXE),'--api',json.dumps(dict(cmd=cmd,**kw),ensure_ascii=False)],env=env,capture_output=True,text=True,timeout=80)
 if result.returncode:raise RuntimeError('Isolated app API failed: '+result.stderr.strip())
 return json.loads(result.stdout)
def apply(action):
 preview=api('preview',action=action);assert preview.get('preview_id'),preview
 return api('apply',preview_id=preview['preview_id'],confirmed=True)
def ask(text,module='optimize',**kw):
 v=api('ai_chat',module=module,text=text,chat_request_id='recovery-check-'+str(time.time_ns()),**kw)
 assert v.get('source')=='ai' and not v.get('error'),{k:v.get(k) for k in ['event','source','error','reply']}
 return v
try:
 for _ in range(80):
  if app.poll() is not None:raise RuntimeError('Isolated app exited: '+str(app.returncode))
  if (support/'control.sock').exists():break
  time.sleep(.1)
 else:raise RuntimeError('Isolated app did not open its control socket')
 api('root_set',module='large',path=str(fixture));req=int(time.time()*1000);api('scan',module='large',request_id=req,min_mb=1)
 for _ in range(100):
  scan=api('session_get',module='large')
  if scan.get('request_id')==req and scan.get('data',{}).get('done'):break
  time.sleep(.1)
 else:raise RuntimeError('Exact own fixture scan not finished')
 assert scan['data']['total']==3,scan
 response=ask('只建议可重建的编译产物，凭证和普通资料保留。生成清理清单供我核对，不要执行。',module='large',scan_request_id=req)
 proposal=next(p for p in response.get('proposals',[]) if p.get('kind')=='cleanup')
 plan=api('ai_proposal_plan',proposal_id=proposal['id']);assert plan.get('total')==1 and plan['items'][0]['path']==str(artifact),plan
 report['ai_plan_only_rebuildable_file']=True;print('PASS AI plan excludes credential and personal files',flush=True)
 moved=apply({'cmd':'cleanup_apply','plan_id':plan['plan_id'],'all':True});assert moved.get('count')==1 and not artifact.exists(),moved
 assert credential.exists() and personal.exists();report['own_file_moved_to_trash']=True
 response=ask('请撤回本应用最近一批清理，先生成撤回建议，不直接操作。')
 restore=next(p for p in response.get('proposals',[]) if p.get('kind')=='restore')
 draft=api('ai_proposal_plan',proposal_id=restore['id']);assert draft.get('event')=='ai_restore_proposal' and draft.get('executed') is False and not artifact.exists(),draft
 denied=api('undo_last',proposal_id=restore['id']);assert denied.get('error') and not artifact.exists(),denied
 report['reading_restore_draft_does_not_restore']=True;report['direct_write_rejected']=True
 recovered=apply(draft['action']);assert recovered.get('restored')==1 and digest(artifact)==expected[str(artifact)],recovered
 report['confirmed_ai_restore_matches_bytes']=True;print('PASS AI restore draft is read-only; confirmed recovery restores exact bytes',flush=True)
 moved=apply({'cmd':'cleanup_apply','plan_id':plan['plan_id'],'all':True});assert moved.get('count')==1 and not artifact.exists(),moved
 stale=apply(draft['action']);assert stale.get('error') is True and '批次' in stale.get('text','') and not artifact.exists(),stale
 recovered=apply({'cmd':'undo_last'});assert recovered.get('restored')==1,recovered
 assert all(f.exists() and digest(f)==expected[str(f)] for f in [artifact,credential,personal])
 report['old_ai_button_cannot_restore_new_batch']=True;report['all_fixture_files_restored_unchanged']=True;report['purged']=False;finished=True
 print(json.dumps(report,ensure_ascii=False),flush=True)
 pathlib.Path('/tmp/karuo-build21-ai-recovery.json').write_text(json.dumps(report,ensure_ascii=False,indent=2))
finally:
 app.terminate()
 try:app.wait(timeout=10)
 except subprocess.TimeoutExpired:raise RuntimeError('Own test app did not stop; preserve fixture and investigate exact PID '+str(app.pid))
 log.close()
 (support/"ai-config.json").unlink(missing_ok=True)
 if finished:shutil.rmtree(base)
 else:print('Preserved own fixture for recovery:',base,flush=True)
