"""Read-only management AI check using the user's configured provider in isolated app state."""
import json, os, plistlib, shutil, subprocess, sys, tempfile, time
from pathlib import Path
exe=str(Path(sys.argv[1] if len(sys.argv)>1 else 'dist/轻理AI.app/Contents/MacOS/KaruoCleaner').resolve())
config=Path.home()/'Library/Application Support/KaruoCleaner/ai-config.json'
with tempfile.TemporaryDirectory(prefix='cm-live-',dir='/private/tmp') as raw:
 root=Path(raw);support=root/'Library/Application Support/KaruoCleaner';support.mkdir(parents=True)
 shutil.copyfile(config,support/'ai-config.json');os.chmod(support/'ai-config.json',0o600)
 agent=root/'Library/LaunchAgents/ai.cleanmac.acceptance.plist';agent.parent.mkdir(parents=True)
 agent.write_bytes(plistlib.dumps({'Label':'ai.cleanmac.acceptance','ProgramArguments':['/usr/bin/true'],'RunAtLoad':True}))
 env=dict(os.environ,HOME=str(root));process=subprocess.Popen([exe],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 def api(request):
  result=subprocess.run([exe,'--api','-'],input=json.dumps(request),env=env,text=True,capture_output=True,timeout=75);result.check_returncode();return json.loads(result.stdout)
 try:
  for _ in range(100):
   if (support/'control.sock').exists():break
   if process.poll() is not None:raise AssertionError('Application exited')
   time.sleep(.05)
  for _ in range(20):
   apps=api({'cmd':'optimize'})
   if apps.get('apps'):break
   time.sleep(.25)
  assert apps.get('apps'),'No running application snapshot available for this scenario'
  results=[]
  for subpage in ('running','startup'):
   result=api({'cmd':'ai_chat','module':'optimize','subpage':subpage,'text':'只分析当前子页，给一句建议，不执行操作。','chat_request_id':'live-'+subpage,'ai_billing':'byok'})
   record={key:result.get(key) for key in ('module','subpage','context_count','source','reply','error','usage')}
   assert record['source']=='ai' and record['context_count']>0,record
   assert not any(text in record['reply'] for text in ('没有检测到','没有显示正在运行','登录项为空')),record
   results.append(record);print(json.dumps(record,ensure_ascii=False),flush=True)
  out=Path('release-assets/acceptance/real-management-ai.json');out.parent.mkdir(parents=True,exist_ok=True);out.write_text(json.dumps(results,ensure_ascii=False,indent=2))
 finally:
  process.terminate();process.wait(10)
