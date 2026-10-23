"""仅新建专属测试文件；不清理任何已有用户数据。"""
import json,socket,pathlib,time,uuid
support=pathlib.Path.home()/'Library/Application Support/KaruoCleaner'
fixture=pathlib.Path.home()/'Library/Caches'/('KaruoAPI-'+uuid.uuid4().hex)
fixture.mkdir(); fixture=fixture.resolve()
def ask(d):
 s=socket.socket(socket.AF_UNIX);s.settimeout(75);s.connect(str(support/'control.sock'));s.sendall(json.dumps(d).encode()+b'\n');out=b''
 while b'\n' not in out:
  b=s.recv(65536)
  if not b:raise RuntimeError('closed')
  out+=b
 s.close();return json.loads(out)
def wait(module):
 for _ in range(200):
  r=ask({'cmd':'session_get','module':module})
  if not r.get('scanning') and r.get('data',{}).get('done'):return r
  time.sleep(.05)
 raise RuntimeError('fixture scan failed to complete')
for i in range(5101):(fixture/f'tiny-{i:05}.txt').write_text('owned')
for i in range(56):
 with open(fixture/f'large-{i:02}.bin','wb')as f:f.truncate(1000001)
report={'fixture':str(fixture)}
assert ask({'cmd':'root_set','module':'space','path':str(fixture)})['read_only']
assert ask({'cmd':'root_set','module':'large','path':str(fixture)})['read_only']
assert ask({'cmd':'space_start','path':str(fixture)})['queued']
assert ask({'cmd':'scan','module':'large','min_mb':1})['queued']
a=wait('space');b=wait('large')
assert a['data']['total']==5157,a
assert b['data']['total']==56,b
assert len(a['data']['entries'])==50
page=ask({'cmd':'scan_page','module':'large','category':'file','query':'large-','page':1})
assert page['data']['filtered_total']==56 and len(page['data']['entries'])==6
report.update(space_total=a['data']['total'],large_total=b['data']['total'],page_two_count=len(page['data']['entries']),concurrent_modules=True)
# 所有筛选结果的选择必须跨越当前50项显示页。
action={'cmd':'move','module':'large','selection':{'category':'file','query':'large-'}}
assert ask(action).get('executed')is False
preview=ask({'cmd':'preview','action':action});token=preview['preview_id']
r=ask({'cmd':'apply','preview_id':token,'confirmed':True});assert r.get('count')==56,r
assert ask({'cmd':'apply','preview_id':token,'confirmed':True}).get('executed')is False
assert ask({'cmd':'session_get','module':'large'})['data']['total']==0
assert ask({'cmd':'session_get','module':'space'})['data']['total']==5101
report.update(filtered_move_count=r['count'],cross_module_remove=True,one_time_preview=True)
history=ask({'cmd':'history'})['records'];owned=[x for x in history if pathlib.Path(x['original_path']).parent==fixture]
assert len(owned)==56
restore={'cmd':'restore','ids':[x['id']for x in owned]};token=ask({'cmd':'preview','action':restore})['preview_id'];r=ask({'cmd':'apply','preview_id':token,'confirmed':True})
# API可能先回message；以实际文件回读为准。
assert all((fixture/f'large-{i:02}.bin').exists() for i in range(56)),r
report['restored_count']=56
pathlib.Path('开发文档/API1.4夹具验收.json').write_text(json.dumps(report,ensure_ascii=False,indent=2));print(json.dumps(report,ensure_ascii=False))
# 保留小型fixture用于重启结果读取/界面截图，验收完再移走，避免污染用户数据。
