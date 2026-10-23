import json,pathlib,socket,time,uuid
home=pathlib.Path.home();fixture=home/'Library/Caches'/('KaruoChat-'+uuid.uuid4().hex);build=fixture/'target/debug/deps';build.mkdir(parents=True)
for i in range(7):(build/f'owned-{i}.rlib').write_bytes(b'owned rebuildable fixture')
(build/'keep-project.txt').write_text('owned ordinary data preserve');(build/'credential.key').write_text('owned secret preserve')
def ask(d):
 s=socket.socket(socket.AF_UNIX);s.settimeout(75);s.connect(str(home/'Library/Application Support/KaruoCleaner/control.sock'));s.sendall(json.dumps(d).encode()+b'\n');out=b''
 while b'\n' not in out:out+=s.recv(65536)
 s.close();return json.loads(out)
assert ask({'cmd':'root_set','module':'space','path':str(fixture)})['read_only']
ask({'cmd':'space_start','path':str(fixture)})
for _ in range(200):
 r=ask({'cmd':'session_get','module':'space'})
 if r.get('data',{}).get('done'):break
 time.sleep(.05)
else:raise RuntimeError('space scan timeout')
ai=ask({'cmd':'ai_chat','module':'space','text':'帮我整理当前目录中可重建的编译产物，只为本机推荐项生成待确认清理清单，个人资料和凭证保留。','history':[]})
assert ai.get('source')=='ai',ai
print('AI chat',ai.get('reply'),ai.get('usage'),flush=True)
prop=next((p for p in ai.get('proposals',[])if p['kind']=='cleanup'),None);assert prop,ai
p=ask({'cmd':'ai_proposal_plan','proposal_id':prop['id']});assert p.get('total')==7,p
assert all(x['path'].endswith('.rlib')for x in p['items'])
assert ask({'cmd':'cleanup_apply','plan_id':p['plan_id'],'all':True}).get('executed')is False
preview=ask({'cmd':'preview','action':{'cmd':'cleanup_apply','plan_id':p['plan_id'],'all':True}})
r=ask({'cmd':'apply','preview_id':preview['preview_id'],'confirmed':True});assert r.get('count')==7,r
assert not list(build.glob('*.rlib')) and (build/'keep-project.txt').exists()and(build/'credential.key').exists()
preview=ask({'cmd':'preview','action':{'cmd':'undo_last'}})
r=ask({'cmd':'apply','preview_id':preview['preview_id'],'confirmed':True});assert r.get('restored')==7,r
assert len(list(build.glob('*.rlib')))==7
# 撤回后在空间索引重新出现，无需重新扫描。
ask({'cmd':'space_browse','path':str(build),'show_hidden':True})
r=ask({'cmd':'session_get','module':'space'});assert r['data']['total']==9,r
schedule=ask({'cmd':'ai_chat','module':'space','text':'帮我安排每周10点只扫描，不自动删除。请给我定时设置草案。','history':[]});assert schedule.get('source')=='ai',schedule
proposal=next((p for p in schedule.get('proposals',[])if p['kind']=='schedule'),None);assert proposal,schedule
s=ask({'cmd':'ai_proposal_plan','proposal_id':proposal['id']});assert s['config']['mode']=='scan'and s['config']['frequency']=='weekly',s
report={'fixture':str(fixture),'ai_chat_usage':ai.get('usage'),'schedule_chat_usage':schedule.get('usage'),'recommended_artifacts':7,'ordinary_and_credentials_retained':True,'move_count':7,'undo_count':7,'restored_space_view_count':9,'schedule_draft':s['config'],'schedule_not_saved':True}
pathlib.Path('开发文档/AI对话与空间撤回验收.json').write_text(json.dumps(report,ensure_ascii=False,indent=2));print(json.dumps(report,ensure_ascii=False))
