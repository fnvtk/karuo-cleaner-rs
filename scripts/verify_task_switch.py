"""Read-only cross-module task switching in the real application; no file mutation commands."""
import json,os,subprocess,sys,tempfile,time
from pathlib import Path
exe=str(Path(sys.argv[1] if len(sys.argv)>1 else 'dist/轻理AI.app/Contents/MacOS/KaruoCleaner').resolve())
scan_root=Path.home()
with tempfile.TemporaryDirectory(prefix='cm-switch-',dir='/private/tmp') as raw:
 root=Path(raw);env=dict(os.environ,HOME=str(root));p=subprocess.Popen([exe],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 def api(request):
  r=subprocess.run([exe,'--api','-'],input=json.dumps(request),env=env,capture_output=True,text=True,timeout=30);r.check_returncode();v=json.loads(r.stdout);assert not v.get('error'),v;return v
 try:
  for _ in range(100):
   if (root/'Library/Application Support/KaruoCleaner/control.sock').exists():break
   time.sleep(.05)
  api({'cmd':'root_set','module':'large','path':str(scan_root)})
  api({'cmd':'scan','module':'large','min_mb':100,'request_id':851})
  for _ in range(300):
   old=api({'cmd':'session_get','module':'large'})
   if old['data']['scanned']>0:break
   time.sleep(.02)
  active=api({'cmd':'task_status'})['active'];assert active and active['module']=='large',active
  job=active['job_id']
  api({'cmd':'optimize'});api({'cmd':'startup_get'})
  assert api({'cmd':'task_status'})['active']['job_id']==job
  api({'cmd':'scan','module':'junk','request_id':852})
  status=api({'cmd':'task_status'});pending=status['pending'];assert pending and status['active']['job_id']==job
  api({'cmd':'task_switch','switch_id':pending['switch_id'],'confirm':False})
  status=api({'cmd':'task_status'});assert status['active']['job_id']==job and status['pending'] is None
  api({'cmd':'scan','module':'junk','request_id':853})
  pending=api({'cmd':'task_status'})['pending'];assert pending
  api({'cmd':'task_switch','switch_id':pending['switch_id'],'confirm':True})
  for _ in range(500):
   status=api({'cmd':'task_status'});new=api({'cmd':'session_get','module':'junk'})
   if new['request_id']==853 and new['data']['done'] and status['active'] is None:break
   time.sleep(.02)
  old=api({'cmd':'session_get','module':'large'})
  assert old['request_id']==851 and old['data']['outcome']=='cancelled' and old['data']['done'],old['data']['outcome']
  assert old['data']['scanned']>0 and new['request_id']==853 and new['data']['done']
  assert api({'cmd':'history'})['records']==[]
  disk=json.loads((root/'Library/Application Support/KaruoCleaner/scan-sessions.json').read_text())
  assert 'large' in disk and 'junk' in disk
  evidence={'old_job':job,'old_request':851,'new_request':853,'old_outcome':old['data']['outcome'],'old_scanned':old['data']['scanned'],'old_results_preserved':old['data']['total'],'new_done':new['data']['done'],'declined_switch_preserved_job':True,'management_pages_read_during_scan':True,'both_sessions_saved':True,'file_mutations':0}
  out=Path('release-assets/acceptance/task-switch.json');out.parent.mkdir(parents=True,exist_ok=True);out.write_text(json.dumps(evidence,indent=2));print('PASS',json.dumps(evidence),flush=True)
 finally:p.terminate();p.wait(10)
