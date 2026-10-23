import json,pathlib,plistlib,socket,time,uuid
home=pathlib.Path.home();tag=uuid.uuid4().hex;bid='com.karuo.fixture.'+tag
app=home/'Applications'/('KaruoUninstallFixture-'+tag+'.app');(app/'Contents').mkdir(parents=True)
with open(app/'Contents/Info.plist','wb')as f:plistlib.dump({'CFBundleIdentifier':bid,'CFBundleName':'Owned fixture','CFBundleExecutable':'OwnedFixture'},f)
(app/'Contents/fixture.bin').write_bytes(b'ownapp')
cache=home/'Library/Caches'/bid;cache.mkdir();(cache/'disposable.cache').write_bytes(b'own cache')
(cache/'credential.key').write_bytes(b'owned secret protected fixture')
prefs=home/'Library/Preferences'/(bid+'.plist');prefs.write_bytes(b'owned prefs retained')
appdata=home/'Library/Application Support'/bid;appdata.mkdir();(appdata/'project.txt').write_text('owned important fixture')
def ask(d):
 s=socket.socket(socket.AF_UNIX);s.settimeout(75);s.connect(str(home/'Library/Application Support/KaruoCleaner/control.sock'));s.sendall(json.dumps(d).encode()+b'\n');out=b''
 while b'\n' not in out:out+=s.recv(65536)
 s.close();return json.loads(out)
print('scan queued',ask({'cmd':'scan','module':'uninstall'}),flush=True)
for _ in range(600):
 r=ask({'cmd':'session_get','module':'uninstall'})
 if not r.get('scanning') and r.get('data',{}).get('done'):break
 time.sleep(.2)
else:raise RuntimeError('application scan timeout')
r=ask({'cmd':'scan_page','module':'uninstall','category':'application','query':tag,'page':0});assert len(r['data']['entries'])==1,r
p=ask({'cmd':'uninstall_plan','ids':[str(app)]});assert p.get('plan_id'),p
assert not any('credential.key' in x['path']or str(appdata)in x['path']for x in p['items'])
assert not next(x for x in p['items']if x['path']==str(prefs))['selected']
plan=p['plan_id'];chosen=[x['id']for x in p['items']]  # 夹具显式选中偏好，验证恢复范围
# 未AI审阅，即使API确认凭证有效也不能执行。
token=ask({'cmd':'preview','action':{'cmd':'uninstall_apply','plan_id':plan,'ids':chosen}})['preview_id']
blocked=ask({'cmd':'apply','preview_id':token,'confirmed':True});assert blocked.get('error')or blocked.get('event')=='message',blocked;assert app.exists()
ai=ask({'cmd':'uninstall_analyze','plan_id':plan});assert ai.get('source')=='ai'and ai.get('reviewed'),ai
print('AI review source',ai['source'],'usage',ai.get('usage'),flush=True)
token=ask({'cmd':'preview','action':{'cmd':'uninstall_apply','plan_id':plan,'ids':chosen}})['preview_id']
r=ask({'cmd':'apply','preview_id':token,'confirmed':True});assert r.get('count')==3,r
assert not app.exists()and not(cache/'disposable.cache').exists()
assert not prefs.exists()and(cache/'credential.key').exists()and(appdata/'project.txt').exists()
owned=[x for x in ask({'cmd':'history'})['records']if x['original_path']in[str(app),str(cache/'disposable.cache'),str(prefs)]];assert len(owned)==3
p=ask({'cmd':'preview','action':{'cmd':'restore','ids':[x['id']for x in owned]}})
ask({'cmd':'apply','preview_id':p['preview_id'],'confirmed':True})
assert app.exists()and(cache/'disposable.cache').exists()and prefs.exists()
report={'fixture_app':str(app),'bundle_id':bid,'ai_source':ai['source'],'usage':ai.get('usage'),'uninstall_move_count':3,'restored_count':3,'protected_credentials_retained':True,'app_support_retained':True,'preferences_default_unselected':True,'explicit_preferences_move_and_restore':True,'AI_required':True}
pathlib.Path('开发文档/卸载1.4夹具验收.json').write_text(json.dumps(report,ensure_ascii=False,indent=2));print(json.dumps(report,ensure_ascii=False))
# 只清除本脚本刚创建、已验证恢复的夹具。
import shutil
shutil.rmtree(app);shutil.rmtree(cache);prefs.unlink();shutil.rmtree(appdata)
