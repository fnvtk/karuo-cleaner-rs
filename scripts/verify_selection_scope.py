"""Installed-app read-only plan check. Creates its own fixture; does not apply plans."""
import json,pathlib,subprocess,tempfile,time
EXE='/Applications/卡若清理.app/Contents/MacOS/KaruoCleaner'
def api(cmd,**kw):
 return json.loads(subprocess.check_output([EXE,'--api',json.dumps(dict(cmd=cmd,**kw))],timeout=15))
with tempfile.TemporaryDirectory(prefix='.karuo-selection-check-',dir=pathlib.Path.home()) as fixture:
 root=pathlib.Path(fixture).resolve();folder=root/'A';folder.mkdir()
 for i in range(151):(root/f'file-{i:03}.txt').write_text('test fixture only')
 (folder/'inside.txt').write_text('excluded folder contents')
 api('root_set',module='space',path=str(root));req=int(time.time()*1000);api('space_start',path=str(root),request_id=req)
 until=time.monotonic()+15
 while time.monotonic()<until:
  result=api('session_get',module='space')
  if result.get('request_id')==req and result.get('data',{}).get('done'):break
  time.sleep(.1)
 else:raise RuntimeError('Exact fixture scan did not finish; no substitute evidence')
 assert result['data']['total']==152,result
 excluded=[str(folder),str(root/'file-051.txt')]
 plan=api('space_cleanup_plan',module='space',request_id=req,path=str(root),selection={'query':'','show_hidden':True,'exclude_ids':excluded})
 assert plan['total']==150,plan
 found=[]
 for page in range(plan['page_count']):
  v=api('cleanup_plan_page',plan_id=plan['plan_id'],page=page);found.extend(x['path'] for x in v['items'])
 assert len(found)==150 and str(root/'file-150.txt') in found
 assert str(root/'file-051.txt') not in found and not any(pathlib.Path(p).is_relative_to(folder) for p in found)
 assert all((root/f'file-{i:03}.txt').exists() for i in range(151)) and (folder/'inside.txt').exists()
 print(json.dumps({'fixture_scan_items':result['data']['scanned'],'direct_items':152,'plan_items':150,'pages_verified':plan['page_count'],'excluded_folder_and_file_absent':True,'all_fixture_files_unchanged':True,'applied':False}))
