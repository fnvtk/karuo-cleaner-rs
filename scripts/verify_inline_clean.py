"""Verify inline cleanup using owned fixtures and isolated HOME; never purge Trash."""
import argparse,json,os,pathlib,shutil,subprocess,tempfile,time
parser=argparse.ArgumentParser();parser.add_argument('--live-ai',action='store_true');args=parser.parse_args()
exe=pathlib.Path(os.environ.get('BIFANG_TEST_BINARY','/Applications/轻理AI.app/Contents/MacOS/KaruoCleaner'))
base=pathlib.Path(tempfile.mkdtemp(prefix='.bifang-inline-check-',dir=pathlib.Path.home())).resolve();support=base/'Library/Application Support/KaruoCleaner';support.mkdir(parents=True)
if args.live_ai:
 shutil.copyfile(pathlib.Path.home()/'Library/Application Support/KaruoCleaner/ai-config.json',support/'ai-config.json');os.chmod(support/'ai-config.json',0o600)
files=[]
for relative,age in [('Library/Caches/fixture/old.cache',12),('Library/Logs/fixture/old.log',45),('Library/Caches/fixture/recent.cache',2),('Library/Caches/fixture/credentials.key',12),('.npm/_cacache/content-v2/developer',12)]:
 p=base/relative;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(b'owned inline fixture\n'*32768);at=time.time()-age*86400;os.utime(p,(at,at));files.append(p)
env=dict(os.environ,HOME=str(base));process=subprocess.Popen([str(exe)],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL);success=False

def api(cmd,**kw):return json.loads(subprocess.check_output([str(exe),'--api',json.dumps(dict(cmd=cmd,**kw))],env=env,timeout=80))
def apply(action):
 preview=api('preview',action=action);assert preview.get('preview_id'),preview
 return api('apply',preview_id=preview['preview_id'],confirmed=True)
try:
 for _ in range(100):
  if (support/'control.sock').exists():break
  time.sleep(.1)
 assert api('space_goal_save',percent=20).get('percent')==20
 assert api('space_goal_get').get('percent')==20
 assert api('space_goal_save',percent=40).get('error')
 denied=api('smart_clean');assert denied.get('error') and denied.get('executed') is False,denied
 assert apply({'cmd':'smart_clean'}).get('queued') is True
 phases=set()
 for _ in range(600):
  result=api('smart_clean_status');phases.add(result.get('phase'))
  if result.get('phase') in ['done','failed']:break
  time.sleep(.1)
 assert result.get('phase')=='done' and result.get('moved_count')==2,result
 assert result['available_after']-result['available_before']==result['available_delta']
 assert not files[0].exists() and not files[1].exists();assert all(p.exists() for p in files[2:])
 if args.live_ai:assert result.get('ai_source')=='ai',{'ai_source':result.get('ai_source'),'advice':result.get('ai_advice')}
 assert result.get('target_percent')==20 and result.get('target_shortfall') is not None,result
 preview=api('purge_preview',ids=result['record_ids'],smart_flow=True);assert preview.get('count')==2 and preview.get('token'),preview
 assert (support/'smart-clean-last.json').exists()
 process.terminate();process.wait(timeout=10);process=subprocess.Popen([str(exe)],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 for _ in range(100):
  try:
   recovered_status=api('smart_clean_status')
   if recovered_status.get('phase')=='done':break
  except subprocess.CalledProcessError:pass
  time.sleep(.1)
 assert recovered_status.get('record_ids')==result.get('record_ids'),recovered_status
 apply({'cmd':'restore','ids':result['record_ids']})
 assert all(p.read_bytes()==b'owned inline fixture\n'*32768 for p in files)
 for _ in range(100):
  status=api('smart_clean_status')
  if status.get('phase')=='restored':break
  time.sleep(.1)
 assert status.get('phase')=='restored' and status.get('record_ids')==[],status
 print(json.dumps({'passed':True,'phases_observed':sorted(phases),'moved_count':result['moved_count'],'moved_bytes':result['moved_bytes'],'ai_source':result.get('ai_source'),'disk_delta_measured':True,'all_5_fixtures_restored':True,'completion_restored_after_restart':True,'purged':False},ensure_ascii=False),flush=True);success=True
finally:
 process.terminate();process.wait(timeout=10);(support/'ai-config.json').unlink(missing_ok=True)
 if success:shutil.rmtree(base)
 else:print('Preserved owned fixture:',base,flush=True)
