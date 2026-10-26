"""Owned read-only fixtures: prove real target values reach the model and plans keep boundaries."""
import json,os,pathlib,subprocess,tempfile,threading,time,shutil,http.server
exe=pathlib.Path(os.environ.get('BIFANG_TEST_BINARY','dist/轻理AI.app/Contents/MacOS/KaruoCleaner')).resolve()
base=pathlib.Path(tempfile.mkdtemp(prefix='.bifang-goal-qa-',dir=pathlib.Path.home())).resolve();support=base/'Library/Application Support/KaruoCleaner';support.mkdir(parents=True)
received=[]
class Server(http.server.BaseHTTPRequestHandler):
 def log_message(self,*a):pass
 def do_POST(self):
  data=json.loads(self.rfile.read(int(self.headers['Content-Length'])));user=json.loads(data['messages'][-1]['content']);received.append(user)
  percent=user['space_goal']['percent'];answer={'reply':f'按{percent}%目标，先整理已验证缓存；不足部分保留资料并复核下载缓存。','action':'cleanup','scope':'recommended'}
  body=json.dumps({'choices':[{'message':{'content':json.dumps(answer,ensure_ascii=False)}}],'usage':{'total_tokens':123}}).encode();self.send_response(200);self.end_headers();self.wfile.write(body)
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Server);threading.Thread(target=server.serve_forever,daemon=True).start()
config={'base_url':f'http://127.0.0.1:{server.server_port}/v1','model':'owned-goal-test','api_key':'owned-test-only'};(support/'ai-config.json').write_text(json.dumps(config));os.chmod(support/'ai-config.json',0o600)
files=[]
for relative,age in [('Library/Caches/owned/old.cache',12),('Library/Caches/owned/recent.cache',2),('Library/Caches/owned/credentials.key',45),('.npm/_npx/oldpack/node_modules/package/cache.js',45),('.npm/_npx/newpack/node_modules/package/recent.js',45),('project/node_modules/package/keep.js',45)]:
 p=base/relative;p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(b'owned goal fixture\n'*100);at=time.time()-age*86400;os.utime(p,(at,at));files.append(p)
at=time.time()-45*86400;os.utime(base/'.npm/_npx/oldpack',(at,at))
env=dict(os.environ,HOME=str(base));process=subprocess.Popen([str(exe)],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL);ok=False

def api(cmd,**kw):return json.loads(subprocess.check_output([str(exe),'--api',json.dumps(dict(cmd=cmd,**kw))],env=env,timeout=80))
try:
 for _ in range(100):
  if (support/'control.sock').exists():break
  time.sleep(.1)
 for percent in [10,20,30]:
  assert api('space_goal_save',percent=percent)['percent']==percent
  assert api('goal_analyze')['queued']
  for _ in range(300):
   result=api('goal_analysis_status')
   if result.get('percent')==percent and result.get('phase') in ['done','failed']:break
   time.sleep(.05)
  assert result['phase']=='done' and result['source']=='ai',result
  assert result['safe_bytes']==files[0].stat().st_size,result
  assert result['review_bytes']==files[3].stat().st_size,result
  assert result['target']['percent']==percent and result['executed'] is False,result
  assert str(percent)+'%' in result['reply']
  assert all(p.exists() for p in files)
  context=received[-1];assert context['space_goal']['percent']==percent
  serialized=json.dumps(context);assert str(base) not in serialized and 'credentials.key' not in serialized and 'cache.js' not in serialized
 assert len(received)==3
 print(json.dumps({'passed':True,'target_percentages_sent_to_model':[x['space_goal']['percent'] for x in received],'old_npx_package_in_manual_review':True,'recent_npx_and_project_node_modules_excluded':True,'all_6_files_unchanged':True,'executed':False}));ok=True
finally:
 process.terminate();process.wait(timeout=10);server.shutdown();(support/'ai-config.json').unlink(missing_ok=True)
 if ok:shutil.rmtree(base)
 else:print('Preserved owned fixture:',base)
